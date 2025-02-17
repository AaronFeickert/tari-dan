// Copyright 2024 The Tari Project
// SPDX-License-Identifier: BSD-3-Clause

use log::*;
use minotari_app_grpc::tari_rpc::RegisterValidatorNodeResponse;
use tokio::{
    process::Child,
    sync::mpsc,
    time::{sleep, Duration},
};

use crate::{
    alerting::{Alerting, MatterMostNotifier, TelegramNotifier},
    config::Channels,
};

#[derive(Copy, Clone, Debug)]
pub struct Transaction {
    id: u64,
    block: u64,
}

impl Transaction {
    pub fn new(response: RegisterValidatorNodeResponse, block: u64) -> Self {
        Self {
            id: response.transaction_id,
            block,
        }
    }
}

#[derive(Clone, Debug)]
pub enum ProcessStatus {
    Running,
    Exited(i32), // status code
    Crashed,
    InternalError(String),
    Submitted(Transaction),
}

pub async fn monitor_child(
    mut child: Child,
    tx_logging: mpsc::Sender<ProcessStatus>,
    tx_alerting: mpsc::Sender<ProcessStatus>,
    tx_restart: mpsc::Sender<()>,
) {
    // process is still running
    tx_logging
        .send(ProcessStatus::Running)
        .await
        .expect("Failed to send process running status to logging");
    tx_alerting
        .send(ProcessStatus::Running)
        .await
        .expect("Failed to send process running status to alerting");
    let exit = child.wait().await;

    match exit {
        Ok(status) => {
            if status.success() {
                info!("Child process exited with status: {}", status);
                tx_logging
                    .send(ProcessStatus::Exited(status.code().unwrap_or(0)))
                    .await
                    .expect("Failed to send process exit status to logging");
                tx_alerting
                    .send(ProcessStatus::Exited(status.code().unwrap_or(0)))
                    .await
                    .expect("Failed to send process exit status to alerting");
                tx_restart.send(()).await.expect("Failed to send restart node signal");
            } else {
                warn!("Child process CRASHED with status: {}", status);
                tx_logging
                    .send(ProcessStatus::Crashed)
                    .await
                    .expect("Failed to send status to logging");
                tx_alerting
                    .send(ProcessStatus::Crashed)
                    .await
                    .expect("Failed to send status to alerting");
                tx_restart.send(()).await.expect("Failed to send restart node signal");
            }
        },
        // if the child process encountered an unexpected error, not related to the process itself
        Err(err) => {
            error!("Child process encountered an error: {}", err);
            let err_msg = err.to_string();
            tx_logging
                .send(ProcessStatus::InternalError(err_msg.clone()))
                .await
                .expect("Failed to send internal error status to logging");
            tx_alerting
                .send(ProcessStatus::InternalError(err_msg))
                .await
                .expect("Failed to send internal error status to alerting");
            tx_restart.send(()).await.expect("Failed to send restart node signal");
        },
    }
}

pub async fn process_status_log(mut rx: mpsc::Receiver<ProcessStatus>) {
    loop {
        if let Some(status) = rx.recv().await {
            match status {
                ProcessStatus::Exited(code) => {
                    error!("Validator node process exited with code {}", code);
                    info!("Pauses process logging for 5 seconds to allow the validator node to restart");
                    sleep(Duration::from_secs(5)).await;
                },
                ProcessStatus::InternalError(err) => {
                    error!("Validator node process exited with error: {}", err);
                    info!("Pausing process logging 5 seconds to allow the validator node to restart");
                    sleep(Duration::from_secs(5)).await;
                },
                ProcessStatus::Crashed => {
                    error!("Validator node process crashed");
                    info!("Pausing process logging for 5 seconds to allow the validator node to restart");
                    sleep(Duration::from_secs(5)).await;
                },
                ProcessStatus::Running => {
                    // all good, process is still running
                },
                ProcessStatus::Submitted(tx) => {
                    info!(
                        "Validator node registration submitted (tx: {}, block: {})",
                        tx.id, tx.block
                    );
                },
            }
        }
    }
}

fn setup_alerting_clients(cfg: Channels) -> (Option<MatterMostNotifier>, Option<TelegramNotifier>) {
    let mut mattermost: Option<MatterMostNotifier> = None;
    if cfg.mattermost.enabled {
        let cfg = cfg.mattermost.clone();
        info!("Mattermost alerting enabled");
        mattermost = Some(MatterMostNotifier {
            server_url: cfg.server_url,
            channel_id: cfg.channel_id,
            credentials: cfg.credentials,
            alerts_sent: 0,
            client: reqwest::Client::new(),
        });
    } else {
        info!("Mattermost alerting disabled");
    }

    let mut telegram: Option<TelegramNotifier> = None;
    if cfg.telegram.enabled {
        let cfg = cfg.telegram.clone();
        info!("Telegram alerting enabled");
        telegram = Some(TelegramNotifier {
            bot_token: cfg.credentials,
            chat_id: cfg.channel_id,
            alerts_sent: 0,
            client: reqwest::Client::new(),
        });
    } else {
        info!("Telegram alerting disabled");
    }

    (mattermost, telegram)
}

pub async fn process_status_alert(mut rx: mpsc::Receiver<ProcessStatus>, cfg: Channels) {
    let (mut mattermost, mut telegram) = setup_alerting_clients(cfg);

    loop {
        while let Some(status) = rx.recv().await {
            match status {
                ProcessStatus::Exited(code) => {
                    if let Some(mm) = &mut mattermost {
                        mm.alert(&format!("Validator node process exited with code {}", code))
                            .await
                            .expect("Failed to send alert to MatterMost");
                    }
                    if let Some(tg) = &mut telegram {
                        tg.alert(&format!("Validator node process exited with code {}", code))
                            .await
                            .expect("Failed to send alert to Telegram");
                    }
                },
                ProcessStatus::InternalError(err) => {
                    if let Some(mm) = &mut mattermost {
                        mm.alert(&format!("Validator node process internal error: {}", err))
                            .await
                            .expect("Failed to send alert to MatterMost");
                    }
                    if let Some(tg) = &mut telegram {
                        tg.alert(&format!("Validator node process internal error: {}", err))
                            .await
                            .expect("Failed to send alert to Telegram");
                    }
                },
                ProcessStatus::Crashed => {
                    if let Some(mm) = &mut mattermost {
                        mm.alert("Validator node process crashed")
                            .await
                            .expect("Failed to send alert to MatterMost");
                    }
                    if let Some(tg) = &mut telegram {
                        tg.alert("Validator node process crashed")
                            .await
                            .expect("Failed to send alert to Telegram");
                    }
                },
                ProcessStatus::Running => {
                    // all good, process is still running, send heartbeat to channel(s)
                    if let Some(mm) = &mut mattermost {
                        if mm.ping().await.is_err() {
                            warn!("Failed to send heartbeat to MatterMost");
                        }
                    }
                    if let Some(tg) = &mut telegram {
                        if tg.ping().await.is_err() {
                            warn!("Failed to send heartbeat to Telegram");
                        }
                    }
                },
                ProcessStatus::Submitted(tx) => {
                    if let Some(mm) = &mut mattermost {
                        mm.alert(&format!(
                            "Validator node registration submitted (tx: {}, block: {})",
                            tx.id, tx.block
                        ))
                        .await
                        .expect("Failed to send alert to MatterMost");
                    }
                    if let Some(tg) = &mut telegram {
                        tg.alert(&format!(
                            "Validator node registration submitted (tx: {}, block: {})",
                            tx.id, tx.block
                        ))
                        .await
                        .expect("Failed to send alert to Telegram");
                    }
                },
            }
        }
    }
}

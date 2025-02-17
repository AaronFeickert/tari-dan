//   Copyright 2023 The Tari Project
//   SPDX-License-Identifier: BSD-3-Clause

use log::*;
use tari_dan_common_types::{committee::CommitteeInfo, optional::Optional, Epoch};
use tari_dan_storage::{
    consensus_models::{Block, LastProposed, LastSentVote, LeafBlock},
    StateStore,
};
use tokio::task;

use crate::{
    hotstuff::HotStuffError,
    messages::{HotstuffMessage, ProposalMessage, SyncRequestMessage},
    traits::{ConsensusSpec, OutboundMessaging},
};

const LOG_TARGET: &str = "tari::dan::consensus::hotstuff::on_sync_request";

#[derive(Debug)]
pub struct OnSyncRequest<TConsensusSpec: ConsensusSpec> {
    store: TConsensusSpec::StateStore,
    outbound_messaging: TConsensusSpec::OutboundMessaging,
}

impl<TConsensusSpec: ConsensusSpec> OnSyncRequest<TConsensusSpec> {
    pub fn new(store: TConsensusSpec::StateStore, outbound_messaging: TConsensusSpec::OutboundMessaging) -> Self {
        Self {
            store,
            outbound_messaging,
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn handle(
        &self,
        from: TConsensusSpec::Addr,
        local_committee_info: CommitteeInfo,
        epoch: Epoch,
        msg: SyncRequestMessage,
    ) {
        if msg.high_qc.epoch() != epoch {
            warn!(
                target: LOG_TARGET,
                "Received SyncRequest from {} for epoch {} but our epoch is {}. Ignoring request.",
                from,
                msg.high_qc.epoch(),
                epoch
            );
            return;
        }

        let mut outbound_messaging = self.outbound_messaging.clone();
        let store = self.store.clone();

        task::spawn(async move {
            let result = store.with_read_tx(|tx| {
                let mut leaf_block = LeafBlock::get(tx, epoch)?;
                if let Some(last_proposed) = LastProposed::get(tx).optional()? {
                    if last_proposed.height > leaf_block.height() {
                        leaf_block = last_proposed.as_leaf_block();
                    }
                }

                if leaf_block.height.is_zero() {
                    info!(target: LOG_TARGET, "This node is at height 0 so cannot return any syn blocks. Ignoring request");
                    return Ok(vec![]);
                }

                if leaf_block.height() < msg.high_qc.block_height() {
                    return Err(HotStuffError::InvalidSyncRequest {
                        details: format!(
                            "Received catch up request from {} for block {} but our leaf block is {}. Ignoring \
                             request.",
                            from, msg.high_qc, leaf_block
                        ),
                    });
                }

                info!(
                    target: LOG_TARGET,
                    "🌐 Received catch up request from {} from block {} to {}",
                    from,
                    msg.high_qc,
                    leaf_block
                );
                // NOTE: We have to send dummy blocks, because the messaging will ignore heights > current_view + 1,
                // until eventually the syncing node's pacemaker leader-fails a few times.
                let blocks = Block::get_all_blocks_between(
                    tx,
                    leaf_block.epoch(),
                    local_committee_info.shard_group(),
                    msg.high_qc.block_height(),
                    leaf_block.height(),
                    true,
                    1000,
                )?;

                Ok::<_, HotStuffError>(blocks)
            });

            let blocks = match result {
                Ok(mut blocks) => {
                    if let Some(pos) = blocks.iter().position(|b| b.is_genesis()) {
                        blocks.remove(pos);
                    }
                    blocks
                },
                Err(err) => {
                    warn!(target: LOG_TARGET, "Failed to fetch blocks for sync request: {}", err);
                    return;
                },
            };

            info!(
                target: LOG_TARGET,
                "🌐 Sending {} block(s) ({} to {}) to {}",
                blocks.len(),
                blocks.first().map(|b| b.height()).unwrap_or_default(),
                blocks.last().map(|b| b.height()).unwrap_or_default(),
                from
            );

            for block in blocks {
                info!(
                    target: LOG_TARGET,
                    "🌐 Sending block {} to {}",
                    block,
                    from
                );
                // TODO(perf): O(n) queries
                let foreign_proposals = match store.with_read_tx(|tx| block.get_foreign_proposals(tx)) {
                    Ok(foreign_proposals) => foreign_proposals,
                    Err(err) => {
                        warn!(target: LOG_TARGET, "Failed to fetch foreign proposals for block {}: {}", block, err);
                        return;
                    },
                };

                if let Err(err) = outbound_messaging
                    .send(
                        from.clone(),
                        HotstuffMessage::Proposal(ProposalMessage {
                            block,
                            foreign_proposals,
                        }),
                    )
                    .await
                {
                    warn!(target: LOG_TARGET, "Error sending SyncResponse: {err}");
                    return;
                }
            }

            // Send last vote.
            let maybe_last_vote = match store.with_read_tx(|tx| LastSentVote::get(tx)).optional() {
                Ok(last_vote) => last_vote,
                Err(err) => {
                    warn!(target: LOG_TARGET, "Failed to fetch last vote for catch-up request: {}", err);
                    return;
                },
            };
            if let Some(last_vote) = maybe_last_vote {
                if let Err(err) = outbound_messaging
                    .send(from.clone(), HotstuffMessage::Vote(last_vote.into()))
                    .await
                {
                    warn!(target: LOG_TARGET, "Failed to send LastVote {err}");
                }
            }
        });
    }
}

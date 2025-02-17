//   Copyright 2023 The Tari Project
//   SPDX-License-Identifier: BSD-3-Clause
//  Copyright 2023. The Tari Project
//
//  Redistribution and use in source and binary forms, with or without modification, are permitted provided that the
//  following conditions are met:
//
//  1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following
//  disclaimer.
//
//  2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the
//  following disclaimer in the documentation and/or other materials provided with the distribution.
//
//  3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote
//  products derived from this software without specific prior written permission.
//
//  THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES,
//  INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
//  DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
//  SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
//  SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY,
//  WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE
//  USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

use std::convert::TryInto;

use async_trait::async_trait;
use log::*;
use minotari_app_grpc::tari_rpc::{
    self as grpc,
    GetShardKeyRequest,
    GetValidatorNodeChangesRequest,
    ValidatorNodeChange,
};
use minotari_node_grpc_client::BaseNodeGrpcClient;
use tari_common_types::types::{FixedHash, PublicKey};
use tari_core::{blocks::BlockHeader, transactions::transaction_components::CodeTemplateRegistration};
use tari_dan_common_types::SubstateAddress;
use tari_utilities::ByteArray;
use url::Url;

use crate::{
    types::{BaseLayerConsensusConstants, BaseLayerMetadata, BaseLayerValidatorNode, BlockInfo, SideChainUtxos},
    BaseNodeClient,
    BaseNodeClientError,
};

const LOG_TARGET: &str = "tari::validator_node::app";

type Client = BaseNodeGrpcClient<tonic::transport::Channel>;

#[derive(Clone)]
pub struct GrpcBaseNodeClient {
    endpoint: Url,
    client: Option<Client>,
}

impl GrpcBaseNodeClient {
    pub fn new(endpoint: Url) -> Self {
        Self { endpoint, client: None }
    }

    pub async fn connect(endpoint: Url) -> Result<Self, BaseNodeClientError> {
        let mut client = Self { endpoint, client: None };
        client.test_connection().await?;
        Ok(client)
    }

    async fn connection(&mut self) -> Result<&mut Client, BaseNodeClientError> {
        if self.client.is_none() {
            let inner = Client::connect(self.endpoint.to_string()).await?;
            self.client = Some(inner);
        }
        self.client.as_mut().ok_or(BaseNodeClientError::ConnectionError)
    }

    pub async fn get_mempool_transaction_count(&mut self) -> Result<usize, BaseNodeClientError> {
        let inner = self.connection().await.unwrap();
        let request = grpc::GetMempoolTransactionsRequest {};

        let mut count = 0;
        let mut stream = inner.get_mempool_transactions(request).await?.into_inner();
        loop {
            match stream.message().await {
                Ok(Some(_val)) => {
                    count += 1;
                },
                Ok(None) => {
                    break;
                },
                Err(e) => {
                    warn!(target: LOG_TARGET, "Error getting mempool transaction count: {}", e);
                    return Err(BaseNodeClientError::ConnectionError);
                },
            }
        }
        Ok(count)
    }
}

#[async_trait]
impl BaseNodeClient for GrpcBaseNodeClient {
    async fn test_connection(&mut self) -> Result<(), BaseNodeClientError> {
        self.connection().await?;
        Ok(())
    }

    async fn get_tip_info(&mut self) -> Result<BaseLayerMetadata, BaseNodeClientError> {
        let inner = self.connection().await?;
        let request = grpc::Empty {};
        let result = inner.get_tip_info(request).await?.into_inner();
        let metadata = result
            .metadata
            .ok_or_else(|| BaseNodeClientError::InvalidPeerMessage("Base node returned no metadata".to_string()))?;
        Ok(BaseLayerMetadata {
            height_of_longest_chain: metadata.best_block_height,
            tip_hash: metadata.best_block_hash.try_into().map_err(|_| {
                BaseNodeClientError::InvalidPeerMessage("best_block was not a valid fixed hash".to_string())
            })?,
        })
    }

    async fn get_validator_node_changes(
        &mut self,
        start_height: u64,
        end_height: u64,
        sidechain_id: Option<&PublicKey>,
    ) -> Result<Vec<ValidatorNodeChange>, BaseNodeClientError> {
        let client = self.connection().await?;
        let result = client
            .get_validator_node_changes(GetValidatorNodeChangesRequest {
                start_height,
                end_height,
                sidechain_id: match sidechain_id {
                    None => vec![],
                    Some(sidechain_id) => sidechain_id.to_vec(),
                },
            })
            .await?
            .into_inner();

        Ok(result.changes)
    }

    async fn get_validator_nodes(&mut self, height: u64) -> Result<Vec<BaseLayerValidatorNode>, BaseNodeClientError> {
        let inner = self.connection().await?;

        // SidechainId is empty because we need all the sidechain nodes to create the merkle root
        let request = grpc::GetActiveValidatorNodesRequest {
            height,
            sidechain_id: vec![],
        };
        let mut stream = inner.get_active_validator_nodes(request).await?.into_inner();

        let mut vns = vec![];
        loop {
            match stream.message().await {
                Ok(Some(val)) => {
                    vns.push(BaseLayerValidatorNode {
                        public_key: PublicKey::from_canonical_bytes(&val.public_key).map_err(|_| {
                            BaseNodeClientError::InvalidPeerMessage("public_key was not a valid public key".to_string())
                        })?,
                        shard_key: {
                            let hash = FixedHash::try_from(val.shard_key.as_slice()).map_err(|_| {
                                BaseNodeClientError::InvalidPeerMessage(
                                    "shard_key was not a valid fixed hash".to_string(),
                                )
                            })?;
                            SubstateAddress::from_hash_and_version(hash, 0)
                        },
                        sidechain_id: if val.sidechain_id.is_empty() {
                            None
                        } else {
                            Some(PublicKey::from_canonical_bytes(&val.sidechain_id).map_err(|_| {
                                BaseNodeClientError::InvalidPeerMessage(
                                    "sidechain_id was not a valid public key".to_string(),
                                )
                            }))
                        }
                        .transpose()?,
                    });
                },
                Ok(None) => {
                    break;
                },
                Err(e) => {
                    return Err(BaseNodeClientError::InvalidPeerMessage(format!(
                        "Error reading stream: {}",
                        e
                    )));
                },
            }
        }

        if vns.is_empty() {
            debug!(target: LOG_TARGET, "No validator nodes at height {}", height);
        }

        Ok(vns)
    }

    async fn get_shard_key(
        &mut self,
        height: u64,
        public_key: &PublicKey,
    ) -> Result<Option<SubstateAddress>, BaseNodeClientError> {
        let inner = self.connection().await?;
        let request = GetShardKeyRequest {
            height,
            public_key: public_key.as_bytes().to_vec(),
        };
        let result = inner.get_shard_key(request).await?.into_inner();
        if result.shard_key.is_empty() {
            Ok(None)
        } else {
            // The SubstateAddress type has 4 extra bytes for the version, this is disregarded for validator node shard
            // key.
            // TODO: separate type for validator node shard key
            let hash = FixedHash::try_from(result.shard_key.as_slice())?;
            Ok(Some(SubstateAddress::from_hash_and_version(hash, 0)))
        }
    }

    async fn get_template_registrations(
        &mut self,
        start_hash: Option<FixedHash>,
        count: u64,
    ) -> Result<Vec<CodeTemplateRegistration>, BaseNodeClientError> {
        let inner = self.connection().await?;
        let request = grpc::GetTemplateRegistrationsRequest {
            start_hash: start_hash.map(|v| v.to_vec()).unwrap_or_default(),
            count,
        };
        let mut templates = vec![];
        let mut stream = inner.get_template_registrations(request).await?.into_inner();
        loop {
            match stream.message().await {
                Ok(Some(val)) => {
                    let template_registration: CodeTemplateRegistration = val
                        .registration
                        .ok_or_else(|| {
                            BaseNodeClientError::InvalidPeerMessage(
                                "Base node returned no template registration".to_string(),
                            )
                        })?
                        .try_into()
                        .map_err(|_| {
                            BaseNodeClientError::InvalidPeerMessage("invalid template registration".to_string())
                        })?;
                    templates.push(template_registration);
                },
                Ok(None) => {
                    break;
                },
                Err(e) => {
                    return Err(BaseNodeClientError::InvalidPeerMessage(format!(
                        "Error reading stream: {}",
                        e
                    )));
                },
            }
        }
        Ok(templates)
    }

    async fn get_header_by_hash(&mut self, block_hash: FixedHash) -> Result<BlockHeader, BaseNodeClientError> {
        let inner = self.connection().await?;
        let request = grpc::GetHeaderByHashRequest {
            hash: block_hash.to_vec(),
        };
        let result = inner.get_header_by_hash(request).await?.into_inner();
        let header = result
            .header
            .ok_or_else(|| BaseNodeClientError::InvalidPeerMessage("Base node returned no header".to_string()))?;
        let header = header.try_into().map_err(BaseNodeClientError::InvalidPeerMessage)?;
        Ok(header)
    }

    async fn get_consensus_constants(
        &mut self,
        block_height: u64,
    ) -> Result<BaseLayerConsensusConstants, BaseNodeClientError> {
        let inner = self.connection().await?;

        let request = grpc::BlockHeight { block_height };
        let result = inner.get_constants(request).await?.into_inner();

        let consensus_constants = BaseLayerConsensusConstants {
            epoch_length: result.epoch_length,
            validator_node_registration_min_deposit_amount: result
                .validator_node_registration_min_deposit_amount
                .into(),
        };
        Ok(consensus_constants)
    }

    async fn get_sidechain_utxos(
        &mut self,
        start_hash: Option<FixedHash>,
        count: u64,
    ) -> Result<Vec<SideChainUtxos>, BaseNodeClientError> {
        let inner = self.connection().await?;
        let request = grpc::GetSideChainUtxosRequest {
            start_hash: start_hash.map(|v| v.to_vec()).unwrap_or_default(),
            count,
        };
        let mut stream = inner.get_side_chain_utxos(request).await?.into_inner();
        let mut responses = Vec::with_capacity(count as usize);
        loop {
            match stream.message().await {
                Ok(Some(resp)) => {
                    let block_info = resp.block_info.ok_or_else(|| {
                        BaseNodeClientError::InvalidPeerMessage("Base node returned no block info".to_string())
                    })?;
                    let resp = SideChainUtxos {
                        block_info: BlockInfo {
                            height: block_info.height,
                            hash: block_info.hash.try_into()?,
                            next_block_hash: Some(block_info.next_block_hash)
                                .filter(|v| !v.is_empty())
                                .map(TryInto::try_into)
                                .transpose()?,
                        },
                        outputs: resp
                            .outputs
                            .into_iter()
                            .map(TryInto::try_into)
                            .collect::<Result<_, _>>()
                            .map_err(BaseNodeClientError::InvalidPeerMessage)?,
                    };
                    responses.push(resp);
                },
                Ok(None) => {
                    break;
                },
                Err(e) => {
                    return Err(BaseNodeClientError::InvalidPeerMessage(format!(
                        "Error reading stream: {}",
                        e
                    )));
                },
            }
        }

        Ok(responses)
    }
}

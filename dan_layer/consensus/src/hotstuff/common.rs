//   Copyright 2023 The Tari Project
//   SPDX-License-Identifier: BSD-3-Clause

use std::ops::ControlFlow;

use log::*;
use tari_common::configuration::Network;
use tari_common_types::types::FixedHash;
use tari_dan_common_types::{committee::Committee, shard::Shard, Epoch, NodeAddressable, NodeHeight};
use tari_dan_storage::consensus_models::{Block, LeafBlock, PendingStateTreeDiff, QuorumCertificate};
use tari_engine_types::substate::SubstateDiff;
use tari_state_tree::{
    Hash,
    StagedTreeStore,
    StateHashTreeDiff,
    StateTreeError,
    SubstateTreeChange,
    TreeStoreReader,
    Version,
};

use crate::traits::LeaderStrategy;

const LOG_TARGET: &str = "tari::dan::consensus::hotstuff::common";

/// The value that fees are divided by to determine the amount of fees to burn. 0 means no fees are burned.
/// This is a placeholder for the fee exhaust consensus constant so that we know where it's used later.
pub const EXHAUST_DIVISOR: u64 = 20; // 5%

/// Calculates the dummy block required to reach the new height and returns the last dummy block (parent for next
/// proposal).
pub fn calculate_last_dummy_block<TAddr: NodeAddressable, TLeaderStrategy: LeaderStrategy<TAddr>>(
    network: Network,
    epoch: Epoch,
    shard: Shard,
    high_qc: &QuorumCertificate,
    parent_merkle_root: FixedHash,
    new_height: NodeHeight,
    leader_strategy: &TLeaderStrategy,
    local_committee: &Committee<TAddr>,
    parent_timestamp: u64,
    parent_base_layer_block_height: u64,
    parent_base_layer_block_hash: FixedHash,
) -> Option<LeafBlock> {
    let mut dummy = None;
    with_dummy_blocks(
        network,
        epoch,
        shard,
        high_qc,
        parent_merkle_root,
        new_height,
        leader_strategy,
        local_committee,
        parent_timestamp,
        parent_base_layer_block_height,
        parent_base_layer_block_hash,
        |dummy_block| {
            dummy = Some(dummy_block.as_leaf_block());
            ControlFlow::Continue(())
        },
    );

    dummy
}

/// Calculates the dummy block required to reach the new height
pub fn calculate_dummy_blocks<TAddr: NodeAddressable, TLeaderStrategy: LeaderStrategy<TAddr>>(
    candidate_block: &Block,
    justify_block: &Block,
    leader_strategy: &TLeaderStrategy,
    local_committee: &Committee<TAddr>,
) -> Vec<Block> {
    let mut dummies = Vec::new();
    with_dummy_blocks(
        candidate_block.network(),
        justify_block.epoch(),
        justify_block.shard(),
        candidate_block.justify(),
        *justify_block.merkle_root(),
        candidate_block.height(),
        leader_strategy,
        local_committee,
        justify_block.timestamp(),
        justify_block.base_layer_block_height(),
        *justify_block.base_layer_block_hash(),
        |dummy_block| {
            if dummy_block.id() == candidate_block.parent() {
                dummies.push(dummy_block);
                ControlFlow::Break(())
            } else {
                dummies.push(dummy_block);
                ControlFlow::Continue(())
            }
        },
    );

    dummies
}

fn with_dummy_blocks<TAddr, TLeaderStrategy, F>(
    network: Network,
    epoch: Epoch,
    shard: Shard,
    high_qc: &QuorumCertificate,
    parent_merkle_root: FixedHash,
    new_height: NodeHeight,
    leader_strategy: &TLeaderStrategy,
    local_committee: &Committee<TAddr>,
    parent_timestamp: u64,
    parent_base_layer_block_height: u64,
    parent_base_layer_block_hash: FixedHash,
    mut callback: F,
) where
    TAddr: NodeAddressable,
    TLeaderStrategy: LeaderStrategy<TAddr>,
    F: FnMut(Block) -> ControlFlow<()>,
{
    let mut parent_block = high_qc.as_leaf_block();
    let mut current_height = high_qc.block_height() + NodeHeight(1);
    if current_height > new_height {
        warn!(
            target: LOG_TARGET,
            "BUG: 🍼 no dummy blocks to calculate. current height {} is greater than new height {}",
            current_height,
            new_height,
        );
        return;
    }

    debug!(
        target: LOG_TARGET,
        "🍼 calculating dummy blocks from {} to {}",
        current_height,
        new_height,
    );
    loop {
        let leader = leader_strategy.get_leader_public_key(local_committee, current_height);
        let dummy_block = Block::dummy_block(
            network,
            *parent_block.block_id(),
            leader.clone(),
            current_height,
            high_qc.clone(),
            epoch,
            shard,
            parent_merkle_root,
            parent_timestamp,
            parent_base_layer_block_height,
            parent_base_layer_block_hash,
        );
        debug!(
            target: LOG_TARGET,
            "🍼 new dummy block: {}",
            dummy_block,
        );
        parent_block = dummy_block.as_leaf_block();

        if callback(dummy_block).is_break() {
            break;
        }

        if current_height == new_height {
            break;
        }
        current_height += NodeHeight(1);
    }
}

pub fn diff_to_substate_changes(diff: &SubstateDiff) -> impl Iterator<Item = SubstateTreeChange> + '_ {
    diff.down_iter()
        .map(|(substate_id, _version)| SubstateTreeChange::Down {
            id: substate_id.clone(),
        })
        .chain(diff.up_iter().map(move |(substate_id, value)| SubstateTreeChange::Up {
            id: substate_id.clone(),
            value_hash: value.to_value_hash(),
        }))
}

pub fn calculate_state_merkle_diff<TTx: TreeStoreReader<Version>, I: IntoIterator<Item = SubstateTreeChange>>(
    tx: &TTx,
    current_version: Version,
    next_version: Version,
    pending_tree_diffs: Vec<PendingStateTreeDiff>,
    substate_changes: I,
) -> Result<(Hash, StateHashTreeDiff), StateTreeError> {
    debug!(
        target: LOG_TARGET,
        "Calculating state merkle diff from version {} to {} with {} pending diff(s)",
        current_version,
        next_version,
        pending_tree_diffs.len(),
    );
    let mut store = StagedTreeStore::new(tx);
    store.apply_ordered_diffs(pending_tree_diffs.into_iter().map(|diff| diff.diff));
    let mut state_tree = tari_state_tree::SpreadPrefixStateTree::new(&mut store);
    let state_root =
        state_tree.put_substate_changes(Some(current_version).filter(|v| *v > 0), next_version, substate_changes)?;
    Ok((state_root, store.into_diff()))
}

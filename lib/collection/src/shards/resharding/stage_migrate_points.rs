use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use rand::seq::SliceRandom;
use tokio::task::block_in_place;
use tokio::time::sleep;

use super::driver::{PersistedState, Stage};
use super::tasks_pool::ReshardTaskProgress;
use super::ReshardKey;
use crate::operations::cluster_ops::ReshardingDirection;
use crate::operations::shared_storage_config::SharedStorageConfig;
use crate::operations::types::{CollectionError, CollectionResult};
use crate::shards::channel_service::ChannelService;
use crate::shards::remote_shard::RemoteShard;
use crate::shards::replica_set::ReplicaState;
use crate::shards::resharding::driver::{
    await_transfer_success, SHARD_TRANSFER_IO_LIMIT_RETRY_INTERVAL,
};
use crate::shards::shard::ShardId;
use crate::shards::shard_holder::LockedShardHolder;
use crate::shards::transfer::resharding_stream_records::transfer_resharding_stream_records;
use crate::shards::transfer::transfer_tasks_pool::TransferTaskProgress;
use crate::shards::transfer::{ShardTransfer, ShardTransferConsensus, ShardTransferMethod};
use crate::shards::CollectionId;

/// Maximum time a point migration transfer might take.
const MIGRATE_POINT_TRANSFER_MAX_DURATION: Duration = Duration::from_secs(24 * 60 * 60);

/// Stage 2: migrate points
///
/// Check whether we need to migrate points into the new shard.
pub(super) fn is_completed(state: &PersistedState) -> bool {
    let state_read = state.read();
    state_read.all_peers_completed(Stage::S2_MigratePoints)
        && state_read.shards_to_migrate().next().is_none()
}

/// Stage 2: migrate points
///
/// Keeps checking what shards are still pending point migrations. For each of them it starts a
/// shard transfer if needed, waiting for them to finish. Once this returns, all points are
/// migrated to the target shard.
#[allow(clippy::too_many_arguments)]
pub(super) async fn drive(
    reshard_key: &ReshardKey,
    state: &PersistedState,
    progress: &Mutex<ReshardTaskProgress>,
    shard_holder: Arc<LockedShardHolder>,
    consensus: &dyn ShardTransferConsensus,
    channel_service: &ChannelService,
    collection_id: &CollectionId,
    shared_storage_config: &SharedStorageConfig,
) -> CollectionResult<()> {
    match reshard_key.direction {
        ReshardingDirection::Up => {
            drive_up(
                reshard_key,
                state,
                progress,
                shard_holder,
                consensus,
                channel_service,
                collection_id,
                shared_storage_config,
            )
            .await?;
        }
        ReshardingDirection::Down => {
            drive_down(
                reshard_key,
                state,
                progress,
                shard_holder,
                consensus,
                channel_service,
                collection_id,
                shared_storage_config,
            )
            .await?;
        }
    }

    state.write(|data| {
        data.complete_for_all_peers(Stage::S2_MigratePoints);
        data.update(progress, consensus);
    })?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn drive_up(
    reshard_key: &ReshardKey,
    state: &PersistedState,
    progress: &Mutex<ReshardTaskProgress>,
    shard_holder: Arc<LockedShardHolder>,
    consensus: &dyn ShardTransferConsensus,
    channel_service: &ChannelService,
    collection_id: &CollectionId,
    shared_storage_config: &SharedStorageConfig,
) -> CollectionResult<()> {
    let this_peer_id = consensus.this_peer_id();

    while let Some(source_shard_id) = block_in_place(|| state.read().shards_to_migrate().next()) {
        let ongoing_transfer = shard_holder
            .read()
            .await
            .get_transfers(|transfer| {
                transfer.method == Some(ShardTransferMethod::ReshardingStreamRecords)
                    && transfer.shard_id == source_shard_id
                    && transfer.to_shard_id == Some(reshard_key.shard_id)
            })
            .pop();

        // Take the existing transfer if ongoing, or decide on what new transfer we want to start
        let (transfer, start_transfer) = match ongoing_transfer {
            Some(transfer) => (Some(transfer), false),
            None => {
                let incoming_limit = shared_storage_config
                    .incoming_shard_transfers_limit
                    .unwrap_or(usize::MAX);
                let outgoing_limit = shared_storage_config
                    .outgoing_shard_transfers_limit
                    .unwrap_or(usize::MAX);

                let source_peer_ids = {
                    let shard_holder = shard_holder.read().await;
                    let replica_set =
                        shard_holder.get_shard(&source_shard_id).ok_or_else(|| {
                            CollectionError::service_error(format!(
                                "Shard {source_shard_id} not found in the shard holder for resharding",
                            ))
                        })?;

                    let active_peer_ids = replica_set.active_shards().await;
                    if active_peer_ids.is_empty() {
                        return Err(CollectionError::service_error(format!(
                            "No peer with shard {source_shard_id} in active state for resharding",
                        )));
                    }

                    // Respect shard transfer limits, always allow local transfers
                    let (incoming, _) = shard_holder.count_shard_transfer_io(&this_peer_id);
                    if incoming < incoming_limit {
                        active_peer_ids
                            .into_iter()
                            .filter(|peer_id| {
                                let (_, outgoing) = shard_holder.count_shard_transfer_io(peer_id);
                                outgoing < outgoing_limit || peer_id == &this_peer_id
                            })
                            .collect()
                    } else if active_peer_ids.contains(&this_peer_id) {
                        vec![this_peer_id]
                    } else {
                        vec![]
                    }
                };

                if source_peer_ids.is_empty() {
                    log::trace!("Postponing resharding migration transfer from shard {source_shard_id} to stay below transfer limit on peers");
                    sleep(SHARD_TRANSFER_IO_LIMIT_RETRY_INTERVAL).await;
                    continue;
                }

                let source_peer_id = *source_peer_ids.choose(&mut rand::thread_rng()).unwrap();

                // Configure shard transfer object, or use none if doing a local transfer
                if source_peer_id != this_peer_id {
                    debug_assert_ne!(source_shard_id, reshard_key.shard_id);
                    let transfer = ShardTransfer {
                        shard_id: source_shard_id,
                        to_shard_id: Some(reshard_key.shard_id),
                        from: source_peer_id,
                        to: this_peer_id,
                        sync: true,
                        method: Some(ShardTransferMethod::ReshardingStreamRecords),
                    };
                    (Some(transfer), true)
                } else {
                    (None, false)
                }
            }
        };

        match transfer {
            // Transfer from a different peer, start the transfer if needed and await completion
            Some(transfer) => {
                // Create listener for transfer end before proposing to start the transfer
                // That way we're sure we receive all transfer notifications the next operation might create
                let await_transfer_end = shard_holder
                    .read()
                    .await
                    .await_shard_transfer_end(transfer.key(), MIGRATE_POINT_TRANSFER_MAX_DURATION);

                if start_transfer {
                    consensus
                        .start_shard_transfer_confirm_and_retry(&transfer, collection_id)
                        .await?;
                }

                await_transfer_success(
                    reshard_key,
                    &transfer,
                    &shard_holder,
                    collection_id,
                    consensus,
                    await_transfer_end,
                )
                .await
                .map_err(|err| {
                    CollectionError::service_error(format!(
                        "Failed to migrate points from shard {source_shard_id} to {} for resharding: {err}",
                        reshard_key.shard_id,
                    ))
                })?;
            }
            // Transfer locally, within this peer
            None => {
                migrate_local(
                    shard_holder.clone(),
                    consensus,
                    channel_service.clone(),
                    collection_id,
                    source_shard_id,
                    reshard_key.shard_id,
                )
                .await?;
            }
        }

        state.write(|data| {
            data.migrated_shards.push(source_shard_id);
            data.update(progress, consensus);
        })?;
        log::debug!(
            "Points of shard {source_shard_id} successfully migrated into shard {} for resharding",
            reshard_key.shard_id,
        );
    }

    // Switch new shard on this node into active state
    consensus
        .set_shard_replica_set_state_confirm_and_retry(
            collection_id,
            None,
            reshard_key.shard_id,
            ReplicaState::Active,
            Some(ReplicaState::Resharding),
        )
        .await?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn drive_down(
    reshard_key: &ReshardKey,
    state: &PersistedState,
    progress: &Mutex<ReshardTaskProgress>,
    shard_holder: Arc<LockedShardHolder>,
    consensus: &dyn ShardTransferConsensus,
    channel_service: &ChannelService,
    collection_id: &CollectionId,
    shared_storage_config: &SharedStorageConfig,
) -> CollectionResult<()> {
    let this_peer_id = consensus.this_peer_id();

    while let Some((target_shard_id, target_peer_id)) = block_in_place(|| {
        state
            .read()
            .replicas_to_migrate(shard_holder.clone())
            .next()
    }) {
        let ongoing_transfer = shard_holder
            .read()
            .await
            .get_transfers(|transfer| {
                transfer.method == Some(ShardTransferMethod::ReshardingStreamRecords)
                    && transfer.shard_id == reshard_key.shard_id
                    && transfer.to_shard_id == Some(target_shard_id)
                    && transfer.to == target_peer_id
            })
            .pop();

        // Take the existing transfer if ongoing, or decide on what new transfer we want to start
        let (transfer, start_transfer) = match ongoing_transfer {
            Some(transfer) => (Some(transfer), false),
            None => {
                let incoming_limit = shared_storage_config
                    .incoming_shard_transfers_limit
                    .unwrap_or(usize::MAX);
                let outgoing_limit = shared_storage_config
                    .outgoing_shard_transfers_limit
                    .unwrap_or(usize::MAX);

                let source_peer_ids = {
                    let shard_holder = shard_holder.read().await;
                    let replica_set =
                        shard_holder.get_shard(&reshard_key.shard_id).ok_or_else(|| {
                            CollectionError::service_error(format!(
                                "Shard {target_shard_id} not found in the shard holder for resharding",
                            ))
                        })?;

                    let active_peer_ids = replica_set.active_shards().await;
                    if active_peer_ids.is_empty() {
                        return Err(CollectionError::service_error(format!(
                            "No peer with shard {target_shard_id} in active state for resharding",
                        )));
                    }

                    // Respect shard transfer limits, always allow local transfers
                    let (incoming, _) = shard_holder.count_shard_transfer_io(&target_peer_id);
                    if incoming < incoming_limit {
                        active_peer_ids
                            .into_iter()
                            .filter(|peer_id| {
                                let (_, outgoing) = shard_holder.count_shard_transfer_io(peer_id);
                                outgoing < outgoing_limit
                                    || (peer_id == &target_peer_id && peer_id == &this_peer_id)
                            })
                            .collect()
                    } else if active_peer_ids.contains(&this_peer_id)
                        && target_peer_id == this_peer_id
                    {
                        vec![this_peer_id]
                    } else {
                        vec![]
                    }
                };

                if source_peer_ids.is_empty() {
                    log::trace!("Postponing resharding migration transfer from shard {} to stay below transfer limit on peers", reshard_key.shard_id);
                    sleep(SHARD_TRANSFER_IO_LIMIT_RETRY_INTERVAL).await;
                    continue;
                }

                let source_peer_id = *source_peer_ids.choose(&mut rand::thread_rng()).unwrap();

                // Configure shard transfer object, or use none if doing a local transfer
                if source_peer_id != target_peer_id || source_peer_id != this_peer_id {
                    debug_assert_ne!(target_shard_id, reshard_key.shard_id);
                    let transfer = ShardTransfer {
                        shard_id: reshard_key.shard_id,
                        to_shard_id: Some(target_shard_id),
                        from: source_peer_id,
                        to: target_peer_id,
                        sync: true,
                        method: Some(ShardTransferMethod::ReshardingStreamRecords),
                    };
                    (Some(transfer), true)
                } else {
                    (None, false)
                }
            }
        };

        match transfer {
            // Transfer from a different peer, start the transfer if needed and await completion
            Some(transfer) => {
                // Create listener for transfer end before proposing to start the transfer
                // That way we're sure we receive all transfer notifications the next operation might create
                let await_transfer_end = shard_holder
                    .read()
                    .await
                    .await_shard_transfer_end(transfer.key(), MIGRATE_POINT_TRANSFER_MAX_DURATION);

                if start_transfer {
                    consensus
                        .start_shard_transfer_confirm_and_retry(&transfer, collection_id)
                        .await?;
                }

                await_transfer_success(
                    reshard_key,
                    &transfer,
                    &shard_holder,
                    collection_id,
                    consensus,
                    await_transfer_end,
                )
                .await
                .map_err(|err| {
                    CollectionError::service_error(format!(
                        "Failed to migrate points from shard {} to {target_shard_id} for resharding: {err}",
                        reshard_key.shard_id,
                    ))
                })?;

                // Switch target replica back into active state
                consensus
                    .set_shard_replica_set_state_confirm_and_retry(
                        collection_id,
                        Some(target_peer_id),
                        target_shard_id,
                        ReplicaState::Active,
                        Some(ReplicaState::Resharding),
                    )
                    .await?;
            }
            // Transfer locally, within this peer
            None => {
                migrate_local(
                    shard_holder.clone(),
                    consensus,
                    channel_service.clone(),
                    collection_id,
                    reshard_key.shard_id,
                    target_shard_id,
                )
                .await?;
            }
        }

        state.write(|data| {
            data.migrated_replicas
                .push((target_shard_id, target_peer_id));
            data.update(progress, consensus);
        })?;
        log::debug!(
            "Points of shard {} successfully migrated into replica {target_peer_id}:{target_shard_id} for resharding",
            reshard_key.shard_id,
        );
    }

    Ok(())
}

/// Migrate a shard locally, within the same node.
///
/// This is a special case for migration transfers, because normal shard transfer don't support the
/// same source and target node.
// TODO(resharding): improve this, don't rely on shard transfers and remote shards, copy directly
// between the two local shard replica
async fn migrate_local(
    shard_holder: Arc<LockedShardHolder>,
    consensus: &dyn ShardTransferConsensus,
    channel_service: ChannelService,
    collection_id: &CollectionId,
    source_shard_id: ShardId,
    target_shard_id: ShardId,
) -> CollectionResult<()> {
    log::debug!("Migrating points of shard {source_shard_id} into shard {target_shard_id} locally for resharding");

    // Target shard is on the same node, but has a different shard ID
    let target_shard = RemoteShard::new(
        target_shard_id,
        collection_id.clone(),
        consensus.this_peer_id(),
        channel_service,
    );

    let progress = Arc::new(Mutex::new(TransferTaskProgress::new()));
    let result = transfer_resharding_stream_records(
        Arc::clone(&shard_holder),
        progress,
        source_shard_id,
        target_shard,
        collection_id,
    )
    .await;

    // Unproxify forward proxy on local shard we just transferred from
    // Normally consensus takes care of this, but we don't use consensus here
    {
        let shard_holder = shard_holder.read().await;
        let replica_set = shard_holder.get_shard(&source_shard_id).ok_or_else(|| {
            CollectionError::service_error(format!(
                "Shard {source_shard_id} not found in the shard holder for resharding, cannot unproxify after local transfer",
            ))
        })?;
        replica_set.un_proxify_local().await?;
    }

    result
}

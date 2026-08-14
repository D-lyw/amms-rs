use super::{
    AppliedLogDedupCache, HookRegistry, LogQueryChunk, LogSource, PendingSyncQueue, StateSpace,
    StateSpaceError, StateSpaceManager,
};
use crate::state_space::{STREAM_IDLE_TIMEOUT, STREAM_RECONNECT_DELAY};
use alloy::consensus::BlockHeader;
use alloy::network::primitives::HeaderResponse;
use alloy::network::Network;
use alloy::providers::Provider;
use async_stream::stream;
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::time::sleep;
use tracing::{error, info, warn};

impl<N, P> StateSpaceManager<N, P> {
    pub(super) fn subscribe_new_heads_stream(
        provider: P,
        state: Arc<RwLock<StateSpace>>,
        hooks: HookRegistry<Vec<alloy::primitives::Address>>,
        update_seq: Arc<AtomicU64>,
        realtime_head: Arc<AtomicU64>,
        canonical_head: Arc<AtomicU64>,
        pending_sync_queue: Arc<Mutex<PendingSyncQueue>>,
        pending_sync_notify: Arc<Notify>,
        applied_log_dedup: Arc<Mutex<AppliedLogDedupCache>>,
        query_chunks: Vec<LogQueryChunk>,
        chain_id: u64,
    ) -> impl Stream<
        Item = Result<
            (super::RealtimeUpdateMeta, Vec<alloy::primitives::Address>),
            StateSpaceError,
        >,
    > + Send
    where
        P: Provider<N> + Clone + 'static,
        N: Network + 'static,
        N::HeaderResponse: Send,
    {
        stream! {
            loop {
                // XLayer 公共网关不再白名单 eth_subscribe（-32601），newHeads 订阅
                // 使用专用 WS provider；其余链沿用主 provider（回填仍走主 provider）。
                let subscribed: Option<
                    Pin<Box<dyn Stream<Item = N::HeaderResponse> + Send>>,
                > = if chain_id == super::XLAYER_CHAIN_ID {
                    match Self::connect_xlayer_subscribe_provider().await {
                        Some(sub_provider) => match sub_provider.subscribe_blocks().await {
                            Ok(sub) => Some(Box::pin(sub.into_stream())),
                            Err(e) => {
                                error!("Failed to subscribe to newHeads: {}", e);
                                None
                            }
                        },
                        None => None,
                    }
                } else {
                    match provider.subscribe_blocks().await {
                        Ok(sub) => Some(Box::pin(sub.into_stream())),
                        Err(e) => {
                            error!("Failed to subscribe to newHeads: {}", e);
                            None
                        }
                    }
                };

                let mut heads_stream = match subscribed {
                    Some(stream) => stream,
                    None => {
                        sleep(STREAM_RECONNECT_DELAY).await;
                        continue;
                    }
                };

                match Self::initial_backfill_results(
                    &provider,
                    &state,
                    &hooks,
                    &query_chunks,
                    &realtime_head,
                    &canonical_head,
                    &pending_sync_queue,
                    &pending_sync_notify,
                    &applied_log_dedup,
                    LogSource::NewHeadsPull,
                    chain_id,
                )
                .await
                {
                    Ok(results) => {
                        // Catch-up stage: apply state updates only; do not emit tradable updates downstream.
                        let mut non_empty_batches = 0usize;
                        let mut affected_pools = 0usize;
                        for (_, affected) in results {
                            if !affected.is_empty() {
                                non_empty_batches += 1;
                                affected_pools += affected.len();
                            }
                        }
                        if non_empty_batches > 0 {
                            info!(
                                non_empty_batches,
                                affected_pools,
                                "Initial catch-up completed (updates suppressed during catch-up stage)"
                            );
                        }
                    }
                    Err(e) => {
                        warn!("Initial backfill failed before newHeads subscribe: {}", e);
                    }
                }

                let mut last_hash: Option<alloy::primitives::FixedBytes<32>> = None;

                loop {
                    match tokio::time::timeout(STREAM_IDLE_TIMEOUT, heads_stream.next()).await {
                        Ok(Some(new_head)) => {
                            let block_num = new_head.number();
                            let block_hash = new_head.hash();
                            let parent_hash = new_head.parent_hash();
                            let logs_bloom = new_head.logs_bloom();

                            let last_processed = realtime_head.load(Ordering::Relaxed);
                            if block_num < last_processed {
                                continue;
                            }

                            if block_num == last_processed {
                                if let Some(last) = last_hash {
                                    if last == block_hash {
                                        continue;
                                    }
                                }
                            } else {
                                if block_num > last_processed + 1 {
                                    let backfill_received_at = Instant::now();
                                    match Self::backfill_range(
                                        &provider,
                                        &state,
                                        &hooks,
                                        &query_chunks,
                                        last_processed + 1,
                                        block_num - 1,
                                        &realtime_head,
                                        &canonical_head,
                                        &pending_sync_queue,
                                        &pending_sync_notify,
                                        &applied_log_dedup,
                                        LogSource::NewHeadsPull,
                                        chain_id,
                                    )
                                    .await
                                    {
                                        Ok(results) => {
                                            for (backfill_block_num, affected) in results {
                                                if !affected.is_empty() {
                                                    let meta = super::build_realtime_update_meta(
                                                        &update_seq,
                                                        backfill_block_num,
                                                        backfill_received_at,
                                                        None,
                                                    );
                                                    super::log_realtime_update_applied(
                                                        meta,
                                                        affected.len(),
                                                        0,
                                                    );
                                                    yield Ok((meta, affected));
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            error!(
                                                "Gap backfill {}..{} failed: {}",
                                                last_processed + 1,
                                                block_num - 1,
                                                e
                                            );
                                            continue;
                                        }
                                    }
                                } else if let Some(last) = last_hash {
                                    if parent_hash != last {
                                        warn!(
                                            "Parent hash mismatch at block {} (expected parent {}, got {})",
                                            block_num,
                                            last,
                                            parent_hash
                                        );
                                    }
                                }
                            }

                            let received_at = Instant::now();
                            let logs = match Self::collect_logs_for_chunks(
                                &provider,
                                &query_chunks,
                                block_num,
                                block_num,
                                Some(&logs_bloom),
                            )
                            .await
                            {
                                Ok(logs) => logs,
                                Err(e) => {
                                    error!("get_logs failed for block {}: {}", block_num, e);
                                    continue;
                                }
                            };
                            let log_count = logs.len();

                            match Self::apply_logs_for_block(
                                &provider,
                                &state,
                                &hooks,
                                block_num,
                                logs,
                                &realtime_head,
                                &canonical_head,
                                &pending_sync_queue,
                                &pending_sync_notify,
                                &applied_log_dedup,
                                LogSource::NewHeadsPull,
                            )
                            .await
                            {
                                Ok(affected) => {
                                    last_hash = Some(block_hash);
                                    if !affected.is_empty() {
                                        let meta = super::build_realtime_update_meta(
                                            &update_seq,
                                            block_num,
                                            received_at,
                                            None,
                                        );
                                        super::log_realtime_update_applied(
                                            meta,
                                            affected.len(),
                                            log_count,
                                        );
                                        yield Ok((meta, affected));
                                    }
                                }
                                Err(e) => {
                                    error!("newHeads processing failed for block {}: {}", block_num, e);
                                }
                            }
                        }
                        Ok(None) => {
                            warn!("newHeads stream ended");
                            break;
                        }
                        Err(_) => {
                            warn!("newHeads stream timeout, reconnecting");
                            break;
                        }
                    }
                }

                sleep(STREAM_RECONNECT_DELAY).await;
            }
        }
    }
}

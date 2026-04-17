use super::{HookRegistry, LogQueryChunk, StateSpace, StateSpaceError, StateSpaceManager};
use crate::state_space::{STREAM_IDLE_TIMEOUT, STREAM_RECONNECT_DELAY};
use alloy::network::Network;
use alloy::providers::Provider;
use alloy::rpc::types::eth::Log;
use async_stream::stream;
use futures::{Stream, StreamExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio::time::sleep;
use tracing::{error, warn};

impl<N, P> StateSpaceManager<N, P> {
    pub(super) fn subscribe_ws_logs_stream(
        provider: P,
        state: Arc<RwLock<StateSpace>>,
        hooks: HookRegistry<Vec<alloy::primitives::Address>>,
        latest_block: Arc<AtomicU64>,
        query_chunks: Vec<LogQueryChunk>,
        chain_id: u64,
    ) -> impl Stream<Item = Result<Vec<alloy::primitives::Address>, StateSpaceError>> + Send
    where
        P: Provider<N> + Clone + 'static,
        N: Network + 'static,
    {
        stream! {
            loop {
                match Self::initial_backfill_results(
                    &provider,
                    &state,
                    &hooks,
                    &query_chunks,
                    &latest_block,
                    chain_id,
                )
                .await
                {
                    Ok(results) => {
                        for affected in results {
                            if !affected.is_empty() {
                                yield Ok(affected);
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Initial backfill failed before wsLogs subscribe: {}", e);
                    }
                }

                let (tx, mut rx) = mpsc::channel::<Log>(8192);
                let mut active_subscriptions = 0usize;

                for chunk in &query_chunks {
                    let filter = chunk.subscription_filter();
                    match provider.subscribe_logs(&filter).await {
                        Ok(sub) => {
                            active_subscriptions += 1;
                            let mut stream = sub.into_stream();
                            let tx_cloned = tx.clone();
                            tokio::spawn(async move {
                                while let Some(log) = stream.next().await {
                                    if tx_cloned.send(log).await.is_err() {
                                        break;
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            warn!("wsLogs chunk subscribe failed: {}", e);
                        }
                    }
                }

                drop(tx);

                if active_subscriptions == 0 {
                    warn!("No active wsLogs subscriptions; reconnecting");
                    sleep(STREAM_RECONNECT_DELAY).await;
                    continue;
                }

                loop {
                    match tokio::time::timeout(STREAM_IDLE_TIMEOUT, rx.recv()).await {
                        Ok(Some(first_log)) => {
                            let mut logs = vec![first_log];
                            while let Ok(log) = rx.try_recv() {
                                logs.push(log);
                            }

                            let block_num = logs
                                .iter()
                                .filter_map(|l| l.block_number)
                                .max()
                                .unwrap_or_else(|| latest_block.load(Ordering::Relaxed));

                            match Self::apply_logs_for_block(
                                &provider,
                                &state,
                                &hooks,
                                block_num,
                                logs,
                                &latest_block,
                            )
                            .await
                            {
                                Ok(affected) => {
                                    if !affected.is_empty() {
                                        yield Ok(affected);
                                    }
                                }
                                Err(e) => {
                                    error!("wsLogs processing failed: {}", e);
                                }
                            }
                        }
                        Ok(None) => {
                            warn!("wsLogs subscription stream ended");
                            break;
                        }
                        Err(_) => {
                            warn!("wsLogs stream timeout, reconnecting");
                            break;
                        }
                    }
                }

                sleep(STREAM_RECONNECT_DELAY).await;
            }
        }
    }
}

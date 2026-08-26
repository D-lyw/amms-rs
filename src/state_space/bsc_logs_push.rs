use super::{
    build_applied_log_key, chunk_to_subscription_filter, AppliedLogDedupCache, HookRegistry,
    LogQueryChunk, LogSource, PendingLogDedupCache, PendingSyncQueue, StateSpace, StateSpaceError,
    StateSpaceManager,
};
use crate::state_space::{BSC_MAINNET_CHAIN_ID, STREAM_IDLE_TIMEOUT, STREAM_RECONNECT_DELAY};
use alloy::network::Network;
use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::rpc::types::eth::Log;
use async_stream::stream;
use futures::{SinkExt, Stream, StreamExt};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{error, info, warn};

// BSC 主网实时同步：标准 geth `eth_subscribe("logs")` push 订阅。
//
// BSC 是 PoSA 共识的原生 L1（0.45s 块），没有 flashblocks 端点也没有
// sequencer feed。实时通道采用标准 `logs` 订阅推送**已打包块**的日志：
// - `blockNumber` + block-global `logIndex` 齐全，与 canonical getLogs
//   的位置 key 完全一致，push 与 backfill 天然互斥去重；
// - 漏推/断流由两层兜底修复（与 Base pendingLogs 相同的兜底体系）：
//   ① log 级：断线重连时 `initial_backfill_results` 按块 getLogs 补拉；
//   ② 状态级：`run_silent_drift_probe_task` / `run_maintenance_coverage_scheduler`
//      探到漂移后经 `pending_sync_worker` 做池级 eth_call 重同步。
//
// 端点由调用方通过 `StateSpaceBuilder::with_realtime_ws_endpoints(...)`
// 提供，必须支持标准 `eth_subscribe("logs")`；传入的 `Provider` 继续负责
// 初始同步、backfill、对账等 RPC 职责（必须支持 `eth_getLogs`）。

impl<N, P> StateSpaceManager<N, P> {
    pub(super) fn subscribe_bsc_logs_push_stream(
        provider: P,
        state: Arc<RwLock<StateSpace>>,
        hooks: HookRegistry<Vec<Address>>,
        update_seq: Arc<AtomicU64>,
        realtime_head: Arc<AtomicU64>,
        canonical_head: Arc<AtomicU64>,
        pending_sync_queue: Arc<Mutex<PendingSyncQueue>>,
        pending_sync_notify: Arc<Notify>,
        applied_log_dedup: Arc<Mutex<AppliedLogDedupCache>>,
        query_chunks: Vec<LogQueryChunk>,
        ws_candidates: Vec<String>,
        chain_id: u64,
    ) -> impl Stream<
        Item = Result<
            (super::RealtimeUpdateMeta, Vec<Address>),
            StateSpaceError,
        >,
    > + Send
    where
        P: Provider<N> + Clone + 'static,
        N: Network + 'static,
    {
        // 与 canonical getLogs 路径共用 query chunks，保证订阅覆盖一致。
        let filters: Vec<Value> = query_chunks
            .iter()
            .map(chunk_to_subscription_filter)
            .collect();

        stream! {
            let mut pending_log_dedup = PendingLogDedupCache::default();

            loop {
                // 启动/重连 catch-up：把 realtime_head 之后已确认块用 getLogs 补回，
                // 期间更新抑制（不产出下游通知），与 Base pendingLogs 管线一致。
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
                    LogSource::BscLogsPush,
                    chain_id,
                )
                .await
                {
                    Ok(results) => {
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
                        warn!("Initial backfill failed before BSC logs subscribe: {}", e);
                    }
                }

                let mut last_err = None;
                let mut connected = None;
                for candidate in &ws_candidates {
                    match connect_async(candidate.clone()).await {
                        Ok((socket, _response)) => {
                            connected = Some((socket, candidate.clone()));
                            break;
                        }
                        Err(e) => {
                            warn!(ws_url = candidate, "BSC logs push ws connect failed: {}", e);
                            last_err = Some(format!("{candidate}: {e}"));
                        }
                    }
                }

                let (mut socket, chosen_ws) = match connected {
                    Some(v) => v,
                    None => {
                        error!(
                            chain_id = BSC_MAINNET_CHAIN_ID,
                            "BSC logs push failed to connect any WSS endpoint: {}",
                            last_err.unwrap_or_else(|| "<unknown>".to_string())
                        );
                        sleep(STREAM_RECONNECT_DELAY).await;
                        continue;
                    }
                };

                info!(
                    chain_id = BSC_MAINNET_CHAIN_ID,
                    ws_url = %chosen_ws,
                    subscriptions = filters.len(),
                    "Connected to BSC logs push WebSocket"
                );

                let mut subscription_ids = HashSet::new();
                let mut subscribe_failed = false;

                for (idx, filter) in filters.iter().enumerate() {
                    let request_id = (idx + 1) as u64;
                    let request = json!({
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "method": "eth_subscribe",
                        "params": ["logs", filter],
                    });

                    if let Err(e) = socket.send(Message::Text(request.to_string().into())).await {
                        warn!("BSC logs push subscribe send failed: {}", e);
                        subscribe_failed = true;
                        break;
                    }

                    let mut ack_received = false;
                    while !ack_received {
                        let next = tokio::time::timeout(STREAM_IDLE_TIMEOUT, socket.next()).await;
                        let Some(message_result) = (match next {
                            Ok(v) => v,
                            Err(_) => {
                                warn!("BSC logs push subscribe ack timeout");
                                subscribe_failed = true;
                                break;
                            }
                        }) else {
                            warn!("BSC logs push stream ended during subscribe");
                            subscribe_failed = true;
                            break;
                        };

                        let message = match message_result {
                            Ok(v) => v,
                            Err(e) => {
                                warn!("BSC logs push subscribe ack receive error: {}", e);
                                subscribe_failed = true;
                                break;
                            }
                        };

                        let payload = match message {
                            Message::Text(text) => text.to_string(),
                            Message::Binary(bin) => {
                                String::from_utf8_lossy(bin.as_ref()).to_string()
                            }
                            Message::Ping(v) => {
                                let _ = socket.send(Message::Pong(v)).await;
                                continue;
                            }
                            Message::Pong(_) => continue,
                            Message::Close(_) => {
                                subscribe_failed = true;
                                break;
                            }
                            Message::Frame(_) => continue,
                        };

                        let value: Value = match serde_json::from_str(&payload) {
                            Ok(v) => v,
                            Err(e) => {
                                warn!("BSC logs push subscribe ack decode failed: {}", e);
                                continue;
                            }
                        };

                        if value.get("method").and_then(Value::as_str) == Some("eth_subscription") {
                            // Notifications can race with later subscribe acks; ignore them during setup.
                            continue;
                        }

                        if value.get("id").and_then(Value::as_u64) != Some(request_id) {
                            continue;
                        }

                        if let Some(err) = value.get("error") {
                            warn!(?err, "BSC logs push subscribe failed");
                            subscribe_failed = true;
                            break;
                        }

                        let Some(sub_id) = value.get("result").and_then(Value::as_str) else {
                            warn!(payload = %payload, "BSC logs push subscribe ack missing result");
                            subscribe_failed = true;
                            break;
                        };

                        subscription_ids.insert(sub_id.to_string());
                        ack_received = true;
                    }

                    if subscribe_failed {
                        break;
                    }
                }

                if subscribe_failed {
                    warn!("BSC logs push subscribe failed; reconnecting");
                    let _ = socket.close(None).await;
                    sleep(STREAM_RECONNECT_DELAY).await;
                    continue;
                }

                info!(
                    chain_id = BSC_MAINNET_CHAIN_ID,
                    active_subscriptions = subscription_ids.len(),
                    "BSC logs push subscriptions established"
                );

                loop {
                    let next = tokio::time::timeout(STREAM_IDLE_TIMEOUT, socket.next()).await;
                    let maybe_message_result = match next {
                        Ok(v) => v,
                        Err(_) => {
                            warn!("BSC logs push stream timeout, reconnecting");
                            break;
                        }
                    };

                    let Some(message_result) = maybe_message_result else {
                        warn!("BSC logs push stream ended");
                        break;
                    };

                    let message = match message_result {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("BSC logs push stream receive error: {}", e);
                            break;
                        }
                    };

                    let received_at = Instant::now();
                    let payload = match message {
                        Message::Text(text) => text.to_string(),
                        Message::Binary(bin) => {
                            String::from_utf8_lossy(bin.as_ref()).to_string()
                        }
                        Message::Ping(v) => {
                            let _ = socket.send(Message::Pong(v)).await;
                            continue;
                        }
                        Message::Pong(_) => continue,
                        Message::Close(_) => break,
                        Message::Frame(_) => continue,
                    };

                    let value: Value = match serde_json::from_str(&payload) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    if value.get("method").and_then(Value::as_str) != Some("eth_subscription") {
                        continue;
                    }

                    let Some(sub_id) = value
                        .get("params")
                        .and_then(|v| v.get("subscription"))
                        .and_then(Value::as_str) else {
                        continue;
                    };

                    if !subscription_ids.contains(sub_id) {
                        continue;
                    }

                    let Some(result) = value
                        .get("params")
                        .and_then(|v| v.get("result"))
                        .cloned() else {
                        continue;
                    };

                    let log = match serde_json::from_value::<Log>(result) {
                        Ok(log) => log,
                        Err(e) => {
                            warn!("BSC logs push log decode failed: {}", e);
                            continue;
                        }
                    };

                    // 标准 logs 订阅应返回已打包块日志（blockNumber 必填）；
                    // 防御性丢弃 pending/半成品推送，避免状态机提前消费。
                    let block_num = match log.block_number {
                        Some(v) => v,
                        None => continue,
                    };

                    // 多个 chunk 订阅可能重叠（共享基础设施合约），
                    // 进入全局去重层前先本地预去重。
                    let prededup_key = build_applied_log_key(&log);
                    if !pending_log_dedup.insert_if_new(prededup_key) {
                        continue;
                    }

                    match Self::apply_logs_for_block(
                        &provider,
                        &state,
                        &hooks,
                        block_num,
                        vec![log],
                        &realtime_head,
                        &canonical_head,
                        &pending_sync_queue,
                        &pending_sync_notify,
                        &applied_log_dedup,
                        LogSource::BscLogsPush,
                    )
                    .await
                    {
                        Ok(affected) => {
                            if !affected.is_empty() {
                                let meta = super::build_realtime_update_meta(
                                    &update_seq,
                                    block_num,
                                    received_at,
                                    None,
                                );
                                super::log_realtime_update_applied(meta, affected.len(), 1);
                                yield Ok((meta, affected));
                            }
                        }
                        Err(e) => {
                            error!("BSC logs push batch process failed: {}", e);
                        }
                    }
                }

                sleep(STREAM_RECONNECT_DELAY).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_space::QueryMode;

    fn sample_chunk() -> LogQueryChunk {
        let addr: Address = "0x16b9a82891338f9bA80E2D6970FddA79D1eb0daE"
            .parse()
            .unwrap();
        LogQueryChunk {
            addresses: vec![addr],
            mode: QueryMode::TopicFiltered(vec![[0xabu8; 32].into()]),
        }
    }

    #[test]
    fn chunk_to_subscription_filter_topic_filtered_shape() {
        let v = chunk_to_subscription_filter(&sample_chunk());
        assert_eq!(
            v["address"][0],
            "0x16b9a82891338f9ba80e2d6970fdda79d1eb0dae"
        );
        let topic: alloy::primitives::FixedBytes<32> = [0xabu8; 32].into();
        assert_eq!(v["topics"][0][0], format!("{topic:?}"));
        assert!(v.get("fromBlock").is_none());
        assert!(v.get("toBlock").is_none());
    }

    #[test]
    fn chunk_to_subscription_filter_address_only_shape() {
        let chunk = LogQueryChunk {
            addresses: sample_chunk().addresses,
            mode: QueryMode::AddressOnly,
        };
        let v = chunk_to_subscription_filter(&chunk);
        assert_eq!(v["address"].as_array().unwrap().len(), 1);
        assert!(v.get("topics").is_none());
    }

    #[test]
    fn pending_log_dedup_cache_collapses_duplicates() {
        let log: Log = serde_json::from_value(json!({
            "address": "0x16b9a82891338f9bA80E2D6970FddA79D1eb0daE",
            "topics": ["0x1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1"],
            "data": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "blockNumber": "0x6ed8000",
            "transactionHash": "0x1111111111111111111111111111111111111111111111111111111111111111",
            "transactionIndex": "0x0",
            "blockHash": "0x2222222222222222222222222222222222222222222222222222222222222222",
            "logIndex": "0x0",
            "removed": false
        }))
        .unwrap();
        let key = build_applied_log_key(&log);
        let mut cache = PendingLogDedupCache::default();
        assert!(cache.insert_if_new(key.clone()));
        assert!(!cache.insert_if_new(key));
    }
}

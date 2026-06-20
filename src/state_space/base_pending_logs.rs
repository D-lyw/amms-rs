use super::{
    AppliedLogDedupCache, HookRegistry, LogQueryChunk, LogSource, PendingSyncQueue,
    StateSpace, StateSpaceError, StateSpaceManager,
};
use crate::state_space::{BASE_CHAIN_ID, STREAM_IDLE_TIMEOUT, STREAM_RECONNECT_DELAY};
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

// Base realtime now consumes `pendingLogs` instead of the historical raw
// Flashblocks payload parser. The upstream raw schema changed, but we want to
// preserve the downstream execution model: decode logs, route to pools through
// `StateSpace::sync()`, and keep all special protocol handling centralized in
// the shared log query/routing code.
//
// The `pendingLogs` subscription itself uses an explicit dedicated WebSocket
// connection. The endpoint list is provided by the caller through
// `StateSpaceBuilder::with_realtime_ws_endpoints(...)`, while the passed
// `Provider` continues to serve regular RPC duties such as initial sync,
// backfill, head tracking, and async/resync refreshes.
//
// Important: these endpoints are not just arbitrary chain RPC WebSockets.
// They must support the Base flashblock-related subscription capability used
// here: `eth_subscribe` with `pendingLogs`.

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PendingLogDedupKey {
    tx_hash: Option<alloy::primitives::B256>,
    log_index: u64,
    address: Address,
    topic0: Option<alloy::primitives::FixedBytes<32>>,
}

#[derive(Default)]
struct PendingLogDedupCache {
    seen: HashSet<PendingLogDedupKey>,
    order: std::collections::VecDeque<PendingLogDedupKey>,
}

impl PendingLogDedupCache {
    fn insert_if_new(&mut self, key: PendingLogDedupKey) -> bool {
        if self.seen.contains(&key) {
            return false;
        }
        self.seen.insert(key.clone());
        self.order.push_back(key);
        while self.order.len() > 300_000 {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        true
    }
}

fn chunk_to_pending_logs_filter(chunk: &LogQueryChunk) -> Value {
    let addresses: Vec<String> = chunk
        .addresses
        .iter()
        .map(|addr| format!("{addr:?}"))
        .collect();

    match &chunk.mode {
        super::QueryMode::TopicFiltered(topics) => {
            let topic0: Vec<String> = topics.iter().map(|topic| format!("{topic:?}")).collect();
            json!({
                "address": addresses,
                "topics": [topic0],
            })
        }
        super::QueryMode::AddressOnly => {
            json!({
                "address": addresses,
            })
        }
    }
}

impl<N, P> StateSpaceManager<N, P> {
    pub(super) fn subscribe_base_pending_logs_stream(
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
            (super::RealtimeUpdateMeta, Vec<alloy::primitives::Address>),
            StateSpaceError,
        >,
    > + Send
    where
        P: Provider<N> + Clone + 'static,
        N: Network + 'static,
    {
        // Reuse the canonical query chunks so Base `pendingLogs` inherits the
        // same subscription coverage as the standard log sync path, including:
        // manager/vault/plugin indirection, Ekubo address-only matching,
        // Slipstream FeeModule events, and other non-trivial AMM cases.
        let filters: Vec<Value> = query_chunks
            .iter()
            .map(chunk_to_pending_logs_filter)
            .collect();

        stream! {
            let mut pending_log_dedup = PendingLogDedupCache::default();

            loop {
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
                    LogSource::RealtimeFlashblock,
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
                        warn!("Initial backfill failed before Base pendingLogs subscribe: {}", e);
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
                            warn!(ws_url = candidate, "Base pendingLogs ws connect failed: {}", e);
                            last_err = Some(format!("{candidate}: {e}"));
                        }
                    }
                }

                let (mut socket, chosen_ws) = match connected {
                    Some(v) => v,
                    None => {
                        error!(
                            chain_id = BASE_CHAIN_ID,
                            "Base pendingLogs failed to connect any WSS endpoint: {}",
                            last_err.unwrap_or_else(|| "<unknown>".to_string())
                        );
                        sleep(STREAM_RECONNECT_DELAY).await;
                        continue;
                    }
                };

                info!(
                    chain_id = BASE_CHAIN_ID,
                    ws_url = %chosen_ws,
                    subscriptions = filters.len(),
                    "Connected to Base pendingLogs WebSocket"
                );

                let mut subscription_ids = HashSet::new();
                let mut subscribe_failed = false;

                for (idx, filter) in filters.iter().enumerate() {
                    let request_id = (idx + 1) as u64;
                    let request = json!({
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "method": "eth_subscribe",
                        "params": ["pendingLogs", filter],
                    });

                    if let Err(e) = socket.send(Message::Text(request.to_string().into())).await {
                        warn!("Base pendingLogs subscribe send failed: {}", e);
                        subscribe_failed = true;
                        break;
                    }

                    let mut ack_received = false;
                    while !ack_received {
                        let next = tokio::time::timeout(STREAM_IDLE_TIMEOUT, socket.next()).await;
                        let Some(message_result) = (match next {
                            Ok(v) => v,
                            Err(_) => {
                                warn!("Base pendingLogs subscribe ack timeout");
                                subscribe_failed = true;
                                break;
                            }
                        }) else {
                            warn!("Base pendingLogs stream ended during subscribe");
                            subscribe_failed = true;
                            break;
                        };

                        let message = match message_result {
                            Ok(v) => v,
                            Err(e) => {
                                warn!("Base pendingLogs subscribe ack receive error: {}", e);
                                subscribe_failed = true;
                                break;
                            }
                        };

                        let payload = match message {
                            Message::Text(text) => text.to_string(),
                            Message::Binary(bin) => String::from_utf8_lossy(bin.as_ref()).to_string(),
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
                                warn!("Base pendingLogs subscribe ack decode failed: {}", e);
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
                            warn!(?err, "Base pendingLogs subscribe failed");
                            subscribe_failed = true;
                            break;
                        }

                        let Some(sub_id) = value.get("result").and_then(Value::as_str) else {
                            warn!(payload = %payload, "Base pendingLogs subscribe ack missing result");
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

                if subscribe_failed || subscription_ids.is_empty() {
                    sleep(STREAM_RECONNECT_DELAY).await;
                    continue;
                }

                loop {
                    let next = tokio::time::timeout(STREAM_IDLE_TIMEOUT, socket.next()).await;
                    let maybe_message_result = match next {
                        Ok(v) => v,
                        Err(_) => {
                            warn!("Base pendingLogs stream timeout, reconnecting");
                            break;
                        }
                    };

                    let Some(message_result) = maybe_message_result else {
                        warn!("Base pendingLogs stream ended");
                        break;
                    };

                    let message = match message_result {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("Base pendingLogs stream receive error: {}", e);
                            break;
                        }
                    };

                    let received_at = Instant::now();
                    let payload = match message {
                        Message::Text(text) => text.to_string(),
                        Message::Binary(bin) => String::from_utf8_lossy(bin.as_ref()).to_string(),
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
                            warn!("Base pendingLogs log decode failed: {}", e);
                            continue;
                        }
                    };

                    let block_num = match log.block_number {
                        Some(v) => v,
                        None => continue,
                    };

                    // Multiple chunk subscriptions can legitimately overlap,
                    // especially when shared infra contracts are involved.
                    // Dedup here prevents duplicated downstream sync work
                    // before logs reach the global applied-log dedup layer.
                    let prededup_key = PendingLogDedupKey {
                        tx_hash: log.transaction_hash,
                        log_index: log.log_index.unwrap_or_default(),
                        address: log.address(),
                        topic0: log.topics().first().copied(),
                    };
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
                        LogSource::RealtimeFlashblock,
                    )
                    .await
                    {
                        Ok(affected) => {
                            if !affected.is_empty() {
                                let meta = super::build_realtime_update_meta(
                                    &update_seq,
                                    block_num,
                                    LogSource::RealtimeFlashblock,
                                    received_at,
                                );
                                super::log_realtime_update_applied(meta, affected.len(), 1);
                                yield Ok((meta, affected));
                            }
                        }
                        Err(e) => {
                            error!("Base pendingLogs batch process failed: {}", e);
                        }
                    }
                }

                sleep(STREAM_RECONNECT_DELAY).await;
            }
        }
    }
}

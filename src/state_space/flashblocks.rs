use super::{
    AppliedLogDedupCache, HookRegistry, LogQueryChunk, LogSource, PendingSyncQueue, QueryMode,
    StateSpace, StateSpaceError, StateSpaceManager,
};
use crate::state_space::{
    BASE_FLASHBLOCKS_RAW_WS_URL, STREAM_IDLE_TIMEOUT, STREAM_RECONNECT_DELAY,
};
use alloy::network::Network;
use alloy::primitives::{Address, Bytes, FixedBytes, LogData, B256};
use alloy::providers::Provider;
use alloy::rpc::types::eth::Log;
use async_stream::stream;
use futures::{SinkExt, Stream, StreamExt};
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{error, info, warn};

const FLASHBLOCKS_DEDUP_PAYLOAD_WINDOW: usize = 4;
const FLASHBLOCKS_HEX_CACHE_MAX: usize = 8192;
const FLASHBLOCKS_BROTLI_READER_BUF_SIZE: usize = 64 * 1024;
const BASE_RECONCILE_CHUNK_BLOCKS: u64 = 100;

#[derive(Clone, Debug)]
struct RawLogMatcher {
    topic_addresses: HashSet<Address>,
    topic_signatures: HashSet<FixedBytes<32>>,
    address_only_addresses: HashSet<Address>,
}

impl RawLogMatcher {
    fn from_query_chunks(chunks: &[LogQueryChunk]) -> Self {
        let mut topic_addresses = HashSet::new();
        let mut topic_signatures = HashSet::new();
        let mut address_only_addresses = HashSet::new();

        for chunk in chunks {
            match &chunk.mode {
                QueryMode::TopicFiltered(topics) => {
                    for addr in &chunk.addresses {
                        topic_addresses.insert(*addr);
                    }
                    for topic in topics {
                        topic_signatures.insert(*topic);
                    }
                }
                QueryMode::AddressOnly => {
                    for addr in &chunk.addresses {
                        address_only_addresses.insert(*addr);
                    }
                }
            }
        }

        Self {
            topic_addresses,
            topic_signatures,
            address_only_addresses,
        }
    }
}

#[derive(Debug, Deserialize)]
struct FlashblockMessage<'a> {
    #[serde(borrow)]
    payload_id: Cow<'a, str>,
    index: u64,
    #[serde(default)]
    base: Option<FlashblockBase<'a>>,
    #[serde(default)]
    diff: Option<FlashblockDiff<'a>>,
    #[serde(default)]
    metadata: Option<FlashblockMetadata<'a>>,
}

#[derive(Debug, Deserialize)]
struct FlashblockDiff<'a> {
    #[serde(default, borrow)]
    transactions: Vec<Cow<'a, str>>,
}

#[derive(Debug, Deserialize)]
struct FlashblockBase<'a> {
    #[serde(default, borrow)]
    block_number: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    timestamp: Option<Cow<'a, str>>,
}

#[derive(Debug, Deserialize)]
struct FlashblockMetadata<'a> {
    #[serde(default)]
    block_number: Option<u64>,
    #[serde(default, borrow)]
    receipts: HashMap<Cow<'a, str>, FlashblockReceipt<'a>>,
}

#[derive(Debug, Deserialize)]
struct FlashblockReceipt<'a> {
    #[serde(default, rename = "transactionIndex", borrow)]
    transaction_index: Option<Cow<'a, str>>,
    #[serde(default)]
    logs: Vec<FlashblockLog<'a>>,
}

#[derive(Debug, Deserialize)]
struct FlashblockLog<'a> {
    #[serde(borrow)]
    address: Cow<'a, str>,
    #[serde(default, borrow)]
    topics: Vec<Cow<'a, str>>,
    #[serde(default, borrow)]
    data: Cow<'a, str>,
}

#[derive(Debug, Default)]
struct FlashblocksParseCache {
    address: HashMap<String, Address>,
    topic: HashMap<String, B256>,
}

impl FlashblocksParseCache {
    fn parse_address(&mut self, value: &str) -> Option<Address> {
        if let Some(v) = self.address.get(value) {
            return Some(*v);
        }

        let parsed = Address::from_str(value).ok()?;
        if self.address.len() < FLASHBLOCKS_HEX_CACHE_MAX {
            self.address.insert(value.to_string(), parsed);
        }
        Some(parsed)
    }

    fn parse_topic(&mut self, value: &str) -> Option<B256> {
        if let Some(v) = self.topic.get(value) {
            return Some(*v);
        }

        let parsed = B256::from_str(value).ok()?;
        if self.topic.len() < FLASHBLOCKS_HEX_CACHE_MAX {
            self.topic.insert(value.to_string(), parsed);
        }
        Some(parsed)
    }
}

#[derive(Debug)]
struct FlashblocksDedupCache {
    // [BugFix]: Switched from (tx_index, log_index) to (tx_hash, log_index)
    // Flashblock chunks reuse local indices (0, 1, 2) since absolute index is missing.
    // This caused legitimate logs in subsequent slices to be flagged as duplicates and dropped.
    // Using the absolute tx_hash permanently resolves collisons and prevents data loss.
    per_payload: HashMap<String, HashSet<(String, u64)>>,
    order: VecDeque<String>,
    max_payloads: usize,
}

impl FlashblocksDedupCache {
    fn new(max_payloads: usize) -> Self {
        Self {
            per_payload: HashMap::new(),
            order: VecDeque::new(),
            max_payloads,
        }
    }

    fn insert(&mut self, payload_id: &str, tx_hash: &str, log_index: u64) -> bool {
        if !self.per_payload.contains_key(payload_id) {
            self.per_payload
                .insert(payload_id.to_string(), HashSet::new());
            self.order.push_back(payload_id.to_string());

            while self.order.len() > self.max_payloads {
                if let Some(evicted) = self.order.pop_front() {
                    self.per_payload.remove(&evicted);
                }
            }
        }

        match self.per_payload.get_mut(payload_id) {
            Some(seen) => seen.insert((tx_hash.to_string(), log_index)),
            None => false,
        }
    }
}

impl<N, P> StateSpaceManager<N, P> {
    fn parse_hex_u64(value: &str) -> Option<u64> {
        let raw = value.strip_prefix("0x").unwrap_or(value);
        u64::from_str_radix(raw, 16).ok()
    }

    fn decode_flashblock_text<'a>(raw: &'a mut [u8]) -> Option<FlashblockMessage<'a>> {
        simd_json::from_slice::<FlashblockMessage<'a>>(raw).ok()
    }

    fn decode_flashblock_binary<'a>(
        raw: &[u8],
        decompressed: &'a mut Vec<u8>,
    ) -> Option<FlashblockMessage<'a>> {
        decompressed.clear();
        let target_capacity = raw.len().saturating_mul(6);
        if decompressed.capacity() < target_capacity {
            decompressed.reserve(target_capacity - decompressed.capacity());
        }

        let mut reader = brotli::Decompressor::new(raw, FLASHBLOCKS_BROTLI_READER_BUF_SIZE);
        if reader.read_to_end(decompressed).is_err() {
            return None;
        }

        simd_json::from_slice::<FlashblockMessage<'a>>(decompressed).ok()
    }

    fn extract_logs_from_flashblock(
        fb: &FlashblockMessage,
        matcher: &RawLogMatcher,
        dedup_cache: &mut FlashblocksDedupCache,
        parse_cache: &mut FlashblocksParseCache,
        latest_block_timestamp: &mut Option<(u64, u64)>,
    ) -> (Vec<Log>, Option<u64>, usize, HashSet<Address>) {
        let mut out = Vec::new();
        let mut decode_fail = 0usize;
        let mut decode_failed_addresses = HashSet::new();

        let block_number = fb
            .metadata
            .as_ref()
            .and_then(|m| m.block_number)
            .or_else(|| {
                fb.base
                    .as_ref()
                    .and_then(|base| base.block_number.as_deref())
                    .and_then(Self::parse_hex_u64)
            });

        let Some(block_number) = block_number else {
            return (out, None, decode_fail, decode_failed_addresses);
        };

        // Extract block timestamp from base.timestamp when present (typically index-0),
        // then reuse/derive a block-level timestamp for following slices.
        let base_timestamp = fb
            .base
            .as_ref()
            .and_then(|base| base.timestamp.as_deref())
            .and_then(Self::parse_hex_u64);
        let block_timestamp = if let Some(ts) = base_timestamp {
            *latest_block_timestamp = Some((block_number, ts));
            Some(ts)
        } else {
            match *latest_block_timestamp {
                Some((known_block, known_ts)) if block_number == known_block => Some(known_ts),
                Some((known_block, known_ts)) if block_number > known_block => {
                    // Fallback: when a newer flashblock arrives without base.timestamp,
                    // estimate from the last known block timestamp at +2s per block.
                    let delta_blocks = block_number - known_block;
                    let estimated_ts = known_ts.saturating_add(delta_blocks.saturating_mul(2));
                    *latest_block_timestamp = Some((block_number, estimated_ts));
                    Some(estimated_ts)
                }
                _ => None,
            }
        };

        let Some(metadata) = fb.metadata.as_ref() else {
            return (
                out,
                Some(block_number),
                decode_fail,
                decode_failed_addresses,
            );
        };

        for (fallback_tx_idx, (tx_hash, receipt)) in metadata.receipts.iter().enumerate() {
            let transaction_index = receipt
                .transaction_index
                .as_deref()
                .and_then(Self::parse_hex_u64)
                .unwrap_or(fallback_tx_idx as u64);

            for (log_idx, raw_log) in receipt.logs.iter().enumerate() {
                if !dedup_cache.insert(&fb.payload_id, tx_hash, log_idx as u64) {
                    continue;
                }

                let Some(address) = parse_cache.parse_address(&raw_log.address) else {
                    decode_fail += 1;
                    continue;
                };

                let is_address_only = matcher.address_only_addresses.contains(&address);
                let is_topic_candidate = matcher.topic_addresses.contains(&address);
                if !is_address_only && !is_topic_candidate {
                    continue;
                }

                if raw_log.topics.len() > 4 {
                    decode_fail += 1;
                    decode_failed_addresses.insert(address);
                    continue;
                }

                let mut topics = Vec::with_capacity(raw_log.topics.len());
                if is_topic_candidate {
                    let Some(topic0_raw) = raw_log.topics.first() else {
                        continue;
                    };
                    let Some(topic0) = parse_cache.parse_topic(topic0_raw) else {
                        decode_fail += 1;
                        decode_failed_addresses.insert(address);
                        continue;
                    };
                    if !matcher.topic_signatures.contains(&topic0) {
                        continue;
                    }
                    topics.push(topic0);

                    let mut topic_decode_failed = false;
                    for topic in raw_log.topics.iter().skip(1) {
                        match parse_cache.parse_topic(topic) {
                            Some(parsed) => topics.push(parsed),
                            None => {
                                topic_decode_failed = true;
                                break;
                            }
                        }
                    }

                    if topic_decode_failed {
                        decode_fail += 1;
                        decode_failed_addresses.insert(address);
                        continue;
                    }
                } else {
                    let mut topic_decode_failed = false;
                    for topic in &raw_log.topics {
                        match parse_cache.parse_topic(topic) {
                            Some(parsed) => topics.push(parsed),
                            None => {
                                topic_decode_failed = true;
                                break;
                            }
                        }
                    }
                    if topic_decode_failed {
                        decode_fail += 1;
                        decode_failed_addresses.insert(address);
                        continue;
                    }
                }

                let data = match Bytes::from_str(&raw_log.data) {
                    Ok(v) => v,
                    Err(_) => {
                        decode_fail += 1;
                        decode_failed_addresses.insert(address);
                        continue;
                    }
                };

                let Some(log_data) = LogData::new(topics, data) else {
                    decode_fail += 1;
                    decode_failed_addresses.insert(address);
                    continue;
                };

                let tx_hash_parsed = tx_hash.parse::<alloy::primitives::B256>().ok();

                let log = Log {
                    inner: alloy::primitives::Log {
                        address,
                        data: log_data,
                    },
                    block_hash: None,
                    block_number: Some(block_number),
                    block_timestamp,
                    transaction_hash: tx_hash_parsed,
                    transaction_index: Some(transaction_index),
                    log_index: Some(log_idx as u64),
                    removed: false,
                };

                out.push(log);
            }
        }

        // [Optimization/Fix]: Lazy Evaluation for absolute transactionIndex
        // Because metadata.receipts is an unordered HashMap, if 2 distinct transactions interact
        // with the EXACT SAME AMM pool within the SAME 200ms window, applying their events in the
        // wrong local order will cause irreversible state rollback (e.g. outdated sqrtPriceX96).
        // To maintain 0ms overhead for 99% of normal blocks, we natively ONLY trigger the intensive
        // Keccak256 RLP hashing (to find true transaction order) if a collision perfectly strikes.
        let mut pool_collision_detector: HashMap<Address, HashSet<alloy::primitives::B256>> =
            HashMap::new();
        let mut needs_lazy_evaluation = false;

        for log in &out {
            if let (Some(hash), address) = (log.transaction_hash, log.inner.address) {
                let interacting_txs = pool_collision_detector.entry(address).or_default();
                interacting_txs.insert(hash);
                // Trigger IF AND ONLY IF >= 2 differing transactions touch the same pool
                if interacting_txs.len() >= 2 {
                    needs_lazy_evaluation = true;
                    break;
                }
            }
        }

        if needs_lazy_evaluation {
            let mut real_tx_index_map = HashMap::new();
            if let Some(diff) = fb.diff.as_ref() {
                for (real_idx, raw_tx_hex) in diff.transactions.iter().enumerate() {
                    let raw_hex = raw_tx_hex.strip_prefix("0x").unwrap_or(raw_tx_hex);
                    if let Ok(raw_bytes) = alloy::hex::decode(raw_hex) {
                        let exact_hash = alloy::primitives::keccak256(&raw_bytes);
                        real_tx_index_map.insert(exact_hash, real_idx as u64);
                    }
                }
            }

            for log in &mut out {
                if let Some(hash) = log.transaction_hash {
                    if let Some(&exact_idx) = real_tx_index_map.get(&hash) {
                        log.transaction_index = Some(exact_idx);
                    }
                }
            }
        }

        (
            out,
            Some(block_number),
            decode_fail,
            decode_failed_addresses,
        )
    }

    pub(super) fn subscribe_flashblocks_raw_stream(
        provider: P,
        state: Arc<RwLock<StateSpace>>,
        hooks: HookRegistry<Vec<Address>>,
        realtime_head: Arc<AtomicU64>,
        canonical_head: Arc<AtomicU64>,
        pending_sync_queue: Arc<Mutex<PendingSyncQueue>>,
        applied_log_dedup: Arc<Mutex<AppliedLogDedupCache>>,
        query_chunks: Vec<LogQueryChunk>,
        chain_id: u64,
    ) -> impl Stream<Item = Result<Vec<Address>, StateSpaceError>> + Send
    where
        P: Provider<N> + Clone + 'static,
        N: Network + 'static,
    {
        let matcher = RawLogMatcher::from_query_chunks(&query_chunks);

        stream! {
            let mut dedup_cache = FlashblocksDedupCache::new(FLASHBLOCKS_DEDUP_PAYLOAD_WINDOW);
            let mut parse_cache = FlashblocksParseCache::default();
            let mut flashblocks_decode_buf = Vec::with_capacity(64 * 1024);
            let mut latest_block_timestamp: Option<(u64, u64)> = None;

            loop {
                match Self::initial_backfill_results(
                    &provider,
                    &state,
                    &hooks,
                    &query_chunks,
                    &realtime_head,
                    &canonical_head,
                    &pending_sync_queue,
                    &applied_log_dedup,
                    LogSource::RealtimeFlashblock,
                    chain_id,
                )
                .await
                {
                    Ok(results) => {
                        // Catch-up stage: apply state updates only; do not emit tradable updates downstream.
                        let mut non_empty_batches = 0usize;
                        let mut affected_pools = 0usize;
                        for affected in results {
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
                        warn!("Initial backfill failed before Flashblocks subscribe: {}", e);
                    }
                }

                let connect = connect_async(BASE_FLASHBLOCKS_RAW_WS_URL).await;
                let (mut socket, _) = match connect {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Flashblocks ws connect failed: {}", e);
                        sleep(STREAM_RECONNECT_DELAY).await;
                        continue;
                    }
                };

                loop {
                    let next = tokio::time::timeout(STREAM_IDLE_TIMEOUT, socket.next()).await;
                    let maybe_message_result = match next {
                        Ok(v) => v,
                        Err(_) => {
                            warn!("Flashblocks raw stream timeout, reconnecting");
                            break;
                        }
                    };

                    let Some(message_result) = maybe_message_result else {
                        warn!("Flashblocks raw stream ended");
                        break;
                    };

                    let message = match message_result {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("Flashblocks raw stream receive error: {}", e);
                            break;
                        }
                    };

                    let fb = match message {
                        Message::Text(text) => {
                            flashblocks_decode_buf.clear();
                            flashblocks_decode_buf.extend_from_slice(text.as_bytes());
                            Self::decode_flashblock_text(&mut flashblocks_decode_buf)
                        }
                        Message::Binary(bin) => {
                            Self::decode_flashblock_binary(bin.as_ref(), &mut flashblocks_decode_buf)
                        }
                        Message::Ping(v) => {
                            let _ = socket.send(Message::Pong(v)).await;
                            continue;
                        }
                        Message::Pong(_) => continue,
                        Message::Close(_) => break,
                        Message::Frame(_) => continue,
                    };

                    let Some(fb) = fb else {
                        continue;
                    };

                    let (logs, block_number, decode_fail, decode_failed_addresses) = Self::extract_logs_from_flashblock(
                        &fb,
                        &matcher,
                        &mut dedup_cache,
                        &mut parse_cache,
                        &mut latest_block_timestamp,
                    );
                    let block_num =
                        block_number.unwrap_or_else(|| realtime_head.load(Ordering::Relaxed));

                    if decode_fail > 0 {
                        warn!(
                            "Flashblocks log decode failures: payload_id={} index={} count={}",
                            fb.payload_id,
                            fb.index,
                            decode_fail
                        );

                        if !decode_failed_addresses.is_empty() {
                            let resolvable: Vec<Address> = {
                                let guard = state.read().await;
                                decode_failed_addresses
                                    .into_iter()
                                    .filter(|addr| guard.state.contains_key(addr))
                                    .collect()
                            };
                            if !resolvable.is_empty() {
                                let mut queue = pending_sync_queue.lock().await;
                                for address in resolvable {
                                    queue.enqueue(
                                        address,
                                        super::PendingSyncAction::Resync,
                                        block_num,
                                        super::PendingSyncReason::SyncError,
                                    );
                                }
                            }
                        }
                    }

                    if logs.is_empty() {
                        continue;
                    }

                    match Self::apply_logs_for_block(
                        &provider,
                        &state,
                        &hooks,
                        block_num,
                        logs,
                        &realtime_head,
                        &canonical_head,
                        &pending_sync_queue,
                        &applied_log_dedup,
                        LogSource::RealtimeFlashblock,
                    )
                    .await
                    {
                        Ok(affected) => {
                            if !affected.is_empty() {
                                yield Ok(affected);
                            }
                        }
                        Err(e) => {
                            error!("Flashblocks raw batch process failed: {}", e);
                        }
                    }
                }

                sleep(STREAM_RECONNECT_DELAY).await;
            }
        }
    }

    pub(super) async fn run_reconcile_worker(
        provider: P,
        state: Arc<RwLock<StateSpace>>,
        hooks: HookRegistry<Vec<Address>>,
        chunks: Vec<LogQueryChunk>,
        realtime_head: Arc<AtomicU64>,
        canonical_head: Arc<AtomicU64>,
        reconcile_cursor: Arc<AtomicU64>,
        pending_sync_queue: Arc<Mutex<PendingSyncQueue>>,
        applied_log_dedup: Arc<Mutex<AppliedLogDedupCache>>,
        chain_id: u64,
    ) where
        P: Provider<N> + Clone + 'static,
        N: Network + 'static,
    {
        loop {
            let canonical = canonical_head.load(Ordering::Relaxed);
            let cursor = reconcile_cursor.load(Ordering::Relaxed);

            if canonical == 0 || cursor >= canonical {
                sleep(Duration::from_secs(1)).await;
                continue;
            }

            let start = cursor.saturating_add(1);
            let end = (start + BASE_RECONCILE_CHUNK_BLOCKS - 1).min(canonical);

            match Self::backfill_range(
                &provider,
                &state,
                &hooks,
                &chunks,
                start,
                end,
                &realtime_head,
                &canonical_head,
                &pending_sync_queue,
                &applied_log_dedup,
                LogSource::CanonicalReconcile,
                chain_id,
            )
            .await
            {
                Ok(_) => {
                    Self::store_monotonic_head(&reconcile_cursor, end);
                    let guard = state.read().await;
                    Self::store_monotonic_head(&guard.reconcile_cursor, end);
                }
                Err(e) => {
                    warn!(
                        from_block = start,
                        to_block = end,
                        "Canonical reconcile range failed: {}",
                        e
                    );
                    sleep(STREAM_RECONNECT_DELAY).await;
                }
            }
        }
    }
}

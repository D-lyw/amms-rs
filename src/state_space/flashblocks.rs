use super::{
    HookRegistry, LogQueryChunk, QueryMode, StateSpace, StateSpaceError, StateSpaceManager,
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
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{error, warn};

const FLASHBLOCKS_DEDUP_PAYLOAD_WINDOW: usize = 4;
const FLASHBLOCKS_HEX_CACHE_MAX: usize = 8192;
const FLASHBLOCKS_BROTLI_READER_BUF_SIZE: usize = 64 * 1024;

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
struct FlashblockMessage {
    payload_id: String,
    index: u64,
    #[serde(default)]
    base: Option<FlashblockBase>,
    #[serde(default)]
    metadata: Option<FlashblockMetadata>,
}

#[derive(Debug, Deserialize)]
struct FlashblockBase {
    #[serde(default)]
    block_number: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FlashblockMetadata {
    #[serde(default)]
    block_number: Option<u64>,
    #[serde(default)]
    receipts: HashMap<String, FlashblockReceipt>,
}

#[derive(Debug, Deserialize)]
struct FlashblockReceipt {
    #[serde(default, rename = "transactionIndex")]
    transaction_index: Option<String>,
    #[serde(default)]
    logs: Vec<FlashblockLog>,
}

#[derive(Debug, Deserialize)]
struct FlashblockLog {
    address: String,
    #[serde(default)]
    topics: Vec<String>,
    #[serde(default)]
    data: String,
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
    per_payload: HashMap<String, HashSet<(u64, u64)>>,
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

    fn insert(&mut self, payload_id: &str, tx_index: u64, log_index: u64) -> bool {
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
            Some(seen) => seen.insert((tx_index, log_index)),
            None => false,
        }
    }
}

impl<N, P> StateSpaceManager<N, P> {
    fn parse_hex_u64(value: &str) -> Option<u64> {
        let raw = value.strip_prefix("0x").unwrap_or(value);
        u64::from_str_radix(raw, 16).ok()
    }

    fn decode_flashblock_text(raw: &str) -> Option<FlashblockMessage> {
        serde_json::from_str::<FlashblockMessage>(raw).ok()
    }

    fn decode_flashblock_binary(
        raw: &[u8],
        decompressed: &mut Vec<u8>,
    ) -> Option<FlashblockMessage> {
        decompressed.clear();
        let target_capacity = raw.len().saturating_mul(6);
        if decompressed.capacity() < target_capacity {
            decompressed.reserve(target_capacity - decompressed.capacity());
        }

        let mut reader = brotli::Decompressor::new(raw, FLASHBLOCKS_BROTLI_READER_BUF_SIZE);
        if reader.read_to_end(decompressed).is_err() {
            return None;
        }

        serde_json::from_slice::<FlashblockMessage>(decompressed).ok()
    }

    fn extract_logs_from_flashblock(
        fb: &FlashblockMessage,
        matcher: &RawLogMatcher,
        dedup_cache: &mut FlashblocksDedupCache,
        parse_cache: &mut FlashblocksParseCache,
    ) -> (Vec<Log>, Option<u64>, usize) {
        let mut out = Vec::new();
        let mut decode_fail = 0usize;

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
            return (out, None, decode_fail);
        };

        let Some(metadata) = fb.metadata.as_ref() else {
            return (out, Some(block_number), decode_fail);
        };

        for (fallback_tx_idx, (_tx_hash, receipt)) in metadata.receipts.iter().enumerate() {
            let transaction_index = receipt
                .transaction_index
                .as_deref()
                .and_then(Self::parse_hex_u64)
                .unwrap_or(fallback_tx_idx as u64);

            for (log_idx, raw_log) in receipt.logs.iter().enumerate() {
                if !dedup_cache.insert(&fb.payload_id, transaction_index, log_idx as u64) {
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
                    continue;
                }

                let mut topics = Vec::with_capacity(raw_log.topics.len());
                if is_topic_candidate {
                    let Some(topic0_raw) = raw_log.topics.first() else {
                        continue;
                    };
                    let Some(topic0) = parse_cache.parse_topic(topic0_raw) else {
                        decode_fail += 1;
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
                        continue;
                    }
                }

                let data = match Bytes::from_str(&raw_log.data) {
                    Ok(v) => v,
                    Err(_) => {
                        decode_fail += 1;
                        continue;
                    }
                };

                let Some(log_data) = LogData::new(topics, data) else {
                    decode_fail += 1;
                    continue;
                };

                let log = Log {
                    inner: alloy::primitives::Log {
                        address,
                        data: log_data,
                    },
                    block_hash: None,
                    block_number: Some(block_number),
                    block_timestamp: None,
                    transaction_hash: None,
                    transaction_index: Some(transaction_index),
                    log_index: Some(log_idx as u64),
                    removed: false,
                };

                out.push(log);
            }
        }

        (out, Some(block_number), decode_fail)
    }

    pub(super) fn subscribe_flashblocks_raw_stream(
        provider: P,
        state: Arc<RwLock<StateSpace>>,
        hooks: HookRegistry<Vec<Address>>,
        latest_block: Arc<AtomicU64>,
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
                        Message::Text(text) => Self::decode_flashblock_text(text.as_ref()),
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

                    let (logs, block_number, decode_fail) = Self::extract_logs_from_flashblock(
                        &fb,
                        &matcher,
                        &mut dedup_cache,
                        &mut parse_cache,
                    );
                    if decode_fail > 0 {
                        warn!(
                            "Flashblocks log decode failures: payload_id={} index={} count={}",
                            fb.payload_id,
                            fb.index,
                            decode_fail
                        );
                    }

                    if logs.is_empty() {
                        continue;
                    }

                    let block_num =
                        block_number.unwrap_or_else(|| latest_block.load(Ordering::Relaxed));

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
                            error!("Flashblocks raw batch process failed: {}", e);
                        }
                    }
                }

                sleep(STREAM_RECONNECT_DELAY).await;
            }
        }
    }
}

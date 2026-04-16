pub mod cache;
pub mod discovery;
pub mod error;
pub mod filters;
pub mod hooks;
pub mod sync_services;

use crate::amms::amm::AutomatedMarketMaker;
use crate::amms::amm::{SyncAction, AMM};
use crate::amms::error::AMMError;
use crate::amms::factory::Factory;
use crate::amms::fluid_dex::get_liquidity_layer;
use crate::amms::{
    aerodrome_slipstream::{ICustomFeeModule, BASE_SLIPSTREAM_FACTORY},
    balancer_v2, balancer_v3, ekubo,
};
use crate::state_space::hooks::HookHandle;
use crate::state_space::hooks::HookRegistry;
use crate::state_space::hooks::SnapshotConfig;
use crate::state_space::hooks::StateHook;

use alloy::eips::BlockId;
use alloy::network::Network;

use alloy::primitives::{Address, Bloom, BloomInput, Bytes, FixedBytes, LogData, B256};
use alloy::providers::Provider;
use alloy::rpc::types::{eth::Log, Filter, FilterSet};
use alloy::sol;
use alloy::sol_types::SolEvent;
use async_stream::stream;
use cache::StateChange;
use cache::StateChangeCache;

use error::StateSpaceError;
use filters::AMMFilter;
use filters::PoolFilter;
use futures::stream::FuturesUnordered;
use futures::{SinkExt, Stream, StreamExt};
use serde::Deserialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Debug;
use std::io::Read;
use std::path::PathBuf;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;
use std::{future::Future, marker::PhantomData, sync::Arc};
use tokio::sync::{mpsc, RwLock};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

pub const CACHE_SIZE: usize = 100;

#[derive(Clone, Debug, Default)]
pub enum RealtimeSyncSource {
    #[default]
    Auto,
    WsLogs,
    BaseFlashblocksRaw,
}

#[derive(Clone)]
pub struct StateSpaceManager<N, P> {
    pub state: Arc<RwLock<StateSpace>>,
    pub block_filter: Filter,
    pub provider: P,
    pub latest_block: Arc<AtomicU64>,
    realtime_source: RealtimeSyncSource,
    hooks: HookRegistry<Vec<Address>>,
    phantom: PhantomData<N>,
}

const LOG_ADDRESS_CHUNK_SIZE: usize = 200;
const BASE_CHAIN_ID: u64 = 8453;
const ARBITRUM_CHAIN_ID: u64 = 42161;
const ETHEREUM_MAINNET_CHAIN_ID: u64 = 1;
const BASE_FLASHBLOCKS_RAW_WS_URL: &str = "wss://mainnet.flashblocks.base.org/ws";
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const STREAM_RECONNECT_DELAY: Duration = Duration::from_secs(2);
const FLASHBLOCKS_DEDUP_PAYLOAD_WINDOW: usize = 4;
const FLASHBLOCKS_HEX_CACHE_MAX: usize = 8192;
const FLASHBLOCKS_PERF_LOG_EVERY_MESSAGES: u64 = 200;
const FLASHBLOCKS_PERF_MAX_SAMPLES: usize = 200_000;

sol! {
    #[derive(Debug)]
    #[sol(rpc)]
    interface ICLFactoryReader {
        function swapFeeModule() external view returns (address);
    }
}

#[derive(Clone, Debug)]
enum QueryMode {
    TopicFiltered(Vec<FixedBytes<32>>),
    AddressOnly,
}

#[derive(Clone, Debug)]
struct LogQueryChunk {
    addresses: Vec<Address>,
    mode: QueryMode,
}

impl LogQueryChunk {
    fn ranged_filter(&self, from_block: u64, to_block: u64) -> Filter {
        let mut filter = Filter::new()
            .address(self.addresses.clone())
            .from_block(from_block)
            .to_block(to_block);

        if let QueryMode::TopicFiltered(topics) = &self.mode {
            if !topics.is_empty() {
                filter = filter.event_signature(topics.clone());
            }
        }

        filter
    }

    fn subscription_filter(&self) -> Filter {
        let mut filter = Filter::new().address(self.addresses.clone());

        if let QueryMode::TopicFiltered(topics) = &self.mode {
            if !topics.is_empty() {
                filter = filter.event_signature(topics.clone());
            }
        }

        filter
    }
}

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

#[derive(Default)]
struct FlashblockExtractStats {
    decode_fail: usize,
    total_logs: usize,
    matched_logs: usize,
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

#[derive(Debug, Default)]
struct FlashblocksPerfStats {
    enabled: bool,
    messages_total: u64,
    messages_binary: u64,
    messages_text: u64,
    decode_fail_messages: u64,
    log_decode_fail_total: u64,
    raw_logs_total: u64,
    matched_logs_total: u64,
    sync_batches_total: u64,
    affected_total: u64,
    decode_ns_total: u128,
    extract_ns_total: u128,
    sync_ns_total: u128,
    decode_ns_max: u128,
    extract_ns_max: u128,
    sync_ns_max: u128,
    decode_samples_ns: Vec<u64>,
    extract_samples_ns: Vec<u64>,
    sync_samples_ns: Vec<u64>,
}

impl FlashblocksPerfStats {
    fn from_env() -> Self {
        Self {
            enabled: std::env::var("AMMS_FLASHBLOCKS_PERF")
                .ok()
                .map(|v| {
                    let lower = v.to_ascii_lowercase();
                    lower == "1" || lower == "true" || lower == "yes" || lower == "on"
                })
                .unwrap_or(false),
            ..Default::default()
        }
    }

    fn on_message_kind(&mut self, is_binary: bool) {
        if !self.enabled {
            return;
        }
        self.messages_total += 1;
        if is_binary {
            self.messages_binary += 1;
        } else {
            self.messages_text += 1;
        }
    }

    fn on_decode_stage(&mut self, elapsed_ns: u128, success: bool) {
        if !self.enabled {
            return;
        }
        self.decode_ns_total += elapsed_ns;
        self.decode_ns_max = self.decode_ns_max.max(elapsed_ns);
        if self.decode_samples_ns.len() < FLASHBLOCKS_PERF_MAX_SAMPLES {
            self.decode_samples_ns.push(elapsed_ns as u64);
        }
        if !success {
            self.decode_fail_messages += 1;
        }
    }

    fn on_extract_stage(&mut self, elapsed_ns: u128, stats: &FlashblockExtractStats) {
        if !self.enabled {
            return;
        }
        self.extract_ns_total += elapsed_ns;
        self.extract_ns_max = self.extract_ns_max.max(elapsed_ns);
        if self.extract_samples_ns.len() < FLASHBLOCKS_PERF_MAX_SAMPLES {
            self.extract_samples_ns.push(elapsed_ns as u64);
        }
        self.log_decode_fail_total += stats.decode_fail as u64;
        self.raw_logs_total += stats.total_logs as u64;
        self.matched_logs_total += stats.matched_logs as u64;
    }

    fn on_sync_stage(&mut self, elapsed_ns: u128, affected_count: usize) {
        if !self.enabled {
            return;
        }
        self.sync_ns_total += elapsed_ns;
        self.sync_ns_max = self.sync_ns_max.max(elapsed_ns);
        if self.sync_samples_ns.len() < FLASHBLOCKS_PERF_MAX_SAMPLES {
            self.sync_samples_ns.push(elapsed_ns as u64);
        }
        self.sync_batches_total += 1;
        self.affected_total += affected_count as u64;
    }

    fn percentile_ns(values: &[u64], p: f64) -> u64 {
        if values.is_empty() {
            return 0;
        }
        let mut sorted = values.to_vec();
        sorted.sort_unstable();
        let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
        sorted[idx]
    }

    fn maybe_log(&self) {
        if !self.enabled {
            return;
        }

        if self.messages_total == 0
            || self.messages_total % FLASHBLOCKS_PERF_LOG_EVERY_MESSAGES != 0
        {
            return;
        }

        let decode_avg_us = self.decode_ns_total as f64 / self.messages_total as f64 / 1_000.0;
        let extract_avg_us = self.extract_ns_total as f64 / self.messages_total as f64 / 1_000.0;
        let sync_avg_us = if self.sync_batches_total > 0 {
            self.sync_ns_total as f64 / self.sync_batches_total as f64 / 1_000.0
        } else {
            0.0
        };
        let decode_p50_us = Self::percentile_ns(&self.decode_samples_ns, 0.50) as f64 / 1_000.0;
        let decode_p95_us = Self::percentile_ns(&self.decode_samples_ns, 0.95) as f64 / 1_000.0;
        let extract_p50_us = Self::percentile_ns(&self.extract_samples_ns, 0.50) as f64 / 1_000.0;
        let extract_p95_us = Self::percentile_ns(&self.extract_samples_ns, 0.95) as f64 / 1_000.0;
        let sync_p50_us = Self::percentile_ns(&self.sync_samples_ns, 0.50) as f64 / 1_000.0;
        let sync_p95_us = Self::percentile_ns(&self.sync_samples_ns, 0.95) as f64 / 1_000.0;

        let stage_total = self.decode_ns_total + self.extract_ns_total + self.sync_ns_total;
        let decode_share = if stage_total > 0 {
            (self.decode_ns_total as f64 / stage_total as f64) * 100.0
        } else {
            0.0
        };
        let extract_share = if stage_total > 0 {
            (self.extract_ns_total as f64 / stage_total as f64) * 100.0
        } else {
            0.0
        };
        let sync_share = if stage_total > 0 {
            (self.sync_ns_total as f64 / stage_total as f64) * 100.0
        } else {
            0.0
        };

        info!(
            "Flashblocks perf: msgs={} (bin={}, text={}) decode_fail={} raw_logs={} matched_logs={} batches={} affected={} avg_us[decode={:.1}, extract={:.1}, sync={:.1}] p50_us[decode={:.1}, extract={:.1}, sync={:.1}] p95_us[decode={:.1}, extract={:.1}, sync={:.1}] max_us[decode={:.1}, extract={:.1}, sync={:.1}] stage_share[decode={:.1}%, extract={:.1}%, sync={:.1}%]",
            self.messages_total,
            self.messages_binary,
            self.messages_text,
            self.decode_fail_messages,
            self.raw_logs_total,
            self.matched_logs_total,
            self.sync_batches_total,
            self.affected_total,
            decode_avg_us,
            extract_avg_us,
            sync_avg_us,
            decode_p50_us,
            extract_p50_us,
            sync_p50_us,
            decode_p95_us,
            extract_p95_us,
            sync_p95_us,
            self.decode_ns_max as f64 / 1_000.0,
            self.extract_ns_max as f64 / 1_000.0,
            self.sync_ns_max as f64 / 1_000.0,
            decode_share,
            extract_share,
            sync_share,
        );
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

#[derive(Clone, Debug)]
enum SelectedRealtimeSource {
    WsLogs,
    BaseFlashblocksRaw,
}

impl<N, P> StateSpaceManager<N, P> {
    /// Registers a hook to be called on every state change.
    pub async fn register_hook(&self, hook: StateHook<Vec<Address>>) -> HookHandle<Vec<Address>> {
        self.hooks.register(hook).await
    }

    /// Subscribes to AMM state changes through a configurable realtime source:
    /// - Base: Flashblocks infrastructure stream by default.
    /// - Other chains: standard ws logs subscription.
    pub async fn subscribe(
        &self,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<Vec<Address>, StateSpaceError>> + Send>>,
        StateSpaceError,
    >
    where
        P: Provider<N> + Clone + 'static,
        N: Network,
    {
        let provider = self.provider.clone();
        let latest_block = self.latest_block.clone();
        let state = self.state.clone();
        let hooks = self.hooks.clone();
        let realtime_source = self.realtime_source.clone();

        let chain_id = { state.read().await.chain_id };
        let query_chunks = Self::build_query_chunks(&provider, &state, chain_id).await;

        let selected = Self::resolve_realtime_source(chain_id, &realtime_source);

        match selected {
            SelectedRealtimeSource::WsLogs => {
                info!(
                    "Starting wsLogs sync (chain_id={}, {} query chunks)",
                    chain_id,
                    query_chunks.len()
                );
                Ok(Box::pin(Self::subscribe_ws_logs_stream(
                    provider,
                    state,
                    hooks,
                    latest_block,
                    query_chunks,
                    chain_id,
                )))
            }
            SelectedRealtimeSource::BaseFlashblocksRaw => {
                info!(
                    "Starting Flashblocks raw sync (chain_id={}, ws_url={}, {} query chunks)",
                    chain_id,
                    BASE_FLASHBLOCKS_RAW_WS_URL,
                    query_chunks.len()
                );
                Ok(Box::pin(Self::subscribe_flashblocks_raw_stream(
                    provider,
                    state,
                    hooks,
                    latest_block,
                    query_chunks,
                    chain_id,
                )))
            }
        }
    }

    fn resolve_realtime_source(
        chain_id: u64,
        source: &RealtimeSyncSource,
    ) -> SelectedRealtimeSource {
        match source {
            RealtimeSyncSource::Auto => {
                if chain_id == BASE_CHAIN_ID {
                    SelectedRealtimeSource::BaseFlashblocksRaw
                } else {
                    SelectedRealtimeSource::WsLogs
                }
            }
            RealtimeSyncSource::WsLogs => SelectedRealtimeSource::WsLogs,
            RealtimeSyncSource::BaseFlashblocksRaw => SelectedRealtimeSource::BaseFlashblocksRaw,
        }
    }

    async fn initial_backfill_results(
        provider: &P,
        state: &Arc<RwLock<StateSpace>>,
        hooks: &HookRegistry<Vec<Address>>,
        chunks: &[LogQueryChunk],
        latest_block: &Arc<AtomicU64>,
        chain_id: u64,
    ) -> Result<Vec<Vec<Address>>, StateSpaceError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let current_synced = latest_block.load(Ordering::Relaxed);
        if current_synced == 0 {
            return Ok(vec![]);
        }

        let chain_head = provider.get_block_number().await?;
        if chain_head <= current_synced {
            return Ok(vec![]);
        }

        Self::backfill_range(
            provider,
            state,
            hooks,
            chunks,
            current_synced + 1,
            chain_head,
            latest_block,
            chain_id,
        )
        .await
    }

    fn subscribe_ws_logs_stream(
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

    fn parse_hex_u64(value: &str) -> Option<u64> {
        let raw = value.strip_prefix("0x").unwrap_or(value);
        u64::from_str_radix(raw, 16).ok()
    }

    fn decode_flashblock_message_brotli(
        raw: &[u8],
        decompressed: &mut Vec<u8>,
    ) -> Option<FlashblockMessage> {
        decompressed.clear();
        let target_capacity = raw.len().saturating_mul(6);
        if decompressed.capacity() < target_capacity {
            decompressed.reserve(target_capacity - decompressed.capacity());
        }

        let mut reader = brotli::Decompressor::new(raw, 16 * 1024);
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
    ) -> (Vec<Log>, Option<u64>, FlashblockExtractStats) {
        let mut out = Vec::new();
        let mut stats = FlashblockExtractStats::default();

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
            return (out, None, stats);
        };

        let Some(metadata) = fb.metadata.as_ref() else {
            return (out, Some(block_number), stats);
        };

        for (fallback_tx_idx, (_tx_hash, receipt)) in metadata.receipts.iter().enumerate() {
            let transaction_index = receipt
                .transaction_index
                .as_deref()
                .and_then(Self::parse_hex_u64)
                .unwrap_or(fallback_tx_idx as u64);

            for (log_idx, raw_log) in receipt.logs.iter().enumerate() {
                stats.total_logs += 1;

                if !dedup_cache.insert(&fb.payload_id, transaction_index, log_idx as u64) {
                    continue;
                }

                let Some(address) = parse_cache.parse_address(&raw_log.address) else {
                    stats.decode_fail += 1;
                    continue;
                };

                let is_address_only = matcher.address_only_addresses.contains(&address);
                let is_topic_candidate = matcher.topic_addresses.contains(&address);
                if !is_address_only && !is_topic_candidate {
                    continue;
                }

                if raw_log.topics.len() > 4 {
                    stats.decode_fail += 1;
                    continue;
                }

                let mut topics = Vec::with_capacity(raw_log.topics.len());
                if is_topic_candidate {
                    let Some(topic0_raw) = raw_log.topics.first() else {
                        continue;
                    };
                    let Some(topic0) = parse_cache.parse_topic(topic0_raw) else {
                        stats.decode_fail += 1;
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
                        stats.decode_fail += 1;
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
                        stats.decode_fail += 1;
                        continue;
                    }
                }

                let data = match Bytes::from_str(&raw_log.data) {
                    Ok(v) => v,
                    Err(_) => {
                        stats.decode_fail += 1;
                        continue;
                    }
                };

                let Some(log_data) = LogData::new(topics, data) else {
                    stats.decode_fail += 1;
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

                stats.matched_logs += 1;
                out.push(log);
            }
        }

        (out, Some(block_number), stats)
    }

    fn subscribe_flashblocks_raw_stream(
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
            let mut perf = FlashblocksPerfStats::from_env();
            if perf.enabled {
                info!(
                    "Flashblocks perf stats enabled via AMMS_FLASHBLOCKS_PERF=1 (log every {} messages)",
                    FLASHBLOCKS_PERF_LOG_EVERY_MESSAGES
                );
            }

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

                    let (logs, block_number) = match message {
                        Message::Text(text) => {
                            perf.on_message_kind(false);
                            let decode_started = Instant::now();
                            let fb = match serde_json::from_str::<FlashblockMessage>(&text) {
                                Ok(v) => {
                                    perf.on_decode_stage(decode_started.elapsed().as_nanos(), true);
                                    v
                                }
                                Err(_) => {
                                    perf.on_decode_stage(decode_started.elapsed().as_nanos(), false);
                                    perf.maybe_log();
                                    continue;
                                }
                            };

                            let extract_started = Instant::now();
                            let (logs, block_number, stats) =
                                Self::extract_logs_from_flashblock(
                                    &fb,
                                    &matcher,
                                    &mut dedup_cache,
                                    &mut parse_cache,
                                );
                            perf.on_extract_stage(extract_started.elapsed().as_nanos(), &stats);
                            if stats.decode_fail > 0 {
                                warn!(
                                    "Flashblocks log decode failures: payload_id={} index={} count={}",
                                    fb.payload_id,
                                    fb.index,
                                    stats.decode_fail
                                );
                            }

                            (logs, block_number)
                        }
                        Message::Binary(bin) => {
                            perf.on_message_kind(true);
                            let decode_started = Instant::now();
                            let fb = match Self::decode_flashblock_message_brotli(
                                &bin,
                                &mut flashblocks_decode_buf,
                            ) {
                                Some(v) => {
                                    perf.on_decode_stage(decode_started.elapsed().as_nanos(), true);
                                    v
                                }
                                None => {
                                    perf.on_decode_stage(decode_started.elapsed().as_nanos(), false);
                                    perf.maybe_log();
                                    continue;
                                }
                            };

                            let extract_started = Instant::now();
                            let (logs, block_number, stats) =
                                Self::extract_logs_from_flashblock(
                                    &fb,
                                    &matcher,
                                    &mut dedup_cache,
                                    &mut parse_cache,
                                );
                            perf.on_extract_stage(extract_started.elapsed().as_nanos(), &stats);
                            if stats.decode_fail > 0 {
                                warn!(
                                    "Flashblocks log decode failures: payload_id={} index={} count={}",
                                    fb.payload_id,
                                    fb.index,
                                    stats.decode_fail
                                );
                            }

                            (logs, block_number)
                        }
                        Message::Ping(v) => {
                            let _ = socket.send(Message::Pong(v)).await;
                            continue;
                        }
                        Message::Pong(_) => continue,
                        Message::Close(_) => break,
                        Message::Frame(_) => continue,
                    };

                    if logs.is_empty() {
                        perf.maybe_log();
                        continue;
                    }

                    let block_num = block_number.unwrap_or_else(|| latest_block.load(Ordering::Relaxed));

                    let sync_started = Instant::now();
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
                            perf.on_sync_stage(sync_started.elapsed().as_nanos(), affected.len());
                            perf.maybe_log();
                            if !affected.is_empty() {
                                yield Ok(affected);
                            }
                        }
                        Err(e) => {
                            perf.on_sync_stage(sync_started.elapsed().as_nanos(), 0);
                            perf.maybe_log();
                            error!("Flashblocks raw batch process failed: {}", e);
                        }
                    }
                }

                sleep(STREAM_RECONNECT_DELAY).await;
            }
        }
    }

    async fn execute_batch_tasks<F, Fut>(
        state: &Arc<RwLock<StateSpace>>,
        amms: Vec<AMM>,
        provider: P,
        log_target: &str,
        task: F,
    ) -> Vec<Address>
    where
        F: Fn(AMM, P) -> Fut,
        Fut: Future<Output = Result<AMM, AMMError>> + Send,
        P: Provider<N> + Clone,
        N: Network,
    {
        if amms.is_empty() {
            return Vec::new();
        }

        let mut futures = FuturesUnordered::new();
        for amm in amms {
            let provider = provider.clone();
            let addr = amm.address();
            let future = task(amm, provider);
            futures.push(async move { (addr, future.await) });
        }

        let mut affected = Vec::new();
        while let Some((addr, res)) = futures.next().await {
            match res {
                Ok(new_amm) => {
                    state.write().await.state.insert(addr, new_amm);
                    affected.push(addr);
                }
                Err(e) => {
                    error!(target: "state_space::sync", ?addr, task = log_target, "Task failed: {}", e);
                }
            }
        }
        affected
    }

    async fn apply_logs_for_block(
        provider: &P,
        state: &Arc<RwLock<StateSpace>>,
        hooks: &HookRegistry<Vec<Address>>,
        block_num: u64,
        mut logs: Vec<Log>,
        latest_block: &Arc<AtomicU64>,
    ) -> Result<Vec<Address>, StateSpaceError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        latest_block.store(block_num, Ordering::Relaxed);

        if logs.is_empty() {
            return Ok(vec![]);
        }

        logs.sort_by_key(|log| {
            (
                log.block_number.unwrap_or_default(),
                log.transaction_index.unwrap_or_default(),
                log.log_index.unwrap_or_default(),
            )
        });

        let (affected, needs_resync, needs_async_update) = state.write().await.sync(&logs)?;

        let amms_to_resync: Vec<AMM> = {
            let guard = state.read().await;
            needs_resync
                .iter()
                .filter_map(|addr| guard.state.get(addr).cloned())
                .collect()
        };

        if !amms_to_resync.is_empty() {
            let _ = Self::execute_batch_tasks(
                state,
                amms_to_resync,
                provider.clone(),
                "auto-resync",
                |amm, provider| async move {
                    amm.init(BlockId::Number(block_num.into()), provider).await
                },
            )
            .await;
        }

        let amms_to_update: Vec<AMM> = {
            let guard = state.read().await;
            needs_async_update
                .iter()
                .filter_map(|addr| guard.state.get(addr).cloned())
                .collect()
        };

        if !amms_to_update.is_empty() {
            let _ = Self::execute_batch_tasks(
                state,
                amms_to_update,
                provider.clone(),
                "async-update",
                |mut amm, provider| async move {
                    amm.update(provider).await?;
                    Ok(amm)
                },
            )
            .await;
        }

        if !affected.is_empty() {
            hooks.notify(&affected).await;
        }

        Ok(affected)
    }

    async fn collect_logs_for_chunks(
        provider: &P,
        chunks: &[LogQueryChunk],
        from_block: u64,
        to_block: u64,
        bloom: Option<&Bloom>,
    ) -> Result<Vec<Log>, StateSpaceError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let mut all_logs = Vec::new();

        for chunk in chunks {
            if let Some(block_bloom) = bloom {
                if !Self::bloom_maybe_has_relevant_logs(block_bloom, chunk) {
                    continue;
                }
            }

            let filter = chunk.ranged_filter(from_block, to_block);
            let logs = provider
                .get_logs(&filter)
                .await
                .map_err(StateSpaceError::from)?;
            all_logs.extend(logs);
        }

        Ok(all_logs)
    }

    async fn backfill_range(
        provider: &P,
        state: &Arc<RwLock<StateSpace>>,
        hooks: &HookRegistry<Vec<Address>>,
        chunks: &[LogQueryChunk],
        from_block: u64,
        to_block: u64,
        latest_block: &Arc<AtomicU64>,
        chain_id: u64,
    ) -> Result<Vec<Vec<Address>>, StateSpaceError>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        if from_block > to_block {
            return Ok(vec![]);
        }

        let mut results = Vec::new();
        let mut start = from_block;
        let window = Self::backfill_window_size(chain_id);

        while start <= to_block {
            let mut end = (start + window - 1).min(to_block);

            loop {
                let logs_res =
                    Self::collect_logs_for_chunks(provider, chunks, start, end, None).await;

                match logs_res {
                    Ok(mut logs) => {
                        logs.sort_by_key(|log| {
                            (
                                log.block_number.unwrap_or_default(),
                                log.transaction_index.unwrap_or_default(),
                                log.log_index.unwrap_or_default(),
                            )
                        });

                        let mut by_block: HashMap<u64, Vec<Log>> = HashMap::new();
                        for log in logs {
                            if let Some(bn) = log.block_number {
                                by_block.entry(bn).or_default().push(log);
                            }
                        }

                        for block_num in start..=end {
                            let block_logs = by_block.remove(&block_num).unwrap_or_default();
                            let affected = Self::apply_logs_for_block(
                                provider,
                                state,
                                hooks,
                                block_num,
                                block_logs,
                                latest_block,
                            )
                            .await?;

                            if !affected.is_empty() {
                                results.push(affected);
                            }
                        }

                        start = end + 1;
                        break;
                    }
                    Err(e) => {
                        if start == end {
                            return Err(e);
                        }

                        end = start + ((end - start) / 2);
                        warn!(
                            "Backfill window {}..{} failed, shrinking window",
                            start, end
                        );
                    }
                }
            }
        }

        Ok(results)
    }

    async fn build_query_chunks(
        provider: &P,
        state: &Arc<RwLock<StateSpace>>,
        chain_id: u64,
    ) -> Vec<LogQueryChunk>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let guard = state.read().await;

        let mut topic_addresses = HashSet::new();
        let mut address_only_addresses = HashSet::new();
        let mut topic_signatures: HashSet<FixedBytes<32>> = HashSet::new();
        let mut has_slipstream_pool = false;

        for amm in guard.state.values() {
            let sync_events = amm.sync_events();
            let has_events = !sync_events.is_empty();

            if has_events {
                for event in sync_events {
                    topic_signatures.insert(event);
                }
            }

            match amm {
                AMM::UniswapV4Pool(p) => {
                    if has_events {
                        topic_addresses.insert(p.manager_address);
                    }
                }
                AMM::PancakeInfinityPool(p) => {
                    if has_events {
                        topic_addresses.insert(p.manager_address);
                    }
                }
                AMM::FluidDexPool(p) => {
                    if has_events {
                        topic_addresses.insert(p.address);
                    }
                    if let Some(addr) = get_liquidity_layer(chain_id) {
                        topic_addresses.insert(addr);
                    }
                }
                AMM::BalancerV2Pool(p) => {
                    if has_events {
                        if let Some(vault) = balancer_v2::get_vault_address(chain_id) {
                            topic_addresses.insert(vault);
                        } else {
                            topic_addresses.insert(p.vault_address);
                        }
                    }
                }
                AMM::BalancerV3Pool(p) => {
                    if has_events {
                        if let Some(vault) = balancer_v3::get_vault_address(chain_id) {
                            topic_addresses.insert(vault);
                        } else {
                            topic_addresses.insert(p.vault_address);
                        }
                    }
                }
                AMM::EkuboPool(_) => {
                    if let Some(core) = ekubo::get_core_address(chain_id) {
                        address_only_addresses.insert(core);
                    }
                }
                AMM::AerodromeSlipstreamPool(_) => {
                    has_slipstream_pool = true;
                    if has_events {
                        topic_addresses.insert(amm.address());
                    }
                }
                _ => {
                    if has_events {
                        topic_addresses.insert(amm.address());
                    }
                }
            }
        }

        drop(guard);

        if has_slipstream_pool && chain_id == BASE_CHAIN_ID {
            if let Some(fee_module) = Self::resolve_slipstream_fee_module(provider).await {
                topic_addresses.insert(fee_module);
                topic_signatures.insert(ICustomFeeModule::CustomFeeSet::SIGNATURE_HASH);
            }
        }

        let mut chunks = Vec::new();

        if !topic_addresses.is_empty() && !topic_signatures.is_empty() {
            let mut topic_addresses: Vec<Address> = topic_addresses.into_iter().collect();
            topic_addresses.sort();

            let mut topic_signatures: Vec<FixedBytes<32>> = topic_signatures.into_iter().collect();
            topic_signatures.sort();

            for addresses in topic_addresses.chunks(LOG_ADDRESS_CHUNK_SIZE) {
                chunks.push(LogQueryChunk {
                    addresses: addresses.to_vec(),
                    mode: QueryMode::TopicFiltered(topic_signatures.clone()),
                });
            }
        }

        if !address_only_addresses.is_empty() {
            let mut address_only_addresses: Vec<Address> =
                address_only_addresses.into_iter().collect();
            address_only_addresses.sort();

            for addresses in address_only_addresses.chunks(LOG_ADDRESS_CHUNK_SIZE) {
                chunks.push(LogQueryChunk {
                    addresses: addresses.to_vec(),
                    mode: QueryMode::AddressOnly,
                });
            }
        }

        chunks
    }

    async fn resolve_slipstream_fee_module(provider: &P) -> Option<Address>
    where
        P: Provider<N> + Clone,
        N: Network,
    {
        let factory = ICLFactoryReader::new(BASE_SLIPSTREAM_FACTORY, provider.clone());
        match factory.swapFeeModule().call().await {
            Ok(addr) if addr != Address::ZERO => Some(addr),
            Ok(_) => None,
            Err(e) => {
                warn!("Failed to fetch Slipstream FeeModule address: {}", e);
                None
            }
        }
    }

    fn bloom_maybe_has_relevant_logs(bloom: &Bloom, chunk: &LogQueryChunk) -> bool {
        let address_hit = chunk
            .addresses
            .iter()
            .any(|addr| bloom.contains_input(BloomInput::Raw(addr.as_slice())));

        if !address_hit {
            return false;
        }

        match &chunk.mode {
            QueryMode::AddressOnly => true,
            QueryMode::TopicFiltered(topics) => topics
                .iter()
                .any(|topic| bloom.contains_input(BloomInput::Raw(topic.as_slice()))),
        }
    }

    fn backfill_window_size(chain_id: u64) -> u64 {
        match chain_id {
            ARBITRUM_CHAIN_ID => 200,
            BASE_CHAIN_ID => 100,
            ETHEREUM_MAINNET_CHAIN_ID => 50,
            _ => 50,
        }
    }
}

#[derive(Clone)]
pub struct StateSpaceBuilder<N, P> {
    pub provider: P,
    pub latest_block: u64,
    pub factories: Vec<Factory>,
    pub amms: Vec<AMM>,
    pub filters: Vec<PoolFilter>,
    pub hooks: Vec<StateHook<Vec<Address>>>,
    pub snapshot_path: Option<PathBuf>,
    pub snapshot_config: Option<SnapshotConfig>,
    pub rate_sync_interval: Option<Duration>,
    pub curve_sync_interval: Option<Duration>,
    pub maintenance_interval: Option<Duration>,
    pub realtime_source: RealtimeSyncSource,
    phantom: PhantomData<N>,
}

impl<N, P> StateSpaceBuilder<N, P>
where
    N: Network,
    P: Provider<N> + Clone + 'static,
{
    pub fn new(provider: P) -> StateSpaceBuilder<N, P> {
        Self {
            provider,
            latest_block: 0,
            factories: vec![],
            amms: vec![],
            filters: vec![],
            phantom: PhantomData,
            snapshot_path: None,
            snapshot_config: None,
            rate_sync_interval: None,
            curve_sync_interval: None,
            maintenance_interval: None,
            realtime_source: RealtimeSyncSource::Auto,
            hooks: vec![],
        }
    }

    pub fn block(self, latest_block: u64) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            latest_block,
            ..self
        }
    }

    pub fn with_factories(self, factories: Vec<Factory>) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder { factories, ..self }
    }

    pub fn with_amms(self, amms: Vec<AMM>) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder { amms, ..self }
    }

    pub fn with_filters(self, filters: Vec<PoolFilter>) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder { filters, ..self }
    }

    pub fn with_hooks(self, hooks: Vec<StateHook<Vec<Address>>>) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder { hooks, ..self }
    }

    pub fn with_snapshot_path(self, snapshot_path: PathBuf) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            snapshot_path: Some(snapshot_path),
            ..self
        }
    }

    pub fn with_snapshot_enabled(self, config: Option<SnapshotConfig>) -> StateSpaceBuilder<N, P> {
        let config = config.unwrap_or_default();
        StateSpaceBuilder {
            snapshot_config: Some(config),
            ..self
        }
    }

    pub fn with_rate_sync_interval(self, interval: Duration) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            rate_sync_interval: Some(interval),
            ..self
        }
    }

    pub fn with_maintenance_interval(self, interval: Duration) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            maintenance_interval: Some(interval),
            ..self
        }
    }

    pub fn with_curve_sync_interval(self, interval: Duration) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            curve_sync_interval: Some(interval),
            ..self
        }
    }

    pub fn with_realtime_source(self, source: RealtimeSyncSource) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            realtime_source: source,
            ..self
        }
    }

    pub async fn sync(mut self) -> Result<StateSpaceManager<N, P>, AMMError> {
        let mut state_space = StateSpace::default();

        let chain_id = self.provider.get_chain_id().await?;
        info!(target: "state_space::sync", "Syncing AMMs for chain {}", chain_id);

        let chain_tip_u64 = if self.latest_block > 0 {
            self.latest_block
        } else {
            self.provider.get_block_number().await?
        };

        // If latest_block was not set (0), update it with the fetched chain tip
        if self.latest_block == 0 {
            self.latest_block = chain_tip_u64;
        }

        let chain_tip = BlockId::from(chain_tip_u64);

        let factories = self.factories.clone();
        let mut futures = FuturesUnordered::new();

        // 1. Filter statically loaded AMMs
        let mut valid_amms = Vec::with_capacity(self.amms.len());
        for amm in self.amms {
            if let Some(supported) = amm.supported_chains() {
                if !supported.contains(&chain_id) {
                    warn!(
                        target: "state_space::sync",
                        amm = ?amm.address(),
                        supported = ?supported,
                        current = chain_id,
                        "Skipping AMM due to chain mismatch"
                    );
                    continue;
                }
            }
            valid_amms.push(amm);
        }
        self.amms = valid_amms;

        let mut filter_set = HashSet::new();
        for factory in &self.factories {
            for event in factory.pool_events() {
                filter_set.insert(event);
            }
        }

        for amm in self.amms.iter() {
            for event in amm.sync_events() {
                filter_set.insert(event);
            }
        }

        let block_filter = Filter::new().event_signature(FilterSet::from(
            filter_set.into_iter().collect::<Vec<FixedBytes<32>>>(),
        ));
        let mut amm_variants = HashMap::new();

        for amm in self.amms.into_iter() {
            amm_variants
                .entry(amm.variant())
                .or_insert_with(Vec::new)
                .push(amm);
        }

        for factory in factories {
            let provider = self.provider.clone();
            let filters = self.filters.clone();

            let extension = amm_variants.remove(&factory.variant());
            futures.push(tokio::spawn(async move {
                let mut discovered_amms = factory.discover(chain_tip, provider.clone()).await?;

                info!(
                    target: "state_space::sync",
                    factory = %factory.address(),
                    discovered = discovered_amms.len(),
                    "Discovered AMMs"
                );

                // 2. Filter discovered AMMs based on chain support
                discovered_amms.retain(|amm| {
                    if let Some(supported) = amm.supported_chains() {
                        if !supported.contains(&chain_id) {
                            warn!(
                                target: "state_space::sync",
                                factory = %factory.address(),
                                amm = ?amm.address(),
                                supported = ?supported,
                                current = chain_id,
                                "Filtering discovered AMM due to chain mismatch"
                            );
                            return false;
                        }
                    }
                    true
                });

                if let Some(amms) = extension {
                    discovered_amms.extend(amms);
                }

                // Apply discovery filters
                for filter in filters.iter() {
                    if filter.stage() == filters::FilterStage::Discovery {
                        let pre_filter_len = discovered_amms.len();
                        discovered_amms = filter.filter(discovered_amms).await?;

                        info!(
                            target: "state_space::sync",
                            factory = %factory.address(),
                            pre_filter_len,
                            post_filter_len = discovered_amms.len(),
                            filter = ?filter,
                            "Discovery filter"
                        );
                    }
                }

                discovered_amms = factory.sync(discovered_amms, chain_tip, provider).await?;

                // Apply sync filters
                for filter in filters.iter() {
                    if filter.stage() == filters::FilterStage::Sync {
                        let pre_filter_len = discovered_amms.len();
                        discovered_amms = filter.filter(discovered_amms).await?;

                        info!(
                            target: "state_space::sync",
                            factory = %factory.address(),
                            pre_filter_len,
                            post_filter_len = discovered_amms.len(),
                            filter = ?filter,
                            "Sync filter"
                        );
                    }
                }

                Ok::<Vec<AMM>, AMMError>(discovered_amms)
            }));
        }

        while let Some(res) = futures.next().await {
            let synced_amms = res??;

            for amm in synced_amms {
                let mut amm = amm;
                amm.set_last_synced_block(chain_tip_u64);
                state_space.state.insert(amm.address(), amm);
            }
        }

        // Sync remaining AMM variants in batches by variant
        for (variant, remaining_amms) in amm_variants.drain() {
            info!(target: "state_space::sync", variant = ?variant, count = remaining_amms.len(), "Syncing batch");
            let provider = self.provider.clone();
            if variant == crate::amms::amm::Variant::UniswapV3Pool {
                let chunk_size = 25;
                for chunk in remaining_amms.chunks(chunk_size) {
                    let synced = variant
                        .init_batch::<N, _>(chunk.to_vec(), chain_tip, provider.clone())
                        .await?;

                    // 在每次循环结束时短暂 sleep，避免超出 RPC 调用频率
                    sleep(Duration::from_millis(1500)).await;

                    for amm in synced {
                        let mut amm = amm;
                        amm.set_last_synced_block(chain_tip_u64);
                        state_space.state.insert(amm.address(), amm);
                    }
                }
            } else {
                let synced = variant
                    .init_batch::<N, _>(remaining_amms, chain_tip, provider.clone())
                    .await?;

                // 在每次循环结束时短暂 sleep，避免超出 RPC 调用频率
                sleep(Duration::from_millis(1500)).await;

                for amm in synced {
                    let mut amm = amm;
                    amm.set_last_synced_block(chain_tip_u64);
                    state_space.state.insert(amm.address(), amm);
                }
            }
        }

        let latest_block = Arc::new(AtomicU64::new(self.latest_block));
        state_space.latest_block = latest_block.clone();
        state_space.chain_id = chain_id;

        let state_space = Arc::new(RwLock::new(state_space));

        if let Some(snapshot_config) = self.snapshot_config {
            let hook = snapshot_config.into_state_hook(state_space.clone()).await;
            self.hooks.push(hook);
        }

        if let Some(interval) = self.rate_sync_interval {
            tokio::spawn(sync_services::start_balancer_v2_rate_sync_task(
                state_space.clone(),
                self.provider.clone(),
                interval,
            ));
            tokio::spawn(sync_services::start_balancer_v3_rate_sync_task(
                state_space.clone(),
                self.provider.clone(),
                interval,
            ));
            // Balancer V3 pools: swap_fee (can be updated by governance, may fail during init)
            tokio::spawn(sync_services::start_balancer_v3_fee_sync_task(
                state_space.clone(),
                self.provider.clone(),
                interval,
            ));

            // Fluid DEX pools: limits and centerPrice (expand over time, drift without events)
            tokio::spawn(sync_services::start_fluid_dex_limits_sync_task(
                state_space.clone(),
                self.provider.clone(),
                interval,
            ));
        }

        // Curve NG StableSwap pools: stored_rates for rebasing tokens & D value sync
        if let Some(interval) = self.curve_sync_interval.or(self.rate_sync_interval) {
            tokio::spawn(sync_services::start_curve_rate_sync_task(
                state_space.clone(),
                self.provider.clone(),
                interval,
            ));
        }

        if let Some(interval) = self.maintenance_interval {
            tokio::spawn(sync_services::start_state_maintenance_task(
                state_space.clone(),
                self.factories.clone(),
                self.provider.clone(),
                interval,
            ));
        }

        Ok(StateSpaceManager {
            latest_block,
            state: state_space,
            block_filter,
            provider: self.provider,
            realtime_source: self.realtime_source,
            phantom: PhantomData,
            hooks: HookRegistry::new(self.hooks),
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct StateSpace {
    pub state: HashMap<Address, AMM>,
    pub latest_block: Arc<AtomicU64>,
    pub chain_id: u64,
    cache: StateChangeCache<CACHE_SIZE>,
}

impl StateSpace {
    pub fn get(&self, address: &Address) -> Option<&AMM> {
        self.state.get(address)
    }

    pub fn get_mut(&mut self, address: &Address) -> Option<&mut AMM> {
        self.state.get_mut(address)
    }

    fn resolve_slipstream_fee_event_pool(&self, topics: &[FixedBytes<32>]) -> Option<Address> {
        if topics.len() < 2 {
            return None;
        }

        if topics[0] != ICustomFeeModule::CustomFeeSet::SIGNATURE_HASH {
            return None;
        }

        let pool_address = Address::from_word(topics[1]);
        match self.state.get(&pool_address) {
            Some(AMM::AerodromeSlipstreamPool(_)) => Some(pool_address),
            _ => None,
        }
    }

    pub fn sync(
        &mut self,
        logs: &[Log],
    ) -> Result<(Vec<Address>, Vec<Address>, Vec<Address>), StateSpaceError> {
        // 处理流程：
        // 1) 先按 (block_number, transaction_index, log_index) 排序，避免 WS/回补乱序破坏缓存与回滚语义
        // 2) 逐条应用 log 到对应池子的本地状态，并缓存“该区块开始前”的 AMM 快照用于 reorg unwind
        if logs.is_empty() {
            return Ok((vec![], vec![], vec![]));
        }

        let mut logs_sorted = logs.to_vec();
        logs_sorted.sort_by_key(|log| {
            (
                log.block_number.unwrap_or_default(),
                log.transaction_index.unwrap_or_default(),
                log.log_index.unwrap_or_default(),
            )
        });

        let latest = self.latest_block.load(Ordering::Relaxed);

        // We do not check for reorgs here using block numbers because partitioned log subscriptions
        // (chunking) can cause logs to arrive out of order or in interleaved batches.
        // A "late" batch from one chunk is not a reorg of the chain.
        // We rely on:
        // 1. Per-AMM `last_synced_block` checks to prevent rewinding individual pools.
        // 2. The periodic `start_state_maintenance_task` to handle actual chain reorgs/discrepancies.
        // 3. The `needs_resync` set to handle syncs that fail due to insufficient data (e.g. Curve V1 RemoveLiquidityOne)

        let mut affected_amms = HashSet::new();
        let mut needs_resync = HashSet::new();
        let mut needs_async_update = HashSet::new();
        let mut max_processed_block = latest;

        for log in &logs_sorted {
            let log_block_number = log
                .block_number
                .ok_or(StateSpaceError::MissingBlockNumber)?;

            // Track the latest block info seen in this batch
            if log_block_number > max_processed_block {
                max_processed_block = log_block_number;
            }

            let address = log.address();
            let direct_hit = self.state.contains_key(&address);

            let target_address = if direct_hit {
                Some(address)
            } else if log.topics().len() >= 2 {
                if Some(address) == get_liquidity_layer(self.chain_id) {
                    let pool_address = Address::from_word(log.topics()[1]);
                    match self.state.get(&pool_address) {
                        Some(AMM::FluidDexPool(_)) => Some(pool_address),
                        _ => None,
                    }
                } else if Some(address) == balancer_v2::get_vault_address(self.chain_id) {
                    // Balancer V2: poolId is in topics[1]
                    // The first 20 bytes of poolId is the pool address, which is used as the key in StateSpace
                    let pool_id = log.topics()[1];
                    let pool_address = Address::from_slice(&pool_id.as_slice()[0..20]);

                    match self.state.get(&pool_address) {
                        Some(AMM::BalancerV2Pool(p)) if p.pool_id == pool_id => Some(pool_address),
                        _ => None,
                    }
                } else if Some(address) == balancer_v3::get_vault_address(self.chain_id) {
                    // Balancer V3: pool address is in topics[1]
                    let pool_address = Address::from_word(log.topics()[1]);
                    if self.state.contains_key(&pool_address) {
                        Some(pool_address)
                    } else {
                        None
                    }
                } else if let Some(pool_address) =
                    self.resolve_slipstream_fee_event_pool(log.topics())
                {
                    Some(pool_address)
                } else {
                    let pool_id = log.topics()[1];
                    let virtual_address = Address::from_slice(&pool_id.as_slice()[0..20]);
                    match self.state.get(&virtual_address) {
                        Some(AMM::UniswapV4Pool(p)) if p.manager_address == address => {
                            Some(virtual_address)
                        }
                        Some(AMM::PancakeInfinityPool(p)) if p.manager_address == address => {
                            Some(virtual_address)
                        }
                        _ => None,
                    }
                }
            } else if log.topics().is_empty()
                && Some(address) == ekubo::get_core_address(self.chain_id)
            {
                // Ekubo Log0 events: no topics, pool_id is at data[20..52]
                let data = log.data().data.as_ref();
                if data.len() >= 52 {
                    let pool_id = FixedBytes::<32>::from_slice(&data[20..52]);
                    // Ekubo uses the first 20 bytes of pool_id as the virtual address key in StateSpace
                    let virtual_address = Address::from_slice(&pool_id.as_slice()[0..20]);

                    match self.state.get(&virtual_address) {
                        Some(AMM::EkuboPool(p)) if p.pool_id == pool_id => Some(virtual_address),
                        _ => None,
                    }
                } else {
                    None
                }
            } else {
                None
            };

            let Some(target_address) = target_address else {
                continue;
            };

            let Some(amm) = self.state.get_mut(&target_address) else {
                continue;
            };

            // 如果 log 区块小于已同步区块，跳过（幂等性）
            if log_block_number < amm.last_synced_block() {
                continue;
            }

            match amm.sync(log) {
                Ok(action) => {
                    amm.set_last_synced_block(log_block_number);
                    affected_amms.insert(target_address);

                    match action {
                        SyncAction::None => {}
                        SyncAction::AsyncUpdate => {
                            needs_async_update.insert(target_address);
                        }
                        SyncAction::Resync => {
                            needs_resync.insert(target_address);
                        }
                    }
                }

                Err(e) => {
                    error!(target: "state_space::sync", ?address, ?log_block_number, "Failed to sync AMM with log: {}", e);
                }
            }
        }

        // Update latest_block internally to ensure consistency with state lock
        if max_processed_block > latest {
            self.latest_block
                .store(max_processed_block, Ordering::Relaxed);
        }

        Ok((
            affected_amms.into_iter().collect(),
            needs_resync.into_iter().collect(),
            needs_async_update.into_iter().collect(),
        ))
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct SerializableStateSpace {
    pub state: HashMap<Address, AMM>,
    pub latest_block: u64,
    pub cache: (Vec<StateChange>, u64),
}

impl From<StateSpace> for SerializableStateSpace {
    fn from(ss: StateSpace) -> Self {
        Self {
            state: ss.state,
            latest_block: ss.latest_block.load(Ordering::Relaxed),
            cache: (ss.cache.cache.into_iter().collect(), ss.cache.oldest_block),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, FixedBytes};

    #[test]
    fn bloom_prefilter_address_only_matches() {
        let address = address!("1111111111111111111111111111111111111111");
        let mut bloom = Bloom::ZERO;
        bloom.accrue(BloomInput::Raw(address.as_slice()));

        let chunk = LogQueryChunk {
            addresses: vec![address],
            mode: QueryMode::AddressOnly,
        };

        assert!(StateSpaceManager::<(), ()>::bloom_maybe_has_relevant_logs(
            &bloom, &chunk
        ));
    }

    #[test]
    fn bloom_prefilter_topic_filtered_requires_topic_hit() {
        let address = address!("2222222222222222222222222222222222222222");
        let hit_topic = FixedBytes::<32>::from([0x11u8; 32]);
        let miss_topic = FixedBytes::<32>::from([0x22u8; 32]);

        let mut bloom = Bloom::ZERO;
        bloom.accrue(BloomInput::Raw(address.as_slice()));
        bloom.accrue(BloomInput::Raw(hit_topic.as_slice()));

        let hit_chunk = LogQueryChunk {
            addresses: vec![address],
            mode: QueryMode::TopicFiltered(vec![hit_topic]),
        };

        let miss_chunk = LogQueryChunk {
            addresses: vec![address],
            mode: QueryMode::TopicFiltered(vec![miss_topic]),
        };

        assert!(StateSpaceManager::<(), ()>::bloom_maybe_has_relevant_logs(
            &bloom, &hit_chunk
        ));
        assert!(!StateSpaceManager::<(), ()>::bloom_maybe_has_relevant_logs(
            &bloom,
            &miss_chunk
        ));
    }

    #[test]
    fn backfill_window_size_is_chain_specific() {
        assert_eq!(
            StateSpaceManager::<(), ()>::backfill_window_size(42161),
            200
        );
        assert_eq!(StateSpaceManager::<(), ()>::backfill_window_size(8453), 100);
        assert_eq!(StateSpaceManager::<(), ()>::backfill_window_size(1), 50);
        assert_eq!(StateSpaceManager::<(), ()>::backfill_window_size(10), 50);
    }

    #[test]
    fn slipstream_custom_fee_event_routes_to_pool_topic1() {
        let pool_address = address!("b2cc224c1c9fee385f8ad6a55b4d94e92359dc59");
        let mut state = StateSpace::default();
        state.state.insert(
            pool_address,
            AMM::AerodromeSlipstreamPool(
                crate::amms::aerodrome_slipstream::AerodromeSlipstreamPool::new(pool_address),
            ),
        );

        let topics = vec![
            ICustomFeeModule::CustomFeeSet::SIGNATURE_HASH,
            pool_address.into_word(),
        ];

        assert_eq!(
            state.resolve_slipstream_fee_event_pool(&topics),
            Some(pool_address)
        );
    }

    #[test]
    fn slipstream_custom_fee_event_ignores_unknown_pool() {
        let pool_address = address!("b2cc224c1c9fee385f8ad6a55b4d94e92359dc59");
        let state = StateSpace::default();
        let topics = vec![
            ICustomFeeModule::CustomFeeSet::SIGNATURE_HASH,
            pool_address.into_word(),
        ];

        assert_eq!(state.resolve_slipstream_fee_event_pool(&topics), None);
    }
}

impl From<SerializableStateSpace> for StateSpace {
    fn from(val: SerializableStateSpace) -> Self {
        let (cache, oldest_block) = val.cache;
        StateSpace {
            state: val.state,
            latest_block: Arc::new(AtomicU64::new(val.latest_block)),
            cache: StateChangeCache {
                cache: cache.into_iter().collect(),
                oldest_block,
            },
            chain_id: 0,
        }
    }
}

#[macro_export]
macro_rules! sync {
    // Sync factories with provider
    ($factories:expr, $provider:expr) => {{
        StateSpaceBuilder::new($provider.clone())
            .with_factories($factories)
            .sync()
            .await?
    }};

    // Sync factories with filters
    ($factories:expr, $filters:expr, $provider:expr) => {{
        StateSpaceBuilder::new($provider.clone())
            .with_factories($factories)
            .with_filters($filters)
            .sync()
            .await?
    }};

    ($factories:expr, $amms:expr, $filters:expr, $provider:expr) => {{
        StateSpaceBuilder::new($provider.clone())
            .with_factories($factories)
            .with_amms($amms)
            .with_filters($filters)
            .sync()
            .await?
    }};
}

//! Xlayer Flashblocks 实时流订阅与日志提取模块
//!
//! Xlayer（chain ID: 196）是基于 OP Stack 的 Optimistic Rollup，
//! 支持 flashblocks 机制 —— 在完整区块构建完成前以分片（slice）形式
//! 流式传输区块内容，实现 ~200ms 亚秒级交易预确认。
//!
//! 本模块与 `flashblocks.rs`（Base 链）的区别：
//!
//! | 维度                | Base                              | Xlayer                              |
//! |---------------------|-----------------------------------|-------------------------------------|
//! | WS 端点              | mainnet.flashblocks.base.org/ws  | ws.xlayer.tech/flashblocks          |
//! | payload_id 格式      | 非 hex ("03307607ad2ba79d")      | hex 编码 ("0x03099f3f7054c613")    |
//! | base 对象            | 仅 block_number + timestamp       | 完整区块头（9 个字段）               |
//! | receipt 中 txIndex   | transactionIndex 字段             | 无此字段，用累计计数器推导           |
//! | 消息压缩             | Brotli + JSON                    | 纯 JSON（Binary 备选 Brotli）       |
//! | 区块号来源           | metadata(数值)/base(hex)          | 相同，metadata 为 u64               |
//!
//! 端点说明（由用户调研确认）:
//! - wss://ws.xlayer.tech/flashblocks   — 新加坡 (AWS CloudFront)，默认
//! - wss://xlayerws.okx.com/flashblocks — 加拿大 (Cloudflare CDN)
//! 可通过环境变量 XLAYER_FLASHBLOCKS_WS 覆盖默认值。

use super::{
    AppliedLogDedupCache, HookRegistry, LogQueryChunk, LogSource, PendingSyncQueue, StateSpace,
    StateSpaceError, StateSpaceManager,
};
use crate::state_space::{
    STREAM_IDLE_TIMEOUT, STREAM_RECONNECT_DELAY, XLAYER_FLASHBLOCKS_RAW_WS_URL,
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
use std::time::Instant;
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{error, info, warn};

// ─────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────

/// Per-payload dedup cache 保留的最大 payload 数
const XLAYER_DEDUP_PAYLOAD_WINDOW: usize = 4;

/// 地址/主题解析缓存容量
const XLAYER_HEX_CACHE_MAX: usize = 8192;

/// 单次可选的交易计数累计器最大跟踪 payload 数
const XLAYER_TX_COUNT_WINDOW: usize = 8;

// ─────────────────────────────────────────────
// Message structs (Xlayer-specific)
// ─────────────────────────────────────────────

/// Xlayer flashblock 顶层消息
///
/// 验证来源: https://web3.okx.com/zh-hans/onchainos/dev-docs/xlayer/developer/flashblocks/node-providers
/// 实际验证: 2025-06-10 probe（xlayer_flashblocks_raw_probe.rs）
///
/// 消息示例（index=0）:
/// ```json
/// {
///   "payload_id": "0x0301772801fba43b",
///   "index": 0,
///   "base": { "block_number": "0x3b6e1e0", "timestamp": "0x6a29376c", ... },
///   "diff": { "block_hash": "0x26e6...", "transactions": [...], "logs_bloom": "0x..." },
///   "metadata": { "block_number": 62317024, "receipts": { "0x...": { "logs": [...] } } }
/// }
/// ```
///
/// 消息示例（index>0，增量 diff）:
/// ```json
/// {
///   "payload_id": "0x0301772801fba43b",
///   "index": 1,
///   "diff": { "block_hash": "0xdd2a...", "transactions": [...], ... },
///   "metadata": { "block_number": 62317024, "receipts": { "0x...": { "logs": [...] } } }
/// }
/// ```
#[derive(Debug, Deserialize)]
struct XlayerFlashblockMessage<'a> {
    #[serde(borrow)]
    payload_id: Cow<'a, str>,
    index: u64,
    #[serde(default)]
    base: Option<XlayerFlashblockBase<'a>>,
    #[serde(default)]
    diff: Option<XlayerFlashblockDiff<'a>>,
    #[serde(default)]
    metadata: Option<XlayerFlashblockMetadata>,
}

/// index=0 时携带的区块头信息
#[derive(Debug, Deserialize)]
struct XlayerFlashblockBase<'a> {
    #[serde(default, borrow)]
    block_number: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    timestamp: Option<Cow<'a, str>>,
}

/// 每个 slice 中的 diff 增量
#[derive(Debug, Deserialize)]
struct XlayerFlashblockDiff<'a> {
    #[serde(default, borrow)]
    block_hash: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    transactions: Vec<Cow<'a, str>>,
}

/// 每个 slice 中的 metadata
#[derive(Debug, Deserialize)]
struct XlayerFlashblockMetadata {
    #[serde(default)]
    block_number: Option<u64>,
    /// 使用 serde_json::Map 保留 receipts 的 JSON 插入顺序，
    /// 后续再按 cumulativeGasUsed 二次排序确保确定性。
    #[serde(default)]
    receipts: serde_json::Map<String, serde_json::Value>,
}

/// 从 serde_json::Value 中提取 cumulativeGasUsed（排序用）
fn parse_cumulative_gas(v: &serde_json::Value) -> Option<u64> {
    let s = v.get("cumulativeGasUsed")?.as_str()?;
    Some(u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()?)
}

/// 从 serde_json::Value 中提取日志列表
fn parse_logs_from_value(v: &serde_json::Value) -> Vec<XlayerFlashblockLog> {
    let Some(logs_array) = v.get("logs").and_then(|l| l.as_array()) else {
        return vec![];
    };
    logs_array
        .iter()
        .filter_map(|lv| {
            let address = lv.get("address")?.as_str()?.to_string();
            let topics = lv
                .get("topics")
                .and_then(|t| t.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|t| t.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let data = lv.get("data")?.as_str()?.to_string();
            Some(XlayerFlashblockLog {
                address,
                topics,
                data,
            })
        })
        .collect()
}

/// 单条日志（Xlayer flashblocks 用 owned String）
#[derive(Debug)]
struct XlayerFlashblockLog {
    address: String,
    topics: Vec<String>,
    data: String,
}

// ─────────────────────────────────────────────
// Dedup cache (per-payload)
// ─────────────────────────────────────────────

/// Per-payload 去重缓存
///
/// 同个 payload_id 的不同 slice 可能包含重复的收据，
/// 以 (tx_hash, log_index) 为 key 去重。
struct XlayerDedupCache {
    per_payload: HashMap<String, HashSet<(String, u64)>>,
    order: VecDeque<String>,
    max_payloads: usize,
}

impl XlayerDedupCache {
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

// ─────────────────────────────────────────────
// Parse cache
// ─────────────────────────────────────────────

/// 地址和主题解析缓存
struct XlayerParseCache {
    address: HashMap<String, Address>,
    topic: HashMap<String, B256>,
}

impl XlayerParseCache {
    fn new() -> Self {
        Self {
            address: HashMap::new(),
            topic: HashMap::new(),
        }
    }

    fn parse_address(&mut self, value: &str) -> Option<Address> {
        if let Some(v) = self.address.get(value) {
            return Some(*v);
        }
        let parsed = Address::from_str(value).ok()?;
        if self.address.len() < XLAYER_HEX_CACHE_MAX {
            self.address.insert(value.to_string(), parsed);
        }
        Some(parsed)
    }

    fn parse_topic(&mut self, value: &str) -> Option<B256> {
        if let Some(v) = self.topic.get(value) {
            return Some(*v);
        }
        let parsed = B256::from_str(value).ok()?;
        if self.topic.len() < XLAYER_HEX_CACHE_MAX {
            self.topic.insert(value.to_string(), parsed);
        }
        Some(parsed)
    }
}

// ─────────────────────────────────────────────
// Per-payload transaction index tracker
// ─────────────────────────────────────────────

/// 跟踪每个 payload_id 的已处理交易数，用于推导 block-global transactionIndex。
///
/// Xlayer receipt 没有 `transactionIndex` 字段，但 JSON 插入序 = 执行序。
/// 我们用累计计数器保证同一个 payload_id（即同一个区块）内，
/// 跨 slice 的交易获得递增且不重复的 tx_index。
struct XlayerTxCountTracker {
    counts: HashMap<String, u64>,
    order: VecDeque<String>,
    max_payloads: usize,
}

impl XlayerTxCountTracker {
    fn new(max_payloads: usize) -> Self {
        Self {
            counts: HashMap::new(),
            order: VecDeque::new(),
            max_payloads,
        }
    }

    /// 获取当前 payload 已累积的交易数（即下一个 tx 的起始 index）
    fn base(&self, payload_id: &str) -> u64 {
        self.counts.get(payload_id).copied().unwrap_or(0)
    }

    /// 在当前 payload 中处理了 `count` 个交易后，更新累计数
    fn advance(&mut self, payload_id: &str, count: u64) {
        let entry = self.counts.entry(payload_id.to_string()).or_insert(0);
        *entry += count;

        // 如果是新 payload，加入淘汰队列
        if *entry == count {
            self.order.push_back(payload_id.to_string());
        }

        // LRU 淘汰
        while self.order.len() > self.max_payloads {
            if let Some(evicted) = self.order.pop_front() {
                self.counts.remove(&evicted);
            }
        }
    }
}

// ─────────────────────────────────────────────
// Log matcher (local copy — same logic as Base flashblocks)
// ─────────────────────────────────────────────

#[derive(Clone, Debug)]
struct XlayerLogMatcher {
    topic_addresses: HashSet<Address>,
    topic_signatures: HashSet<FixedBytes<32>>,
    address_only_addresses: HashSet<Address>,
}

impl XlayerLogMatcher {
    fn from_query_chunks(chunks: &[LogQueryChunk]) -> Self {
        let mut topic_addresses = HashSet::new();
        let mut topic_signatures = HashSet::new();
        let mut address_only_addresses = HashSet::new();

        for chunk in chunks {
            match &chunk.mode {
                super::QueryMode::TopicFiltered(topics) => {
                    for addr in &chunk.addresses {
                        topic_addresses.insert(*addr);
                    }
                    for topic in topics {
                        topic_signatures.insert(*topic);
                    }
                }
                super::QueryMode::AddressOnly => {
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

// ─────────────────────────────────────────────
// Hex parsing utilities
// ─────────────────────────────────────────────

fn parse_hex_u64(s: &str) -> Option<u64> {
    let raw = s.strip_prefix("0x").unwrap_or(s);
    u64::from_str_radix(raw, 16).ok()
}

fn xlayer_flashblock_block_number(fb: &XlayerFlashblockMessage<'_>) -> Option<u64> {
    fb.metadata
        .as_ref()
        .and_then(|m| m.block_number)
        .or_else(|| {
            fb.base
                .as_ref()
                .and_then(|base| base.block_number.as_deref())
                .and_then(parse_hex_u64)
        })
}

// ─────────────────────────────────────────────
// Log extraction
// ─────────────────────────────────────────────

/// 从 Xlayer flashblock 消息中提取匹配的 Alloy Log。
///
/// 返回 (匹配的日志, block_number, 解析失败数, 解析失败的地址集合)
#[allow(clippy::type_complexity)]
fn extract_logs_from_xlayer_flashblock(
    fb: &XlayerFlashblockMessage,
    matcher: &XlayerLogMatcher,
    dedup_cache: &mut XlayerDedupCache,
    parse_cache: &mut XlayerParseCache,
    tx_tracker: &mut XlayerTxCountTracker,
    latest_block_timestamp: &mut Option<(u64, u64)>,
) -> (Vec<Log>, Option<u64>, usize, HashSet<Address>) {
    let mut out = Vec::new();
    let mut decode_fail = 0usize;
    let mut decode_failed_addresses = HashSet::new();

    // 1. 获取 block_number（metadata 数值优先，base hex 备选）
    let block_number = xlayer_flashblock_block_number(fb);

    let Some(block_number) = block_number else {
        return (out, None, decode_fail, decode_failed_addresses);
    };

    // 2. 获取/缓存 block_timestamp
    let base_timestamp = fb
        .base
        .as_ref()
        .and_then(|base| base.timestamp.as_deref())
        .and_then(parse_hex_u64);
    let block_timestamp = if let Some(ts) = base_timestamp {
        *latest_block_timestamp = Some((block_number, ts));
        Some(ts)
    } else {
        match *latest_block_timestamp {
            Some((known_block, known_ts)) if block_number == known_block => Some(known_ts),
            Some((known_block, known_ts)) if block_number > known_block => {
                // Xlayer 出块时间约 1s，此处保守估计 +1s/block
                let delta_blocks = block_number - known_block;
                let estimated_ts = known_ts.saturating_add(delta_blocks.saturating_mul(1));
                *latest_block_timestamp = Some((block_number, estimated_ts));
                Some(estimated_ts)
            }
            _ => None,
        }
    };

    // 3. 获取 metadata（receipts 从这里提取）
    let Some(metadata) = fb.metadata.as_ref() else {
        return (
            out,
            Some(block_number),
            decode_fail,
            decode_failed_addresses,
        );
    };

    if metadata.receipts.is_empty() {
        return (
            out,
            Some(block_number),
            decode_fail,
            decode_failed_addresses,
        );
    }

    // 4. 获取 block_hash（用于构造 Log）
    let block_hash: Option<B256> = fb
        .diff
        .as_ref()
        .and_then(|d| d.block_hash.as_deref())
        .and_then(|h| B256::from_str(h).ok());

    // ═══════════════════════════════════════════════════════════════════
    //  5. 推导 transaction_index
    //  ═══════════════════════════════════════════════════════════════════
    //
    //  背景: Base flashblocks 的 receipt 中直接包含 `transactionIndex`（hex 字符串），
    //  可以直接解析使用。但 Xlayer flashblocks 的 receipt **没有** `transactionIndex` 字段。
    //
    //  解决方案: 使用 per-payload 累计计数器 `XlayerTxCountTracker`。
    //
    //  原理:
    //  - Xlayer flashblocks 的 receipts 以 JSON Object（Map）形式传输，
    //    serde_json 的 Map 类型（基于 indexmap::IndexMap）**保留键的插入序**。
    //  - 服务端按交易执行顺序依次插入收据，因此 JSON 枚举顺序 = 执行顺序。
    //  - 同一个 payload_id（即同一个区块）可能跨多个 slice（index=0,1,2...），
    //    每个 slice 携带该区块的增量交易收据。
    //  - `tx_tracker` 按 payload_id 累计已处理的交易数，保证跨 slice 的 tx_index 递增不重复。
    //
    //  边缘情况: 如果同个 payload 中有 ≥2 个不同交易操作同个池子，
    //  后续的"懒排序"步骤会 hash diff.transactions 中的原始交易字节来修正真实顺序。
    //  ═══════════════════════════════════════════════════════════════════
    let tx_base = tx_tracker.base(&fb.payload_id);

    // 按 cumulativeGasUsed 排序 receipts，替代不安全的 HashMap 迭代顺序
    let mut sorted_receipts: Vec<(&String, &serde_json::Value)> =
        metadata.receipts.iter().collect();
    sorted_receipts.sort_by(|(_, a), (_, b)| {
        parse_cumulative_gas(a)
            .unwrap_or(0)
            .cmp(&parse_cumulative_gas(b).unwrap_or(0))
    });

    let mut tx_position = 0u64;
    for (_tx_hash, receipt_value) in sorted_receipts {
        let transaction_index = tx_base + tx_position;
        let receipt_logs = parse_logs_from_value(receipt_value);

        for (log_idx, raw_log) in receipt_logs.iter().enumerate() {
            // 5a. dedup 检查
            if !dedup_cache.insert(&fb.payload_id, _tx_hash, log_idx as u64) {
                continue;
            }

            // 5b. 解析 address
            let Some(address) = parse_cache.parse_address(&raw_log.address) else {
                decode_fail += 1;
                continue;
            };

            // 5c. 预筛选：地址是否在当前关注的集合中
            let is_address_only = matcher.address_only_addresses.contains(&address);
            let is_topic_candidate = matcher.topic_addresses.contains(&address);
            if !is_address_only && !is_topic_candidate {
                continue;
            }

            // 5d. 检查 topics 数量上限
            if raw_log.topics.len() > 4 {
                decode_fail += 1;
                decode_failed_addresses.insert(address);
                continue;
            }

            // 5e. 解析 topics
            let mut topics = Vec::with_capacity(raw_log.topics.len());
            if is_topic_candidate {
                // 需要匹配 event signature（topic0）
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
                // address_only 模式：解析所有 topics 但不做 signature 匹配
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

            // 5f. 解析 data
            let data = match Bytes::from_str(&raw_log.data) {
                Ok(v) => v,
                Err(_) => {
                    decode_fail += 1;
                    decode_failed_addresses.insert(address);
                    continue;
                }
            };

            // 5g. 构造 LogData
            let Some(log_data) = LogData::new(topics, data) else {
                decode_fail += 1;
                decode_failed_addresses.insert(address);
                continue;
            };

            // 5h. 解析 tx_hash
            let tx_hash_parsed = _tx_hash.parse::<B256>().ok();

            // 5i. 构造 Alloy Log
            let log = Log {
                inner: alloy::primitives::Log {
                    address,
                    data: log_data,
                },
                block_hash,
                block_number: Some(block_number),
                block_timestamp,
                transaction_hash: tx_hash_parsed,
                transaction_index: Some(transaction_index),
                log_index: Some(log_idx as u64),
                removed: false,
            };

            out.push(log);
        }

        tx_position += 1;
    }

    // 6. 更新 payload tx 计数器
    tx_tracker.advance(&fb.payload_id, tx_position);

    // 7. 懒排序检测与修正（与 Base 相同）
    //    如果同个池子在同个 payload 中被 ≥2 个不同交易操作，
    //    需要 hash raw transactions 来修正真实的交易顺序。
    let mut pool_collision_detector: HashMap<Address, HashSet<B256>> = HashMap::new();
    let mut needs_lazy_evaluation = false;

    for log in &out {
        if let (Some(hash), address) = (log.transaction_hash, log.inner.address) {
            let interacting_txs = pool_collision_detector.entry(address).or_default();
            interacting_txs.insert(hash);
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
                    // diff.transactions is slice-local on XLayer. Keep the
                    // per-payload base so the corrected index remains
                    // block-global across all flashblock slices.
                    real_tx_index_map.insert(exact_hash, tx_base + real_idx as u64);
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

// ─────────────────────────────────────────────
// Flashblock stream subscription
// ─────────────────────────────────────────────

impl<N, P> StateSpaceManager<N, P> {
    /// 创建 Xlayer flashblocks 实时日志流。
    ///
    /// 连接 Xlayer flashblocks WebSocket，解析消息，提取 logs，
    /// 经过 bloom 预筛 + 地址/主题匹配后通过 `apply_logs_for_block()` 应用。
    pub(super) fn subscribe_xlayer_flashblocks_stream(
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
        chain_id: u64,
    ) -> impl Stream<Item = Result<(super::RealtimeUpdateMeta, Vec<Address>), StateSpaceError>> + Send
    where
        P: Provider<N> + Clone + 'static,
        N: Network + 'static,
    {
        let matcher = XlayerLogMatcher::from_query_chunks(&query_chunks);

        stream! {
            let mut dedup_cache = XlayerDedupCache::new(XLAYER_DEDUP_PAYLOAD_WINDOW);
            let mut parse_cache = XlayerParseCache::new();
            let mut tx_tracker = XlayerTxCountTracker::new(XLAYER_TX_COUNT_WINDOW);
            let mut latest_block_timestamp: Option<(u64, u64)> = None;
            // 用于 simd_json 零拷贝解码的可变缓冲区（在 stream 作用域内，保证生命周期足够长）
            let mut decode_buf = Vec::with_capacity(64 * 1024);

            loop {
                // 1. 初始回填：处理上次同步点之后的区块
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
                                "Xlayer initial catch-up completed (updates suppressed during catch-up stage)"
                            );
                        }
                    }
                    Err(e) => {
                        warn!("Xlayer initial backfill failed before flashblocks subscribe: {}", e);
                    }
                }

                // 2. 连接 flashblocks WebSocket
                let connect = connect_async(XLAYER_FLASHBLOCKS_RAW_WS_URL).await;
                let (mut socket, _) = match connect {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Xlayer flashblocks ws connect failed: {}", e);
                        sleep(STREAM_RECONNECT_DELAY).await;
                        continue;
                    }
                };

                info!("Connected to Xlayer flashblocks WebSocket");

                // 3. 消息读取循环
                loop {
                    let next = tokio::time::timeout(STREAM_IDLE_TIMEOUT, socket.next()).await;
                    let maybe_message_result = match next {
                        Ok(v) => v,
                        Err(_) => {
                            warn!("Xlayer flashblocks stream timeout, reconnecting");
                            break;
                        }
                    };

                    let Some(message_result) = maybe_message_result else {
                        warn!("Xlayer flashblocks stream ended");
                        break;
                    };

                    let message = match message_result {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("Xlayer flashblocks stream receive error: {}", e);
                            break;
                        }
                    };

                    let received_at = Instant::now();
                    // 4. 解析消息（decode_buf 在 stream 作用域内，保证生命周期覆盖返回值的借用）
                    decode_buf.clear();
                    let fb: Option<XlayerFlashblockMessage> = match message {
                        Message::Text(text) => {
                            decode_buf.extend_from_slice(text.as_bytes());
                            simd_json::from_slice::<XlayerFlashblockMessage>(&mut decode_buf).ok()
                        }
                        Message::Binary(bin) => {
                            // 方案 A: 拷贝到 decode_buf 后尝试 simd_json
                            decode_buf.extend_from_slice(bin.as_ref());
                            let simd_result =
                                simd_json::from_slice::<XlayerFlashblockMessage>(&mut decode_buf);
                            if simd_result.is_ok() {
                                simd_result.ok()
                            } else {
                                // 方案 B: Brotli 解压 → decode_buf → serde_json
                                decode_buf.clear();
                                let mut reader =
                                    brotli::Decompressor::new(bin.as_ref(), 4096);
                                if reader.read_to_end(&mut decode_buf).is_err() {
                                    None
                                } else {
                                    serde_json::from_slice::<XlayerFlashblockMessage>(
                                        &decode_buf,
                                    )
                                    .ok()
                                }
                            }
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

                    let block_num = xlayer_flashblock_block_number(&fb)
                        .unwrap_or_else(|| realtime_head.load(Ordering::Relaxed));
                    let current_synced = realtime_head.load(Ordering::Relaxed);
                    if block_num > current_synced.saturating_add(1) {
                        match provider.get_block_number().await {
                            Ok(chain_head) => {
                                let from_block = current_synced.saturating_add(1);
                                let to_block = chain_head.min(block_num.saturating_sub(1));
                                if from_block <= to_block {
                                    match Self::backfill_range(
                                        &provider,
                                        &state,
                                        &hooks,
                                        &query_chunks,
                                        from_block,
                                        to_block,
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
                                            let affected_pools = results
                                                .iter()
                                                .map(|(_, affected)| affected.len())
                                                .sum::<usize>();
                                            if !results.is_empty() {
                                                info!(
                                                    from_block,
                                                    to_block,
                                                    live_block = block_num,
                                                    backfilled_blocks = results.len(),
                                                    affected_pools,
                                                    "Xlayer flashblocks gap catch-up completed before applying live block"
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            warn!(
                                                from_block,
                                                to_block,
                                                live_block = block_num,
                                                "Xlayer flashblocks gap catch-up failed before applying live block: {}",
                                                e
                                            );
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(
                                    current_synced,
                                    live_block = block_num,
                                    "Xlayer flashblocks gap catch-up skipped: failed to fetch chain head: {}",
                                    e
                                );
                            }
                        }
                    }

                    // 6. 提取 logs
                    let (logs, block_number, decode_fail_count, decode_failed_addresses) =
                        extract_logs_from_xlayer_flashblock(
                            &fb,
                            &matcher,
                            &mut dedup_cache,
                            &mut parse_cache,
                            &mut tx_tracker,
                            &mut latest_block_timestamp,
                        );

                    let block_num =
                        block_number.unwrap_or_else(|| realtime_head.load(Ordering::Relaxed));

                    // 7. 处理解析失败
                    if decode_fail_count > 0 {
                        warn!(
                            "Xlayer flashblocks log decode failures: payload_id={} index={} count={}",
                            fb.payload_id, fb.index, decode_fail_count
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

                    // 8. 空日志跳过
                    if logs.is_empty() {
                        continue;
                    }
                    let log_count = logs.len();

                    // 9. 应用日志到池子状态
                    match Self::apply_logs_for_block_timed(
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
                        LogSource::XlayerFlashblock,
                    )
                    .await
                    {
                        Ok((affected, _apply_timing)) => {
                            if !affected.is_empty() {
                                let meta = super::build_realtime_update_meta(
                                    &update_seq,
                                    block_num,
                                    received_at,
                                    Some(fb.index),
                                );
                                super::log_realtime_update_applied(meta, affected.len(), log_count);
                                yield Ok((meta, affected));
                            }
                        }
                        Err(e) => {
                            error!("Xlayer flashblocks batch process failed: {}", e);
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
    use alloy::primitives::{address, keccak256, B256};
    use serde_json::{json, Map};

    fn hash_raw_tx(raw: &[u8]) -> String {
        format!("{:#x}", keccak256(raw))
    }

    fn receipt(address: Address, topic0: B256, cumulative_gas: u64) -> serde_json::Value {
        json!({
            "cumulativeGasUsed": format!("0x{cumulative_gas:x}"),
            "logs": [{
                "address": format!("{address:#x}"),
                "topics": [format!("{topic0:#x}")],
                "data": "0x"
            }]
        })
    }

    fn flashblock<'a>(
        payload_id: &'a str,
        index: u64,
        block_number: u64,
        txs: Vec<Vec<u8>>,
        receipts: Map<String, serde_json::Value>,
    ) -> XlayerFlashblockMessage<'a> {
        XlayerFlashblockMessage {
            payload_id: Cow::Borrowed(payload_id),
            index,
            base: None,
            diff: Some(XlayerFlashblockDiff {
                block_hash: None,
                transactions: txs
                    .into_iter()
                    .map(|tx| Cow::Owned(format!("0x{}", alloy::hex::encode(tx))))
                    .collect(),
            }),
            metadata: Some(XlayerFlashblockMetadata {
                block_number: Some(block_number),
                receipts,
            }),
        }
    }

    #[test]
    fn xlayer_lazy_tx_index_correction_preserves_payload_base_across_slices() {
        let pool = address!("1111111111111111111111111111111111111111");
        let topic0 = B256::repeat_byte(0x22);
        let matcher = XlayerLogMatcher {
            topic_addresses: HashSet::from([pool]),
            topic_signatures: HashSet::from([topic0]),
            address_only_addresses: HashSet::new(),
        };
        let mut dedup = XlayerDedupCache::new(XLAYER_DEDUP_PAYLOAD_WINDOW);
        let mut parse_cache = XlayerParseCache::new();
        let mut tx_tracker = XlayerTxCountTracker::new(XLAYER_TX_COUNT_WINDOW);
        let mut latest_block_timestamp = None;

        let payload_id = "0xpayload";
        let block_number = 63368070;

        let slice0_txs = vec![vec![0x01], vec![0x02]];
        let mut slice0_receipts = Map::new();
        slice0_receipts.insert(hash_raw_tx(&slice0_txs[0]), receipt(pool, topic0, 10));
        slice0_receipts.insert(hash_raw_tx(&slice0_txs[1]), receipt(pool, topic0, 20));
        let slice0 = flashblock(payload_id, 0, block_number, slice0_txs, slice0_receipts);

        let (logs0, _, _, _) = extract_logs_from_xlayer_flashblock(
            &slice0,
            &matcher,
            &mut dedup,
            &mut parse_cache,
            &mut tx_tracker,
            &mut latest_block_timestamp,
        );
        assert_eq!(
            logs0
                .iter()
                .map(|log| log.transaction_index.unwrap())
                .collect::<Vec<_>>(),
            vec![0, 1]
        );

        let slice1_txs = vec![vec![0x03], vec![0x04]];
        let mut slice1_receipts = Map::new();
        slice1_receipts.insert(hash_raw_tx(&slice1_txs[0]), receipt(pool, topic0, 30));
        slice1_receipts.insert(hash_raw_tx(&slice1_txs[1]), receipt(pool, topic0, 40));
        let slice1 = flashblock(payload_id, 1, block_number, slice1_txs, slice1_receipts);

        let (logs1, _, _, _) = extract_logs_from_xlayer_flashblock(
            &slice1,
            &matcher,
            &mut dedup,
            &mut parse_cache,
            &mut tx_tracker,
            &mut latest_block_timestamp,
        );

        assert_eq!(
            logs1
                .iter()
                .map(|log| log.transaction_index.unwrap())
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
    }
}

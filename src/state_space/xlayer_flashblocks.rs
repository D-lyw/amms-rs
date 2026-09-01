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
use crate::amms::amm::AMM;
use crate::amms::binaryfi_prop::{enrich_update_log_data, BINARYFI_UPDATE_EVENT};
use crate::amms::caliber_prop::{
    decode_batch_update_parameters, decode_caliber_swap_log, extract_input_from_raw_tx,
    extract_to_from_raw_tx, CaliberBatchUpdate, CaliberSwapEvent, CALIBER_SWAP_EVENT,
};
use crate::amms::elfomo_prop::{ElfomoFiPropPool, ELFOMO_UPDATE_EVENT};
use crate::state_space::{
    STREAM_IDLE_TIMEOUT, STREAM_RECONNECT_DELAY, XLAYER_FLASHBLOCKS_RAW_WS_URL,
};
use alloy::network::Network;
use alloy::primitives::{Address, Bytes, FixedBytes, LogData, B256, U256};
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

/// 从 receipt 中提取交易执行结果：`status == "0x1"` 成功、`"0x0"` 回滚；
/// 字段缺失或无法解析返回 `None`（调用方按"未确认"处理）。
fn parse_receipt_status(v: &serde_json::Value) -> Option<bool> {
    match v.get("status") {
        Some(serde_json::Value::String(s)) => {
            Some(u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()? == 1)
        }
        Some(serde_json::Value::Number(n)) => n.as_u64().map(|n| n == 1),
        _ => None,
    }
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

/// 从 flashblocks 原始交易中提取的 caliber 报价更新事件。
///
/// `tx_index` 为块内全局索引（跨 slice 由 `XlayerTxCountTracker` 的 `tx_base`
/// 拼接，与懒排序修正使用同一约定）；`contract` 为被调用的 caliber 合约地址，
/// 供路由层推导 virtual_address。
#[derive(Debug)]
pub(crate) struct CaliberTxEvent {
    pub contract: Address,
    pub tx_index: u64,
    pub update: CaliberBatchUpdate,
}

/// 从 flashblocks 原始交易中提取的 ElfomoFi `updatePrices` 报价更新事件。
///
/// `tx_index` 为块内全局索引（跨 slice 由 `XlayerTxCountTracker` 的 `tx_base`
/// 拼接，与懒排序修正使用同一约定）；`pool` 为被调用的 Pool 地址；
/// `seed` 为 calldata 参数高 32 位价格种子（`a`），可直接本地重算 orderbook。
#[derive(Debug)]
pub(crate) struct ElfomoTxEvent {
    pub pool: Address,
    pub seed: U256,
    pub tx_index: u64,
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
/// 返回 (匹配的日志, block_number, 解析失败数, 解析失败的地址集合,
/// caliber 报价更新事件, caliber swap 事件)
#[allow(clippy::type_complexity)]
fn extract_logs_from_xlayer_flashblock(
    fb: &XlayerFlashblockMessage,
    matcher: &XlayerLogMatcher,
    binaryfi_engines: &HashSet<Address>,
    caliber_contracts: &HashSet<Address>,
    elfomo_pools: &HashSet<Address>,
    dedup_cache: &mut XlayerDedupCache,
    parse_cache: &mut XlayerParseCache,
    tx_tracker: &mut XlayerTxCountTracker,
    latest_block_timestamp: &mut Option<(u64, u64)>,
) -> (
    Vec<Log>,
    Option<u64>,
    usize,
    HashSet<Address>,
    Vec<CaliberTxEvent>,
    Vec<CaliberSwapEvent>,
    Vec<ElfomoTxEvent>,
) {
    let mut out = Vec::new();
    let mut decode_fail = 0usize;
    let mut decode_failed_addresses = HashSet::new();
    let mut caliber_events = Vec::new();
    let mut caliber_swap_events = Vec::new();
    let mut elfomo_updates = Vec::new();

    // 1. 获取 block_number（metadata 数值优先，base hex 备选）
    let block_number = xlayer_flashblock_block_number(fb);

    let Some(block_number) = block_number else {
        return (
            out,
            None,
            decode_fail,
            decode_failed_addresses,
            caliber_events,
            caliber_swap_events,
            elfomo_updates,
        );
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

    // 2b. 块内全局 tx_base：caliber 原始交易提取与懒排序共用同一约定
    let tx_base = tx_tracker.base(&fb.payload_id);

    // 2c. caliber 报价更新提取：更新交易 0 日志，只能从 diff.transactions 原始
    //     交易发现。先轻量 RLP 定位 to（只跳字段、零分配，~100-200ns/笔），
    //     过滤出目标合约交易；命中后才完整解码 calldata + 标准 ABI 解码
    //     （每块 ~14 笔通常只有 1-2 笔命中，避免对无关交易做全量解码）。
    //     失败/未确认交易过滤（P0，2026-08-09 事故 0x914e39…）：caliber 更新
    //     无事件，只能靠 receipt status 判定是否真正落地——链上回滚
    //     （status=0x0）或 receipt 缺失（未确认）的更新一律不应用，否则失败
    //     交易的 deadline 会作为"幻影报价"喂饱本地时效门控，产生幻影套利机会。
    //     对账任务仍作为低频兜底（只读链上真实存储，天然免疫）。
    if !caliber_contracts.is_empty() {
        if let Some(diff) = fb.diff.as_ref() {
            for (real_idx, raw_tx_hex) in diff.transactions.iter().enumerate() {
                let raw_hex = raw_tx_hex.strip_prefix("0x").unwrap_or(raw_tx_hex);
                let Ok(raw) = alloy::hex::decode(raw_hex) else {
                    continue;
                };
                // 第一步：轻量定位 to，过滤非目标交易
                let Some(to) = extract_to_from_raw_tx(&raw) else {
                    continue;
                };
                if !caliber_contracts.contains(&to) {
                    continue;
                }
                // 第二步：receipt status 校验——仅应用链上确认成功的更新。
                // metadata.receipts 以 tx hash 为键，raw 交易字节的 keccak256
                // 即为 tx hash（与懒排序修正同一约定）。
                let confirmed = fb
                    .metadata
                    .as_ref()
                    .and_then(|m| {
                        m.receipts
                            .get(&format!("{:#x}", alloy::primitives::keccak256(&raw)))
                    })
                    .and_then(parse_receipt_status)
                    .unwrap_or(false);
                if !confirmed {
                    continue;
                }
                // 第三步：命中后才取 calldata（完整 RLP 解码 + 拷贝）
                let Some(input) = extract_input_from_raw_tx(&raw) else {
                    continue;
                };
                let Some(updates) = decode_batch_update_parameters(&input) else {
                    continue;
                };
                // diff.transactions 数组下标 + tx_base = 块内全局 tx_index
                // （EVM 语义：块内后者覆盖前者）。
                let tx_index = tx_base + real_idx as u64;
                for update in updates {
                    caliber_events.push(CaliberTxEvent {
                        contract: to,
                        tx_index,
                        update,
                    });
                }
            }
        }
    }

    // 2d. ElfomoFi updatePrices 原始交易主通道（本地直算，零 RPC）：
    //     calldata 参数就是价格种子 `a = arg >> 32`（2026-09-01 链上实证，
    //     实测 `arg ≈ (a<<32) | (ts-1)`），解析后调用方本地重算 orderbook，
    //     不再需要重拉 getOrderbook。
    //     receipt status 校验：仅应用链上确认成功的更新（失败/回滚不触发）。
    if !elfomo_pools.is_empty() {
        if let Some(diff) = fb.diff.as_ref() {
            for (real_idx, raw_tx_hex) in diff.transactions.iter().enumerate() {
                let raw_hex = raw_tx_hex.strip_prefix("0x").unwrap_or(raw_tx_hex);
                let Ok(raw) = alloy::hex::decode(raw_hex) else {
                    continue;
                };
                // 轻量定位 to（只跳字段、零分配），过滤非目标 Pool 交易
                let Some(to) = extract_to_from_raw_tx(&raw) else {
                    continue;
                };
                if !elfomo_pools.contains(&to) {
                    continue;
                }
                let confirmed = fb
                    .metadata
                    .as_ref()
                    .and_then(|m| {
                        m.receipts
                            .get(&format!("{:#x}", alloy::primitives::keccak256(&raw)))
                    })
                    .and_then(parse_receipt_status)
                    .unwrap_or(false);
                if !confirmed {
                    continue;
                }
                let Some(input) = extract_input_from_raw_tx(&raw) else {
                    continue;
                };
                let Some(seed) = ElfomoFiPropPool::parse_update_prices_calldata(&input) else {
                    continue;
                };
                elfomo_updates.push(ElfomoTxEvent {
                    pool: to,
                    seed,
                    tx_index: tx_base + real_idx as u64,
                });
            }
        }
    }

    // 3. 获取 metadata（receipts 从这里提取）
    let Some(metadata) = fb.metadata.as_ref() else {
        return (
            out,
            Some(block_number),
            decode_fail,
            decode_failed_addresses,
            caliber_events,
            caliber_swap_events,
            elfomo_updates,
        );
    };

    if metadata.receipts.is_empty() {
        return (
            out,
            Some(block_number),
            decode_fail,
            decode_failed_addresses,
            caliber_events,
            caliber_swap_events,
            elfomo_updates,
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

        // caliber swap 事件提取（2026-08-11 新增，驱动 ladder 消费同步）：
        // 与 caliber 更新事件同一 P0 纪律——仅应用 receipt status==0x1 的
        // 已确认交易，回滚/未确认的 swap 一律不应用（失败交易的 amountOut
        // 会作为"幻影消费"污染本地 pos/储备，产生幻影报价）。
        // 先于通用日志循环提取并占用 dedup 键：swap 日志不属于任何池子的
        // sync_events（caliber 合约不在 log matcher 地址集合），通用循环
        // 本就会在预筛阶段丢弃，占用 dedup 键避免跨 slice 重复应用。
        if !caliber_contracts.is_empty() && parse_receipt_status(receipt_value) == Some(true) {
            for (log_idx, raw_log) in receipt_logs.iter().enumerate() {
                let Ok(log_addr) = Address::from_str(&raw_log.address) else {
                    continue;
                };
                if !caliber_contracts.contains(&log_addr) {
                    continue;
                }
                let mut topics: Vec<B256> = Vec::with_capacity(raw_log.topics.len());
                let mut topics_ok = true;
                for t in &raw_log.topics {
                    match B256::from_str(t) {
                        Ok(p) => topics.push(p),
                        Err(_) => {
                            topics_ok = false;
                            break;
                        }
                    }
                }
                if !topics_ok || topics.first() != Some(&CALIBER_SWAP_EVENT) {
                    continue;
                }
                let Ok(data) = Bytes::from_str(&raw_log.data) else {
                    continue;
                };
                let Some(mut ev) = decode_caliber_swap_log(&topics, data.as_ref()) else {
                    continue;
                };
                if !dedup_cache.insert(&fb.payload_id, _tx_hash, log_idx as u64) {
                    continue;
                }
                ev.contract = log_addr;
                ev.tx_index = transaction_index;
                caliber_swap_events.push(ev);
            }
        }

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

            // 5g. 构造 LogData（engine 地址按已注册池子实例配置匹配）
            let is_binaryfi_update_log = binaryfi_engines.contains(&address)
                && topics.first() == Some(&BINARYFI_UPDATE_EVENT);
            let Some(log_data) = LogData::new(topics, data) else {
                decode_fail += 1;
                decode_failed_addresses.insert(address);
                continue;
            };

            // 5h. 解析 tx_hash
            let tx_hash_parsed = _tx_hash.parse::<B256>().ok();

            // 5h1. BinaryFi 引擎 update 日志增强：raw tx 解析 price 注入 data
            //       （找不到 raw bytes 或解码失败时保留原始日志）
            let final_log_data = if is_binaryfi_update_log {
                let raw_txs = fb
                    .diff
                    .as_ref()
                    .map(|d| d.transactions.as_slice())
                    .unwrap_or(&[]);
                enrich_update_log_data(raw_txs, tx_hash_parsed, &log_data, address)
                    .unwrap_or(log_data)
            } else {
                log_data
            };

            // 5i. 构造 Alloy Log
            let log = Log {
                inner: alloy::primitives::Log {
                    address,
                    data: final_log_data,
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

    // 8. ElfomoFi 更新事件去重：updatePrices 空事件与其原始交易同块共存。
    //    raw-tx 已携带价格种子（本地直算通道），空事件不再触发 AsyncUpdate，
    //    直接剔除，避免冗余 RPC 重拉。
    if !elfomo_updates.is_empty() {
        let updated_pools: HashSet<Address> = elfomo_updates.iter().map(|e| e.pool).collect();
        out.retain(|log| {
            !(updated_pools.contains(&log.inner.address)
                && log.topics().first() == Some(&ELFOMO_UPDATE_EVENT))
        });
    }

    (
        out,
        Some(block_number),
        decode_fail,
        decode_failed_addresses,
        caliber_events,
        caliber_swap_events,
        elfomo_updates,
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
            // 已注册 BinaryFi 池子的 engine 地址（update 日志 raw-tx 增强用，配置化支持多部署）
            let binaryfi_engines: HashSet<Address> = {
                let read_guard = state.read().await;
                read_guard
                    .state
                    .values()
                    .filter_map(|amm| match amm.as_ref() {
                        AMM::BinaryFiPropPool(p) => Some(p.engine_address),
                        _ => None,
                    })
                    .collect()
            };
            // 已注册 Caliber 池子的合约地址（更新交易 0 日志，按 to 地址过滤原始交易）
            let caliber_contracts: HashSet<Address> = {
                let read_guard = state.read().await;
                read_guard
                    .state
                    .values()
                    .filter_map(|amm| match amm.as_ref() {
                        AMM::CaliberPropPool(p) => Some(p.contract_address),
                        _ => None,
                    })
                    .collect()
            };
            // 已注册 ElfomoFi 池子的 Pool 地址（updatePrices raw-tx 兜底，按 to 过滤）
            let elfomo_pools: HashSet<Address> = {
                let read_guard = state.read().await;
                read_guard
                    .state
                    .values()
                    .filter_map(|amm| match amm.as_ref() {
                        AMM::ElfomoFiPropPool(p) => Some(p.pool_address),
                        _ => None,
                    })
                    .collect()
            };
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

                    // 6. 提取 logs + caliber 报价更新事件 + caliber swap 事件
                    //    + elfomo updatePrices 原始交易
                    let (
                        logs,
                        block_number,
                        decode_fail_count,
                        decode_failed_addresses,
                        caliber_events,
                        caliber_swap_events,
                        elfomo_updates,
                    ) =
                        extract_logs_from_xlayer_flashblock(
                            &fb,
                            &matcher,
                            &binaryfi_engines,
                            &caliber_contracts,
                            &elfomo_pools,
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

                    // 8. 无日志且无 caliber 更新/swap 且无 elfomo 更新 → 跳过
                    if logs.is_empty()
                        && caliber_events.is_empty()
                        && caliber_swap_events.is_empty()
                        && elfomo_updates.is_empty()
                    {
                        continue;
                    }
                    let log_count = logs.len();

                    let mut affected: Vec<Address> = Vec::new();

                    // 9. 应用日志到池子状态（原有路径）
                    if !logs.is_empty() {
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
                            Ok((affected_logs, _apply_timing)) => {
                                affected.extend(affected_logs);
                            }
                            Err(e) => {
                                error!("Xlayer flashblocks batch process failed: {}", e);
                            }
                        }
                    }

                    // 10. 应用 caliber 实时报价更新（无日志交易，由原始交易驱动）
                    if !caliber_events.is_empty() {
                        match Self::apply_caliber_updates_for_block(
                            &state,
                            block_num,
                            caliber_events,
                            &realtime_head,
                        )
                        .await
                        {
                            Ok(affected_caliber) => {
                                affected.extend(affected_caliber);
                            }
                            Err(e) => {
                                error!(
                                    "Xlayer flashblocks caliber update apply failed: {}",
                                    e
                                );
                            }
                        }
                    }

                    // 11. 应用 caliber swap 事件（日志驱动，ladder 消费同步）
                    if !caliber_swap_events.is_empty() {
                        match Self::apply_caliber_swaps_for_block(
                            &state,
                            block_num,
                            caliber_swap_events,
                            &realtime_head,
                        )
                        .await
                        {
                            Ok(affected_swaps) => {
                                affected.extend(affected_swaps);
                            }
                            Err(e) => {
                                error!("Xlayer flashblocks caliber swap apply failed: {}", e);
                            }
                        }
                    }

                    // 12. ElfomoFi updatePrices 原始交易 → 本地直算 orderbook
                    //     （calldata 携带价格种子，apply_price_seed 按本地金库
                    //     余额重算，零 RPC；同块空事件已在提取侧剔除，避免
                    //     冗余 AsyncUpdate。仅事件无 raw-tx 时由 L1 通道回退）
                    if !elfomo_updates.is_empty() {
                        match Self::apply_elfomo_updates_for_block(
                            &state,
                            block_num,
                            elfomo_updates,
                            &realtime_head,
                        )
                        .await
                        {
                            Ok(affected_elfomo) => {
                                affected.extend(affected_elfomo);
                            }
                            Err(e) => {
                                error!("Xlayer flashblocks elfomo update apply failed: {}", e);
                            }
                        }
                    }

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

                sleep(STREAM_RECONNECT_DELAY).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, b256, keccak256, B256, U256};
    use serde_json::{json, Map};

    /// 真实 caliber 更新交易（XLayer 块 67329558，tx 0xd9a1ffba…，
    /// EIP-1559，to=0x154586b2…，5 个 pair 的 batchUpdateParameters calldata）
    const REAL_CALIBER_RAW_TX_HEX: &str = "02f9033481c483129664832dc6c08401c9c3808301d4c094154586b2479b9a11e3d4db90024dc0e26f09731280b902c4008dcc8e00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000005d2ba36ae7a49fbbb15ac04a76531d9e811ca5fe2e57f4c559f200ed2a57aac7a0000000000000000000000000000000000000000000000000000000efaaa31bf0000000000000000000000000000000000000000000000000000000000000483000000000000000000000000000000000000000000000000000000006a75b3a0f4b05af384ac756330659972e8584851916c39bf13414abd632dc7c11ee792380000000000000000000000000000000000000000000000000000001b318644c0000000000000000000000000000000000000000000000000000000000000043d000000000000000000000000000000000000000000000000000000006a75b3a0b2d5c47f635aa119fc5e911aa881db33bc77b61a1872d035d6122869e24a96730000000000000000000000000000000000000000000000000000003338d4970100000000000000000000000000000000000000000000000000000000000003b1000000000000000000000000000000000000000000000000000000006a75b3a0304e5bfc144bd0991c990cbbe6488660faf1f6be58a8afb15f3330c8a01599880000000000000000000000000000000000000000000000000000012de1e46dc000000000000000000000000000000000000000000000000000000000000005e3000000000000000000000000000000000000000000000000000000006a75b3a0de4c3cddfd81d8ee19634d5d62f07681bf28fdc2c622a1bbdb276d3359053ddf0000000000000000000000000000000000000000000000000000002127ffdb40000000000000000000000000000000000000000000000000000000000000042e000000000000000000000000000000000000000000000000000000006a75b3a0c001a07ed1485c2f6ace2104a384b0e596f9a39729450002b77b23bd1e4ab10ea24512a0179baa7e6f42f7e616b59046768d8a81c12d3c0653338501c4855584046f291e";

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

        let (logs0, _, _, _, _, _, _) = extract_logs_from_xlayer_flashblock(
            &slice0,
            &matcher,
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::new(),
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

        let (logs1, _, _, _, _, _, _) = extract_logs_from_xlayer_flashblock(
            &slice1,
            &matcher,
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::new(),
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

    #[test]
    fn xlayer_caliber_update_extraction_from_raw_tx() {
        use alloy::hex;

        let caliber_contract: Address = "0x154586b2479b9a11e3d4db90024dc0e26f097312"
            .parse()
            .unwrap();
        // 真实更新交易（块 67329558，tx 0xd9a1ffba…，EIP-1559，5 pair）
        let raw_tx = hex::decode(REAL_CALIBER_RAW_TX_HEX).unwrap();

        let matcher = XlayerLogMatcher {
            topic_addresses: HashSet::new(),
            topic_signatures: HashSet::new(),
            address_only_addresses: HashSet::new(),
        };
        let mut dedup = XlayerDedupCache::new(XLAYER_DEDUP_PAYLOAD_WINDOW);
        let mut parse_cache = XlayerParseCache::new();
        let mut tx_tracker = XlayerTxCountTracker::new(XLAYER_TX_COUNT_WINDOW);
        let mut latest_block_timestamp = None;

        // 同块两笔更新交易：tx_index 应为 0 和 1（tx_base=0 + 数组下标）
        let mut receipts = Map::new();
        receipts.insert(
            hash_raw_tx(&raw_tx),
            json!({"status": "0x1", "cumulativeGasUsed": "0x1", "logs": []}),
        );
        let fb = flashblock(
            "0xcaliber",
            0,
            67329558,
            vec![raw_tx.clone(), raw_tx.clone()],
            receipts,
        );

        let (logs, _, _, _, events, _swap_events, _) = extract_logs_from_xlayer_flashblock(
            &fb,
            &matcher,
            &HashSet::new(),
            &HashSet::from([caliber_contract]),
            &HashSet::new(),
            &mut dedup,
            &mut parse_cache,
            &mut tx_tracker,
            &mut latest_block_timestamp,
        );
        assert!(logs.is_empty());
        assert_eq!(events.len(), 10); // 2 笔 × 5 pair
        assert_eq!(events[0].tx_index, 0);
        assert_eq!(events[5].tx_index, 1);
        assert_eq!(events[0].contract, caliber_contract);
        assert_eq!(events[0].update.pair_id, events[5].update.pair_id);
        assert_eq!(events[0].update.price, U256::from(64_334_999_999u64));
        assert_eq!(events[0].update.flags, 1155);
        assert_eq!(events[0].update.deadline, 1_786_098_592);

        // to 不在兴趣集合 → 不产出事件
        let (_, _, _, _, events2, _swap_events2, _) = extract_logs_from_xlayer_flashblock(
            &fb,
            &matcher,
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::new(),
            &mut dedup,
            &mut parse_cache,
            &mut tx_tracker,
            &mut latest_block_timestamp,
        );
        assert!(events2.is_empty());
    }

    #[test]
    fn xlayer_caliber_reverted_or_unconfirmed_tx_not_extracted() {
        use alloy::hex;

        let caliber_contract: Address = "0x154586b2479b9a11e3d4db90024dc0e26f097312"
            .parse()
            .unwrap();
        let raw_tx = hex::decode(REAL_CALIBER_RAW_TX_HEX).unwrap();

        let matcher = XlayerLogMatcher {
            topic_addresses: HashSet::new(),
            topic_signatures: HashSet::new(),
            address_only_addresses: HashSet::new(),
        };
        let mut dedup = XlayerDedupCache::new(XLAYER_DEDUP_PAYLOAD_WINDOW);
        let mut parse_cache = XlayerParseCache::new();
        let mut tx_tracker = XlayerTxCountTracker::new(XLAYER_TX_COUNT_WINDOW);
        let mut latest_block_timestamp = None;

        // status=0x0（链上回滚，如 2026-08-09 事故的 MM 更新）→ 不产出事件
        let mut reverted = Map::new();
        reverted.insert(
            hash_raw_tx(&raw_tx),
            json!({"status": "0x0", "cumulativeGasUsed": "0x1", "logs": []}),
        );
        let fb_reverted = flashblock("0xcaliber", 0, 67329558, vec![raw_tx.clone()], reverted);

        let (_, _, _, _, events, _swap_events, _) = extract_logs_from_xlayer_flashblock(
            &fb_reverted,
            &matcher,
            &HashSet::new(),
            &HashSet::from([caliber_contract]),
            &HashSet::new(),
            &mut dedup,
            &mut parse_cache,
            &mut tx_tracker,
            &mut latest_block_timestamp,
        );
        assert!(
            events.is_empty(),
            "reverted caliber update must not produce events"
        );

        // receipt 缺失（未确认）→ 不产出事件
        let fb_missing = flashblock("0xcaliber", 0, 67329558, vec![raw_tx.clone()], Map::new());
        let (_, _, _, _, events, _swap_events, _) = extract_logs_from_xlayer_flashblock(
            &fb_missing,
            &matcher,
            &HashSet::new(),
            &HashSet::from([caliber_contract]),
            &HashSet::new(),
            &mut dedup,
            &mut parse_cache,
            &mut tx_tracker,
            &mut latest_block_timestamp,
        );
        assert!(
            events.is_empty(),
            "unconfirmed caliber update must not produce events"
        );
    }

    #[test]
    fn xlayer_caliber_extract_to_apply_end_to_end() {
        use crate::amms::caliber_prop::CaliberPropPool;
        use crate::amms::Token;

        let caliber_contract: Address = "0x154586b2479b9a11e3d4db90024dc0e26f097312"
            .parse()
            .unwrap();
        // 真实更新 calldata 的第一个 pair（price=64334999999, flags=1155, deadline=1786098592）
        let pair_id: B256 = B256::from_slice(
            &alloy::hex::decode("d2ba36ae7a49fbbb15ac04a76531d9e811ca5fe2e57f4c559f200ed2a57aac7a")
                .unwrap(),
        );
        let virtual_address =
            CaliberPropPool::virtual_address_from_pair_id(pair_id, caliber_contract);

        let mut state = StateSpace::default();
        state.insert_amm(AMM::CaliberPropPool(CaliberPropPool {
            contract_address: caliber_contract,
            pair_id,
            virtual_address,
            token_x: Address::from([1u8; 20]),
            token_y: Address::from([2u8; 20]),
            token_a: Token::new_with_decimals(Address::from([1u8; 20]), 18),
            token_b: Token::new_with_decimals(Address::from([2u8; 20]), 18),
            created_block: 0,
            last_synced_block: 0,
            reserve_a: U256::from(1_000),
            reserve_b: U256::from(1_000),
            ladder: Default::default(),
            price_a_in_b: 1.0,
            price_b_in_a: 1.0,
        }));

        let raw_tx = alloy::hex::decode(REAL_CALIBER_RAW_TX_HEX).unwrap();
        let mut receipts = Map::new();
        receipts.insert(
            hash_raw_tx(&raw_tx),
            json!({"status": "0x1", "cumulativeGasUsed": "0x1", "logs": []}),
        );
        let fb = flashblock("0xcaliber", 0, 67329558, vec![raw_tx], receipts);
        let matcher = XlayerLogMatcher {
            topic_addresses: HashSet::new(),
            topic_signatures: HashSet::new(),
            address_only_addresses: HashSet::new(),
        };
        let mut dedup = XlayerDedupCache::new(XLAYER_DEDUP_PAYLOAD_WINDOW);
        let mut parse_cache = XlayerParseCache::new();
        let mut tx_tracker = XlayerTxCountTracker::new(XLAYER_TX_COUNT_WINDOW);
        let mut latest_block_timestamp = None;

        // 提取：真实 flashblock 原始交易 → caliber 事件
        let (_, _, _, _, events, _swap_events, _) = extract_logs_from_xlayer_flashblock(
            &fb,
            &matcher,
            &HashSet::new(),
            &HashSet::from([caliber_contract]),
            &HashSet::new(),
            &mut dedup,
            &mut parse_cache,
            &mut tx_tracker,
            &mut latest_block_timestamp,
        );
        assert_eq!(events.len(), 5);

        // 路由 + 应用：pool.field0/field1/deadline 与链上 data+0 一致
        let affected = state.apply_caliber_updates(&events, 67_329_558);
        assert_eq!(affected, vec![virtual_address]);
        let pool = match state.get(&virtual_address).unwrap() {
            AMM::CaliberPropPool(p) => p,
            _ => unreachable!(),
        };
        assert_eq!(pool.ladder.field0, U256::from(64_334_999_999u64));
        assert_eq!(pool.ladder.field1, U256::from(1155u64));
        assert_eq!(pool.ladder.deadline, 1_786_098_592);
        assert_eq!(pool.last_synced_block, 67_329_558);

        // 其余 4 个 pair 不在本地池子 → 静默跳过，不影响已应用状态
        assert_eq!(affected, vec![virtual_address]);
    }

    // ── RLP 编码小工具（合成 legacy raw tx，供 elfomo 提取测试用）──
    fn rlp_len_prefix(payload: &[u8], short: u8, long: u8) -> Vec<u8> {
        let n = payload.len();
        if n <= 55 {
            vec![short + n as u8]
        } else {
            let nb = ((64 - (n as u64).leading_zeros()) + 7) / 8; // 长度字节数
            let mut out = vec![long + nb as u8];
            for i in (0..nb).rev() {
                out.push(((n as u64) >> (8 * i)) as u8);
            }
            out
        }
    }

    fn rlp_item(payload: &[u8]) -> Vec<u8> {
        let mut out = rlp_len_prefix(payload, 0x80, 0xb7);
        out.extend_from_slice(payload);
        out
    }

    fn rlp_list(items: &[Vec<u8>]) -> Vec<u8> {
        let payload: Vec<u8> = items.iter().flatten().copied().collect();
        let mut out = rlp_len_prefix(&payload, 0xc0, 0xf7);
        out.extend_from_slice(&payload);
        out
    }

    fn rlp_u64(v: u64) -> Vec<u8> {
        if v == 0 {
            return vec![0x80];
        }
        let bytes = v.to_be_bytes();
        let first = bytes.iter().position(|b| *b != 0).unwrap_or(7);
        rlp_item(&bytes[first..])
    }

    fn rlp_addr(a: Address) -> Vec<u8> {
        let mut out = vec![0x94];
        out.extend_from_slice(a.as_slice());
        out
    }

    /// ElfomoFi `updatePrices` raw-tx → 种子提取 → 本地直算 orderbook 端到端：
    /// calldata 高 32 位即价格种子；同块空 data 更新事件被过滤（避免冗余
    /// AsyncUpdate）；apply_price_seed 用本地金库余额重算整本 orderbook。
    #[test]
    fn xlayer_elfomo_update_extraction_to_apply_end_to_end() {
        use crate::amms::elfomo_prop::ElfomoFiPropPool;
        use crate::amms::Token;

        let pool = address!("02dcdf4171939ac0fe28e48e8758649311e9459a");
        let a: U256 = U256::from(0x143c60fu64);
        // 真实形态：arg ≈ (a<<32) | (ts-1)
        let arg: U256 = (a << 32) | U256::from(0x6a96bd30u64);
        let mut calldata = vec![0xae, 0x7e, 0x8d, 0x81];
        calldata.extend_from_slice(&arg.to_be_bytes::<32>());

        // 合成 legacy raw tx：to=Pool，data=updatePrices calldata
        let fields = vec![
            rlp_u64(0),             // nonce
            rlp_u64(1_000_000_000), // gasPrice
            rlp_u64(300_000),       // gasLimit
            rlp_addr(pool),         // to
            rlp_u64(0),             // value
            rlp_item(&calldata),    // data
            vec![27],               // v
            vec![1],                // r
            vec![1],                // s
        ];
        let raw_tx = rlp_list(&fields);

        let mut receipts = Map::new();
        receipts.insert(
            hash_raw_tx(&raw_tx),
            json!({
                "status": "0x1",
                "cumulativeGasUsed": "0x1",
                "logs": [{
                    "address": format!("{pool:#x}"),
                    "topics": [format!("{:#x}", ELFOMO_UPDATE_EVENT)],
                    "data": "0x"
                }]
            }),
        );
        let fb = flashblock("0xelfomo", 0, 69_452_472, vec![raw_tx], receipts);
        let matcher = XlayerLogMatcher {
            topic_addresses: HashSet::from([pool]),
            topic_signatures: HashSet::from([ELFOMO_UPDATE_EVENT]),
            address_only_addresses: HashSet::new(),
        };
        let mut dedup = XlayerDedupCache::new(XLAYER_DEDUP_PAYLOAD_WINDOW);
        let mut parse_cache = XlayerParseCache::new();
        let mut tx_tracker = XlayerTxCountTracker::new(XLAYER_TX_COUNT_WINDOW);
        let mut latest_block_timestamp = None;

        // 提取：raw-tx → ElfomoTxEvent（带种子）；同块空事件被过滤
        let (logs, _, _, _, _, _, elfomo_events) = extract_logs_from_xlayer_flashblock(
            &fb,
            &matcher,
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::from([pool]),
            &mut dedup,
            &mut parse_cache,
            &mut tx_tracker,
            &mut latest_block_timestamp,
        );
        assert_eq!(elfomo_events.len(), 1);
        assert_eq!(elfomo_events[0].pool, pool);
        assert_eq!(elfomo_events[0].seed, a);
        assert_eq!(elfomo_events[0].tx_index, 0);
        assert!(logs.is_empty(), "update 空事件应被 raw-tx 通道过滤");

        // 路由 + 应用：apply_price_seed 用本地金库余额重算 orderbook（零 RPC）
        let mut state = StateSpace::default();
        state.insert_amm(AMM::ElfomoFiPropPool(ElfomoFiPropPool {
            pool_address: pool,
            token_x: address!("e7b000003a45145decf8a28fc755ad5ec5ea025a"),
            token_y: address!("779ded0c9e1022225f8e0630b35a9b54be713736"),
            factory_address: address!("ffffffbb2d432b8acb4c57d556c0c721a431d038"),
            router_address: address!("f0f0f0f0fb0d738452efd03a28e8be14c76d5f73"),
            vault_address: address!("bb1b19f138db3925883a96ff7a304277460e0c99"),
            chain_id: 196,
            created_block: 0,
            last_synced_block: 0,
            price_seed: U256::ZERO,
            tokens: vec![
                Token::new_with_decimals(address!("e7b000003a45145decf8a28fc755ad5ec5ea025a"), 18),
                Token::new_with_decimals(address!("779ded0c9e1022225f8e0630b35a9b54be713736"), 6),
            ],
            levels: crate::amms::elfomo_prop::types::OrderbookSnapshot {
                from_to_levels: vec![],
                to_from_levels: vec![],
                vault_usdt0: U256::from(19_192_415_254u64),
                vault_xeth: U256::from(2_940_462_501_000_862_186u128),
                price_seed: U256::ZERO,
            },
            consumed: Default::default(),
        }));

        let affected = state.apply_elfomo_updates(&elfomo_events, 69_452_472);
        assert_eq!(affected, vec![pool]);
        let pool_obj = match state.get(&pool).unwrap() {
            AMM::ElfomoFiPropPool(p) => p,
            _ => unreachable!(),
        };
        assert_eq!(pool_obj.price_seed, a);
        assert_eq!(pool_obj.last_synced_block, 69_452_472);
        // 与块 0x423c2b8 链上 getOrderbook 逐位一致（种子+金库余额纯函数）
        assert_eq!(
            pool_obj.levels.from_to_levels[0].size,
            U256::from(600_000_000_000_000_000u128)
        );
        assert_eq!(
            pool_obj.levels.from_to_levels[0].price,
            U256::from(2_473_060_529_144_115u128)
        );
        assert_eq!(
            pool_obj.levels.to_from_levels[1].size,
            U256::from(1_740_462_501_000_862_186u128)
        );
        assert_eq!(
            pool_obj.levels.to_from_levels[1].price,
            U256::from(2_474_964_919_058_850u128)
        );
    }

    /// caliber swap 事件提取：真实日志（块 67650064 tx#15 W→U）、
    /// status=0x0 回滚过滤、非目标合约过滤。
    #[test]
    fn xlayer_caliber_swap_event_extraction() {
        let caliber_contract = address!("0x154586b2479b9a11e3d4db90024dc0e26f097312");
        let pair_id = b256!("b2d5c47f635aa119fc5e911aa881db33bc77b61a1872d035d6122869e24a9673");
        let caller = b256!("000000000000000000000000311350ded40088b8504bb67a7d5974e9da287bd1");
        let swap_data = "0x000000000000000000000000a8ddb5cd96b5222afe198316e9a57caa642850d5000000000000000000000000779ded0c9e1022225f8e0630b35a9b54be7137360000000000000000000000000000000000000000000000001a600c3aa3bd69ec0000000000000000000000000000000000000000000000000000000018d82d010000000000000000000000000000000000000000000000000000000000000002";

        let make_receipt = |status: &str| {
            json!({
                "cumulativeGasUsed": "0x10",
                "status": status,
                "logs": [{
                    "address": format!("{caliber_contract:#x}"),
                    "topics": [
                        format!("{:#x}", CALIBER_SWAP_EVENT),
                        format!("{pair_id:#x}"),
                        format!("{caller:#x}"),
                    ],
                    "data": swap_data
                }]
            })
        };

        let raw_tx = vec![0x02u8, 0x01, 0x02];
        let mut receipts = Map::new();
        receipts.insert(hash_raw_tx(&raw_tx), make_receipt("0x1"));
        let fb = flashblock("0xswap", 0, 67_650_064, vec![raw_tx.clone()], receipts);

        let matcher = XlayerLogMatcher::from_query_chunks(&[]);
        let mut dedup = XlayerDedupCache::new(XLAYER_DEDUP_PAYLOAD_WINDOW);
        let mut parse_cache = XlayerParseCache::new();
        let mut tx_tracker = XlayerTxCountTracker::new(XLAYER_TX_COUNT_WINDOW);
        let mut latest_block_timestamp = None;
        let (_, _, _, _, _, swap_events, _) = extract_logs_from_xlayer_flashblock(
            &fb,
            &matcher,
            &HashSet::new(),
            &HashSet::from([caliber_contract]),
            &HashSet::new(),
            &mut dedup,
            &mut parse_cache,
            &mut tx_tracker,
            &mut latest_block_timestamp,
        );
        assert_eq!(swap_events.len(), 1);
        let ev = &swap_events[0];
        assert_eq!(ev.contract, caliber_contract);
        assert_eq!(ev.tx_index, 0);
        assert_eq!(ev.pair_id, pair_id);
        assert_eq!(
            ev.token_in,
            address!("0xa8ddb5cd96b5222afe198316e9a57caa642850d5")
        );
        assert_eq!(
            ev.token_out,
            address!("0x779ded0c9e1022225f8e0630b35a9b54be713736")
        );
        assert_eq!(ev.amount_in, U256::from(1_900_532_488_745_085_420u64));
        assert_eq!(ev.amount_out, U256::from(416_820_481u64));

        // status=0x0（回滚）→ 不产出 swap 事件（P0：失败交易不得污染状态）
        let mut receipts2 = Map::new();
        receipts2.insert(hash_raw_tx(&raw_tx), make_receipt("0x0"));
        let fb2 = flashblock("0xswap", 0, 67_650_064, vec![raw_tx], receipts2);
        let (_, _, _, _, _, swap_events2, _) = extract_logs_from_xlayer_flashblock(
            &fb2,
            &matcher,
            &HashSet::new(),
            &HashSet::from([caliber_contract]),
            &HashSet::new(),
            &mut dedup,
            &mut parse_cache,
            &mut tx_tracker,
            &mut latest_block_timestamp,
        );
        assert!(swap_events2.is_empty(), "回滚交易不应用");

        // 合约不在兴趣集合 → 不产出
        let (_, _, _, _, _, swap_events3, _) = extract_logs_from_xlayer_flashblock(
            &fb,
            &matcher,
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::new(),
            &mut dedup,
            &mut parse_cache,
            &mut tx_tracker,
            &mut latest_block_timestamp,
        );
        assert!(swap_events3.is_empty());
    }
}

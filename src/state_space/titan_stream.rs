//! Titan PropAMM 报价流基础设施(M1)。
//!
//! 所有 Ethereum PropAMM venue(Fermi/bopAMM/Kipseli/...)共用的数据源:
//!
//! - **overrides 流**(WS `pamm_quote_stream` / RPC `titan_getPammStateOverrides`):
//!   携带各 venue 的最新 lane 槽位(`stateDiff`)+ 余额/随机数 override——这是链上看不到的
//!   高频报价状态,裸 `eth_call`/链上存储只能看到过时值;
//! - 每条消息为**完整快照、最新者胜**;消息带 beacon `slot`,模拟时必须把 block time 固定为
//!   `BEACON_GENESIS_TS + slot*12` 才能通过 venue 的报价新鲜度检查(`StaleUpdate`);
//! - 流是 maker opt-in + 节流,Titan 对 >400ms 未更新的报价从流中淘汰;断线只能回到
//!   最新快照(无历史),用 RPC 拉一次最新快照 rebase 后重连;
//! - **price-levels 流**(WS `pamm_price_levels` / RPC `titan_getPammPriceLevels`):
//!   Titan 预报价梯子(`Simulated` = EVM 实测,`Interpolated` = 线性插值),每条消息为
//!   完整快照、最新者胜;**不参与状态同步**,仅供交叉验证/机会发现。
//!
//! 消费方(如 `fermi_prop`)以 `Arc<TitanOverridesSnapshot>` 拿到最新状态,按 slot 版本
//! 更新本地 lane/余额缓存并触发下游变化检测。

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, B256, U256};
use async_stream::stream;
use futures::{SinkExt, Stream, StreamExt};
use serde_json::Value;
use thiserror::Error;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{debug, error, info, warn};

// ============================================================================
// 常量
// ============================================================================

/// 主网 beacon 创世时间(秒):报价新鲜度校验的 block time 基准。
pub const BEACON_GENESIS_TS: u64 = 1_606_824_023;
/// 主网 slot 时长(秒)。
pub const SECS_PER_SLOT: u64 = 12;
/// 默认 overrides WS 端点(eu-central-1;`ap.`/`us.` 前缀可替换就近区域)。
pub const DEFAULT_OVERRIDES_WS_URL: &str = "wss://eu.rpc.titanbuilder.xyz/ws/pamm_quote_stream";
/// 默认 overrides JSON-RPC 端点(数据 API)。
pub const DEFAULT_OVERRIDES_RPC_URL: &str = "https://eu.rpc.titanbuilder.xyz/data";
/// WS 空闲超时(消息间隔超过则判定断线)。
pub const TITAN_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// 断线/连接失败后的重连延迟。
pub const TITAN_STREAM_RECONNECT_DELAY: Duration = Duration::from_secs(2);

// ============================================================================
// 错误
// ============================================================================

#[derive(Debug, Error)]
pub enum TitanStreamError {
    #[error("titan overrides parse error: {0}")]
    Parse(String),
    #[error("titan overrides transport error: {0}")]
    Transport(String),
    #[error("titan overrides stream ended")]
    StreamEnded,
}

pub type TitanStreamResult<T> = Result<T, TitanStreamError>;

// ============================================================================
// 快照结构
// ============================================================================

/// 单个账户的 override(balance/nonce/stateDiff)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TitanAccountOverride {
    pub balance: Option<U256>,
    pub nonce: Option<u64>,
    /// storage slot → value。
    pub state_diff: HashMap<B256, U256>,
}

/// 单个 venue 的 overrides:合约/账户地址 → override。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TitanPammOverrides {
    pub accounts: HashMap<Address, TitanAccountOverride>,
}

/// 一次完整的 overrides 快照(WS 消息或 RPC result 的解析结果)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TitanOverridesSnapshot {
    /// beacon slot(报价有效性的时间基准)。
    pub slot: Option<u64>,
    /// 快照生成时的区块号。
    pub block_number: Option<u64>,
    /// 生成时间(ns)。
    pub timestamp_ns: Option<u64>,
    /// venue 地址 → overrides。
    pub per_pamm: HashMap<Address, TitanPammOverrides>,
}

impl TitanOverridesSnapshot {
    /// 解析 Titan overrides 负载(WS 帧或 JSON-RPC `result`)。
    ///
    /// 顶层:`slot`/`blockNumber`(或 `block_number`)/`timestamp` 为元数据;
    /// 其余 `0x` 开头的 40 位 hex 键为 venue 地址,值为
    /// `{"stateOverride" | "state_override": { <account>: {balance?, nonce?, stateDiff?} }}`。
    pub fn parse(raw: &Value) -> TitanStreamResult<Self> {
        let object = raw.as_object().ok_or_else(|| {
            TitanStreamError::Parse("overrides payload is not a JSON object".into())
        })?;

        let mut snapshot = TitanOverridesSnapshot {
            slot: object.get("slot").and_then(parse_u64),
            block_number: object
                .get("blockNumber")
                .or_else(|| object.get("block_number"))
                .and_then(parse_u64),
            timestamp_ns: object.get("timestamp").and_then(parse_u64),
            per_pamm: HashMap::new(),
        };

        for (key, payload) in object {
            if is_address_key(key) {
                let Ok(venue) = Address::from_str(key) else {
                    continue;
                };
                if let Some(pamm) = parse_pamm_overrides(payload) {
                    snapshot.per_pamm.insert(venue, pamm);
                }
            }
        }

        Ok(snapshot)
    }

    /// 快照对应的 canonical block time(秒):`BEACON_GENESIS_TS + slot*12`。
    /// 无 slot 时退回消息生成时间(秒)。
    pub fn block_time_secs(&self) -> Option<u64> {
        match self.slot {
            Some(slot) => Some(BEACON_GENESIS_TS + slot.saturating_mul(SECS_PER_SLOT)),
            None => self.timestamp_ns.map(|ns| ns / 1_000_000_000),
        }
    }
}

/// 判断是否为 venue 地址键(0x + 40 hex)。
fn is_address_key(key: &str) -> bool {
    key.starts_with("0x") && key.len() == 42 && key[2..].chars().all(|c| c.is_ascii_hexdigit())
}

/// 宽容解析 u64:数字 / "0x" hex 字符串 / 纯数字字符串。
fn parse_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => {
            let trimmed = s.trim_start_matches("0x");
            u64::from_str_radix(trimmed, 16).ok()
        }
        _ => None,
    }
}

fn parse_u256(value: &Value) -> Option<U256> {
    let s = value.as_str()?.trim_start_matches("0x");
    U256::from_str_radix(s, 16).ok()
}

fn parse_pamm_overrides(payload: &Value) -> Option<TitanPammOverrides> {
    let inner = payload
        .get("stateOverride")
        .or_else(|| payload.get("state_override"))?
        .as_object()?;

    let mut accounts = HashMap::new();
    for (key, spec) in inner {
        let Ok(account) = Address::from_str(key) else {
            continue;
        };
        let Some(obj) = spec.as_object() else {
            continue;
        };
        let mut override_ = TitanAccountOverride::default();
        if let Some(balance) = obj.get("balance").and_then(parse_u256) {
            override_.balance = Some(balance);
        }
        if let Some(nonce) = obj.get("nonce").and_then(parse_u64) {
            override_.nonce = Some(nonce);
        }
        if let Some(diffs) = obj.get("stateDiff").or_else(|| obj.get("state_diff")) {
            if let Some(diffs) = diffs.as_object() {
                for (slot, value) in diffs {
                    let (Ok(slot), Some(value)) = (B256::from_str(slot), parse_u256(value)) else {
                        continue;
                    };
                    override_.state_diff.insert(slot, value);
                }
            }
        }
        accounts.insert(account, override_);
    }

    (!accounts.is_empty()).then_some(TitanPammOverrides { accounts })
}

// ============================================================================
// 一次性 RPC 拉取(断线 rebase / 冷启动)
// ============================================================================

/// 通过 JSON-RPC `titan_getPammStateOverrides` 拉取一次最新快照。
pub async fn fetch_overrides_snapshot(
    rpc_url: &str,
) -> TitanStreamResult<Arc<TitanOverridesSnapshot>> {
    let client = reqwest::Client::new();
    let response = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "titan_getPammStateOverrides",
            "params": [],
        }))
        .send()
        .await
        .map_err(|e| TitanStreamError::Transport(format!("rpc request failed: {e}")))?;

    let body: Value = response
        .json()
        .await
        .map_err(|e| TitanStreamError::Transport(format!("rpc response decode failed: {e}")))?;

    let result = body
        .get("result")
        .ok_or_else(|| TitanStreamError::Transport(format!("rpc error: {body}")))?;

    TitanOverridesSnapshot::parse(result).map(Arc::new)
}

// ============================================================================
// WS 订阅(实时流,slot 单调守卫 + 断线重连)
// ============================================================================

/// Titan pAMM 流消费配置（M4，state_space 挂载入口）。
///
/// `None` = 关闭（默认）；Ethereum 主网 + Fermi 时建议启用。
#[derive(Debug, Clone)]
pub struct TitanPammStreamConfig {
    pub ws_url: String,
    pub rpc_url: String,
    pub idle_timeout: Duration,
    pub reconnect_delay: Duration,
    /// 链上校准周期（`eth_getStorageAt` 读 registry lane 槽位交叉验证）。
    pub reconcile_interval: Duration,
}

impl Default for TitanPammStreamConfig {
    fn default() -> Self {
        Self {
            ws_url: DEFAULT_OVERRIDES_WS_URL.to_string(),
            rpc_url: DEFAULT_OVERRIDES_RPC_URL.to_string(),
            idle_timeout: TITAN_STREAM_IDLE_TIMEOUT,
            reconnect_delay: TITAN_STREAM_RECONNECT_DELAY,
            reconcile_interval: Duration::from_secs(30),
        }
    }
}

impl TitanPammStreamConfig {
    pub fn new(ws_url: impl Into<String>, rpc_url: impl Into<String>) -> Self {
        Self {
            ws_url: ws_url.into(),
            rpc_url: rpc_url.into(),
            ..Default::default()
        }
    }
}

/// 订阅配置。
#[derive(Debug, Clone)]
pub struct TitanQuoteStreamConfig {
    pub ws_url: String,
    pub rpc_url: String,
    pub reconnect_delay: Duration,
    pub idle_timeout: Duration,
}

impl Default for TitanQuoteStreamConfig {
    fn default() -> Self {
        Self {
            ws_url: DEFAULT_OVERRIDES_WS_URL.to_string(),
            rpc_url: DEFAULT_OVERRIDES_RPC_URL.to_string(),
            reconnect_delay: TITAN_STREAM_RECONNECT_DELAY,
            idle_timeout: TITAN_STREAM_IDLE_TIMEOUT,
        }
    }
}

impl TitanQuoteStreamConfig {
    pub fn new(ws_url: impl Into<String>, rpc_url: impl Into<String>) -> Self {
        Self {
            ws_url: ws_url.into(),
            rpc_url: rpc_url.into(),
            ..Default::default()
        }
    }
}

/// 订阅 overrides 实时流。
///
/// 语义:
/// - 每条消息为完整快照,**最新者胜**:slot 严格小于当前值的消息被丢弃,大于等于则覆盖;
/// - 空闲超时/连接断开 → 用 RPC 拉一次最新快照 rebase(同样过 slot 守卫)后重连;
/// - 快照以 `Arc` 共享,消费方克隆指针即可,避免热路径深拷贝。
pub fn subscribe_overrides_stream(
    config: TitanQuoteStreamConfig,
) -> impl Stream<Item = TitanStreamResult<Arc<TitanOverridesSnapshot>>> + Send {
    stream! {
        let mut last_slot: Option<u64> = None;
        let mut last_block: Option<u64> = None;

        loop {
            // 连接前先尝试一次 RPC 快照(cold-start / rebase)。
            match fetch_overrides_snapshot(&config.rpc_url).await {
                Ok(snapshot) => {
                    if accept_snapshot(&snapshot, &mut last_slot, &mut last_block) {
                        debug!(
                            slot = ?snapshot.slot,
                            block = ?snapshot.block_number,
                            pamms = snapshot.per_pamm.len(),
                            "titan overrides rpc snapshot"
                        );
                        yield Ok(snapshot);
                    }
                }
                Err(e) => warn!("titan overrides rpc snapshot failed: {}", e),
            }

            let mut socket = match connect_async(&config.ws_url).await {
                Ok((socket, _)) => {
                    info!(ws_url = %config.ws_url, "Connected to Titan pAMM quote stream");
                    socket
                }
                Err(e) => {
                    error!(
                        ws_url = %config.ws_url,
                        "Titan pAMM quote stream connect failed: {}",
                        e
                    );
                    tokio::time::sleep(config.reconnect_delay).await;
                    continue;
                }
            };

            loop {
                let next = tokio::time::timeout(config.idle_timeout, socket.next()).await;
                let Some(message_result) = (match next {
                    Ok(v) => v,
                    Err(_) => {
                        warn!("Titan pAMM quote stream idle timeout");
                        break;
                    }
                }) else {
                    warn!("Titan pAMM quote stream ended");
                    break;
                };

                let message = match message_result {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Titan pAMM quote stream receive error: {}", e);
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
                    Message::Close(frame) => {
                        warn!(?frame, "Titan pAMM quote stream closed");
                        break;
                    }
                    Message::Frame(_) => continue,
                };

                let value: Value = match serde_json::from_str(&payload) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Titan pAMM quote stream decode failed: {}", e);
                        continue;
                    }
                };

                match TitanOverridesSnapshot::parse(&value) {
                    Ok(snapshot) => {
                        if accept_snapshot(&snapshot, &mut last_slot, &mut last_block) {
                            yield Ok(Arc::new(snapshot));
                        }
                    }
                    Err(e) => {
                        warn!("Titan pAMM quote stream parse failed: {}", e);
                    }
                }
            }

            // 流结束/断线:短暂延迟后重连(重连循环顶部会先做一次 RPC rebase)。
            tokio::time::sleep(config.reconnect_delay).await;
        }
    }
}

// ============================================================================
// price-levels 流(辅助:交叉验证 / 机会发现,不参与状态同步)
// ============================================================================

/// 报价梯子 rung 类型:`Simulated` = EVM 合成吃单实测,`Interpolated` = 线性插值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitanLevelVariant {
    Simulated,
    Interpolated,
}

/// 单个报价 rung(吃单金额 → 出单金额)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitanLevel {
    pub amount_in: U256,
    pub amount_out: U256,
    pub variant: TitanLevelVariant,
}

/// 单个 pair 的报价梯子。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TitanPairLadder {
    pub token_in: Address,
    pub token_out: Address,
    pub order_book: Vec<TitanLevel>,
}

/// 单个 pAMM 的全部 pair 梯子。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TitanPammLadder {
    pub pamm: Address,
    pub pairs: Vec<TitanPairLadder>,
}

/// price-levels 完整快照(WS 消息或 RPC `result`)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TitanPriceLevelsSnapshot {
    pub slot: Option<u64>,
    pub block_number: Option<u64>,
    pub timestamp_ns: Option<u64>,
    pub pamms: Vec<TitanPammLadder>,
}

impl TitanPriceLevelsSnapshot {
    /// 解析 price-levels 负载。
    ///
    /// 顶层:`slot`/`blockNumber`(或 `block_number`)/`timestamp` + `pamms` 数组;
    /// 每项 `{pamm, pairs: [{tokenIn, tokenOut, orderBook: [{amountIn, amountOut, variant}]}]}`。
    pub fn parse(raw: &Value) -> TitanStreamResult<Self> {
        let object = raw.as_object().ok_or_else(|| {
            TitanStreamError::Parse("price levels payload is not a JSON object".into())
        })?;

        let mut snapshot = TitanPriceLevelsSnapshot {
            slot: object.get("slot").and_then(parse_u64),
            block_number: object
                .get("blockNumber")
                .or_else(|| object.get("block_number"))
                .and_then(parse_u64),
            timestamp_ns: object.get("timestamp").and_then(parse_u64),
            pamms: Vec::new(),
        };

        let Some(pamms) = object.get("pamms").and_then(Value::as_array) else {
            return Ok(snapshot);
        };

        for pamm_value in pamms {
            let Some(pamm_obj) = pamm_value.as_object() else {
                continue;
            };
            let Some(pamm_addr) = pamm_obj
                .get("pamm")
                .and_then(Value::as_str)
                .and_then(|s| Address::from_str(s).ok())
            else {
                continue;
            };

            let mut ladder = TitanPammLadder {
                pamm: pamm_addr,
                pairs: Vec::new(),
            };

            if let Some(pairs) = pamm_obj.get("pairs").and_then(Value::as_array) {
                for pair_value in pairs {
                    let Some(pair_obj) = pair_value.as_object() else {
                        continue;
                    };
                    let (Some(token_in), Some(token_out)) = (
                        pair_obj
                            .get("tokenIn")
                            .and_then(Value::as_str)
                            .and_then(|s| Address::from_str(s).ok()),
                        pair_obj
                            .get("tokenOut")
                            .and_then(Value::as_str)
                            .and_then(|s| Address::from_str(s).ok()),
                    ) else {
                        continue;
                    };
                    let mut pair_ladder = TitanPairLadder {
                        token_in,
                        token_out,
                        order_book: Vec::new(),
                    };
                    if let Some(order_book) = pair_obj.get("orderBook").and_then(Value::as_array) {
                        for level in order_book {
                            let Some(level_obj) = level.as_object() else {
                                continue;
                            };
                            let (Some(amount_in), Some(amount_out)) = (
                                level_obj.get("amountIn").and_then(parse_u256),
                                level_obj.get("amountOut").and_then(parse_u256),
                            ) else {
                                continue;
                            };
                            let variant = match level_obj
                                .get("variant")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                            {
                                "Simulated" => TitanLevelVariant::Simulated,
                                "Interpolated" => TitanLevelVariant::Interpolated,
                                _ => continue,
                            };
                            pair_ladder.order_book.push(TitanLevel {
                                amount_in,
                                amount_out,
                                variant,
                            });
                        }
                    }
                    if !pair_ladder.order_book.is_empty() {
                        ladder.pairs.push(pair_ladder);
                    }
                }
            }

            if !ladder.pairs.is_empty() {
                snapshot.pamms.push(ladder);
            }
        }

        Ok(snapshot)
    }

    /// 快照对应的 canonical block time(秒),与 overrides 快照同语义。
    pub fn block_time_secs(&self) -> Option<u64> {
        match self.slot {
            Some(slot) => Some(BEACON_GENESIS_TS + slot.saturating_mul(SECS_PER_SLOT)),
            None => self.timestamp_ns.map(|ns| ns / 1_000_000_000),
        }
    }
}

/// 版本守卫核心:slot 为准(单调递增),无 slot 时退化为 block_number。
fn accept_version(
    slot: Option<u64>,
    block_number: Option<u64>,
    last_slot: &mut Option<u64>,
    last_block: &mut Option<u64>,
) -> bool {
    if let Some(slot) = slot {
        if let Some(last) = *last_slot {
            if slot < last {
                return false;
            }
        }
        *last_slot = Some(slot);
        return true;
    }
    if let Some(block) = block_number {
        if let Some(last) = *last_block {
            if block < last {
                return false;
            }
        }
        *last_block = Some(block);
    }
    true
}

/// 通过 JSON-RPC `titan_getPammPriceLevels` 拉取一次最新 price-levels 快照。
pub async fn fetch_price_levels_snapshot(
    rpc_url: &str,
) -> TitanStreamResult<Arc<TitanPriceLevelsSnapshot>> {
    let client = reqwest::Client::new();
    let response = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "titan_getPammPriceLevels",
            "params": [],
        }))
        .send()
        .await
        .map_err(|e| TitanStreamError::Transport(format!("rpc request failed: {e}")))?;

    let body: Value = response
        .json()
        .await
        .map_err(|e| TitanStreamError::Transport(format!("rpc response decode failed: {e}")))?;

    let result = body
        .get("result")
        .ok_or_else(|| TitanStreamError::Transport(format!("rpc error: {body}")))?;

    TitanPriceLevelsSnapshot::parse(result).map(Arc::new)
}

/// 订阅 price-levels 实时流。
///
/// 语义与 overrides 流一致:完整快照、最新者胜(slot 单调守卫)、断线/空闲超时
/// RPC rebase 后重连。**仅供交叉验证/机会发现,不参与状态同步。**
pub fn subscribe_price_levels_stream(
    config: TitanQuoteStreamConfig,
) -> impl Stream<Item = TitanStreamResult<Arc<TitanPriceLevelsSnapshot>>> + Send {
    stream! {
        let mut last_slot: Option<u64> = None;
        let mut last_block: Option<u64> = None;

        loop {
            match fetch_price_levels_snapshot(&config.rpc_url).await {
                Ok(snapshot) => {
                    if accept_price_levels(&snapshot, &mut last_slot, &mut last_block) {
                        debug!(
                            slot = ?snapshot.slot,
                            block = ?snapshot.block_number,
                            pamms = snapshot.pamms.len(),
                            "titan price levels rpc snapshot"
                        );
                        yield Ok(snapshot);
                    }
                }
                Err(e) => warn!("titan price levels rpc snapshot failed: {}", e),
            }

            let mut socket = match connect_async(&config.ws_url).await {
                Ok((socket, _)) => {
                    info!(ws_url = %config.ws_url, "Connected to Titan pAMM price levels stream");
                    socket
                }
                Err(e) => {
                    error!(
                        ws_url = %config.ws_url,
                        "Titan pAMM price levels stream connect failed: {}",
                        e
                    );
                    tokio::time::sleep(config.reconnect_delay).await;
                    continue;
                }
            };

            loop {
                let next = tokio::time::timeout(config.idle_timeout, socket.next()).await;
                let Some(message_result) = (match next {
                    Ok(v) => v,
                    Err(_) => {
                        warn!("Titan pAMM price levels stream idle timeout");
                        break;
                    }
                }) else {
                    warn!("Titan pAMM price levels stream ended");
                    break;
                };

                let message = match message_result {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Titan pAMM price levels stream receive error: {}", e);
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
                    Message::Close(frame) => {
                        warn!(?frame, "Titan pAMM price levels stream closed");
                        break;
                    }
                    Message::Frame(_) => continue,
                };

                let value: Value = match serde_json::from_str(&payload) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Titan pAMM price levels decode failed: {}", e);
                        continue;
                    }
                };

                match TitanPriceLevelsSnapshot::parse(&value) {
                    Ok(snapshot) => {
                        if accept_price_levels(&snapshot, &mut last_slot, &mut last_block) {
                            yield Ok(Arc::new(snapshot));
                        }
                    }
                    Err(e) => {
                        warn!("Titan pAMM price levels parse failed: {}", e);
                    }
                }
            }

            tokio::time::sleep(config.reconnect_delay).await;
        }
    }
}

/// slot 单调守卫(overrides 快照):丢弃严格更旧的快照。
fn accept_snapshot(
    snapshot: &TitanOverridesSnapshot,
    last_slot: &mut Option<u64>,
    last_block: &mut Option<u64>,
) -> bool {
    accept_version(snapshot.slot, snapshot.block_number, last_slot, last_block)
}

/// slot 单调守卫(price-levels 快照):丢弃严格更旧的快照。
fn accept_price_levels(
    snapshot: &TitanPriceLevelsSnapshot,
    last_slot: &mut Option<u64>,
    last_block: &mut Option<u64>,
) -> bool {
    accept_version(snapshot.slot, snapshot.block_number, last_slot, last_block)
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 实测快照(2026-08-23,eu 区域 RPC 返回,Fermi 条目)裁剪后作为解析 fixture。
    fn sample_payload() -> Value {
        json!({
            "slot": 15053114,
            "blockNumber": "0x189ea55",
            "timestamp": 1781801564588230787u64,
            "0xb1076fe3ab5e28005c7c323bac5ac06a680d452e": {
                "stateOverride": {
                    "0xda7afeed01fe625cf15d187a19f94b45f00b8c5f": {
                        "balance": "0x0",
                        "nonce": "0x1",
                        "stateDiff": {
                            "0x1ba5a5b4f3238a22bdbfc2cb8c8da3b2407a3cb61bad29d706607de95bf0b58a":
                                "0x6a8a7d770100000000000000000000000000000000000000000006f8628fe3a8",
                            "0x461a5afe3bfdbe196241a0acc4cdb6da2977a9f81e8c76a449ea5c07d1705329":
                                "0x6a8a7d7701000000000000000000000000000000000000000000000005f60e14"
                        }
                    },
                    "0x5c4c7ac6295490cd476995f1a454e783099b4792": {
                        "balance": "0x9f7f354fd7d9c3",
                        "nonce": "0x2b16"
                    }
                }
            }
        })
    }

    #[test]
    fn parse_snapshot_metadata_and_pamms() {
        let snapshot = TitanOverridesSnapshot::parse(&sample_payload()).unwrap();
        assert_eq!(snapshot.slot, Some(15053114));
        assert_eq!(snapshot.block_number, Some(0x189ea55));
        assert_eq!(snapshot.timestamp_ns, Some(1781801564588230787));
        assert_eq!(
            snapshot.block_time_secs(),
            Some(BEACON_GENESIS_TS + 15053114 * SECS_PER_SLOT)
        );

        let venue = Address::from_str("0xb1076fe3ab5e28005c7c323bac5ac06a680d452e").unwrap();
        let pamm = snapshot.per_pamm.get(&venue).expect("fermi venue present");
        assert_eq!(pamm.accounts.len(), 2);

        let registry = Address::from_str("0xda7afeed01fe625cf15d187a19f94b45f00b8c5f").unwrap();
        let reg = pamm.accounts.get(&registry).unwrap();
        assert_eq!(reg.balance, Some(U256::ZERO));
        assert_eq!(reg.nonce, Some(1));
        assert_eq!(reg.state_diff.len(), 2);

        // lane 槽位值:时间戳 + 标志 + 价格(E8)。
        let slot =
            B256::from_str("0x1ba5a5b4f3238a22bdbfc2cb8c8da3b2407a3cb61bad29d706607de95bf0b58a")
                .unwrap();
        let value = reg.state_diff.get(&slot).unwrap();
        // 高 32 位为 updateTimestamp。
        let ts = (*value >> U256::from(224)).to::<u64>();
        assert_eq!(ts, 0x6a8a7d77);
        // 低 20 字节为 fairPriceE8。
        let price = (value
            & U256::from_str_radix("ffffffffffffffffffffffffffffffffffffffff", 16).unwrap())
        .to::<u64>();
        assert_eq!(price, 0x6f8628fe3a8);
    }

    #[test]
    fn parse_ws_snake_case_block_number() {
        // WS 帧可能是 snake_case(文档示例)。
        let payload = json!({
            "slot": 14285824,
            "block_number": 25051224,
            "0xb0999914b3de1be58ef2416af09bd2e7f8aad03c": {
                "state_override": {
                    "0xda7afeed01fe625cf15d187a19f94b45f00b8c5f": {
                        "state_diff": {
                            "0x2dfd4c728fc9f02fd35c6b57f8a8134612fa2b60d3e3ac5b0869fcba3ffb8512":
                                "0x6a8a6c8b020101d6808000000000000000000000000000000000000000000000"
                        }
                    }
                }
            }
        });
        let snapshot = TitanOverridesSnapshot::parse(&payload).unwrap();
        assert_eq!(snapshot.slot, Some(14285824));
        assert_eq!(snapshot.block_number, Some(25051224));
        let venue = Address::from_str("0xb0999914b3de1be58ef2416af09bd2e7f8aad03c").unwrap();
        let pamm = snapshot.per_pamm.get(&venue).unwrap();
        assert_eq!(pamm.accounts.len(), 1);
        assert!(
            pamm.accounts
                .get(&Address::from_str("0xda7afeed01fe625cf15d187a19f94b45f00b8c5f").unwrap())
                .unwrap()
                .state_diff
                .len()
                == 1
        );
    }

    #[test]
    fn parse_rejects_non_object() {
        assert!(TitanOverridesSnapshot::parse(&json!([])).is_err());
        assert!(TitanOverridesSnapshot::parse(&json!("x")).is_err());
    }

    #[test]
    fn parse_price_levels_real_payload() {
        // 2026-08-24 实测(裁剪):Fermi 以 wrapper 地址为 pamm key。
        let payload = json!({
            "slot": 15058570,
            "blockNumber": 25821077,
            "timestamp": 1787526856624188209u64,
            "pamms": [
                {
                    "pamm": "0x5979458912f80b96d30d4220af8e2e4925a33320",
                    "pairs": [
                        {
                            "tokenIn": "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599",
                            "tokenOut": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                            "orderBook": [
                                {"amountIn": "0x989680", "amountOut": "0x174b67393", "variant": "Simulated"},
                                {"amountIn": "0xaa810a", "amountOut": "0x1a0781260", "variant": "Interpolated"}
                            ]
                        }
                    ]
                }
            ]
        });
        let snapshot = TitanPriceLevelsSnapshot::parse(&payload).unwrap();
        assert_eq!(snapshot.slot, Some(15058570));
        assert_eq!(snapshot.block_number, Some(25821077));
        assert_eq!(snapshot.timestamp_ns, Some(1787526856624188209));
        assert_eq!(
            snapshot.block_time_secs(),
            Some(BEACON_GENESIS_TS + 15058570 * SECS_PER_SLOT)
        );
        assert_eq!(snapshot.pamms.len(), 1);
        let ladder = &snapshot.pamms[0];
        assert_eq!(
            ladder.pamm,
            Address::from_str("0x5979458912f80b96d30d4220af8e2e4925a33320").unwrap()
        );
        assert_eq!(ladder.pairs.len(), 1);
        let pair = &ladder.pairs[0];
        assert_eq!(
            pair.token_in,
            Address::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap()
        );
        assert_eq!(
            pair.token_out,
            Address::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap()
        );
        assert_eq!(pair.order_book.len(), 2);
        assert_eq!(pair.order_book[0].variant, TitanLevelVariant::Simulated);
        assert_eq!(pair.order_book[1].variant, TitanLevelVariant::Interpolated);
        assert_eq!(
            pair.order_book[0].amount_in,
            U256::from_str_radix("989680", 16).unwrap()
        );
        assert_eq!(
            pair.order_book[1].amount_out,
            U256::from_str_radix("1a0781260", 16).unwrap()
        );
    }

    #[test]
    fn parse_price_levels_skips_unknown_variant() {
        let payload = json!({
            "slot": 1,
            "pamms": [{
                "pamm": "0x5979458912f80b96d30d4220af8e2e4925a33320",
                "pairs": [{
                    "tokenIn": "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599",
                    "tokenOut": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                    "orderBook": [
                        {"amountIn": "0x1", "amountOut": "0x2", "variant": "Simulated"},
                        {"amountIn": "0x3", "amountOut": "0x4", "variant": "Bogus"}
                    ]
                }]
            }]
        });
        let snapshot = TitanPriceLevelsSnapshot::parse(&payload).unwrap();
        assert_eq!(snapshot.pamms[0].pairs[0].order_book.len(), 1);
        assert_eq!(
            snapshot.pamms[0].pairs[0].order_book[0].variant,
            TitanLevelVariant::Simulated
        );
    }

    #[test]
    fn price_levels_slot_guard_rejects_older() {
        let mut last_slot = None;
        let mut last_block = None;
        let newer = TitanPriceLevelsSnapshot {
            slot: Some(15058570),
            block_number: Some(25821077),
            ..Default::default()
        };
        let older = TitanPriceLevelsSnapshot {
            slot: Some(15058569),
            block_number: Some(25821076),
            ..Default::default()
        };
        assert!(accept_price_levels(&newer, &mut last_slot, &mut last_block));
        assert!(!accept_price_levels(
            &older,
            &mut last_slot,
            &mut last_block
        ));
    }

    #[test]
    fn slot_guard_rejects_older_snapshots() {
        let mut last_slot = None;
        let mut last_block = None;

        let newer = TitanOverridesSnapshot {
            slot: Some(15053114),
            block_number: Some(0x189ea55),
            ..Default::default()
        };
        let older = TitanOverridesSnapshot {
            slot: Some(15053113),
            block_number: Some(0x189ea54),
            ..Default::default()
        };

        assert!(accept_snapshot(&newer, &mut last_slot, &mut last_block));
        assert!(!accept_snapshot(&older, &mut last_slot, &mut last_block));
        assert_eq!(last_slot, Some(15053114));
    }
}

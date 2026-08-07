//! Caliber propAMM — XLayer flashblocks 实时流订阅 probe。
//!
//! 订阅 XLayer flashblocks WS，逐块扫描 `diff.transactions` 原始交易，用模块真实
//! 代码路径提取 caliber `batchUpdateParameters` 更新：
//!
//! 1. 轻量 RLP 定位 `to`（`extract_to_from_raw_tx`，零分配）→ 过滤出 caliber 合约交易；
//! 2. 命中后完整解码 calldata（`extract_input_from_raw_tx`）→ 标准 ABI 解码
//!    （`decode_batch_update_parameters`）；
//! 3. 链上交叉验证（异步、不阻塞流处理）：
//!    a. `eth_getTransactionByHash` 对比 `to`/`input`（流内 raw bytes 与规范链一致）；
//!    b. 交易所在块上读 `data+0` 存储槽，对比 field0/flags/deadline。
//!
//! 用法:
//! ```bash
//! cargo run --example caliber_prop_flashblocks_probe
//! ```
//!
//! 环境变量:
//! - `XLAYER_FLASHBLOCKS_WS`  WS 端点（默认 `wss://ws.xlayer.tech/flashblocks`）
//! - `XLAYER_RPC`             验证用 HTTP RPC（默认 `https://rpc.xlayer.tech`）
//! - `CALIBER_CONTRACT`       caliber 合约地址（默认 XLayer 主部署）
//! - `RUN_SECS`               运行时长（默认 60）
//! - `VERIFY`                 是否做链上交叉验证（默认 1）

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{keccak256, Address, B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use amms::amms::caliber_prop::{
    decode_batch_update_parameters, extract_input_from_raw_tx, extract_to_from_raw_tx,
    CaliberBatchUpdate,
};
use eyre::Context;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::Message;

const DEFAULT_WS: &str = "wss://ws.xlayer.tech/flashblocks";
const DEFAULT_RPC: &str = "https://rpc.xlayer.tech";
const DEFAULT_CONTRACT: &str = "0x154586b2479b9a11e3d4db90024dc0e26f097312";

// ─────────────────────────────────────────────
// Flashblock 消息结构（与提取层同构的极简版本）
// ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ProbeMessage {
    payload_id: String,
    #[serde(default)]
    base: Option<ProbeBase>,
    #[serde(default)]
    diff: Option<ProbeDiff>,
    #[serde(default)]
    metadata: Option<ProbeMetadata>,
}

#[derive(Debug, Deserialize)]
struct ProbeBase {
    #[serde(default)]
    block_number: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProbeDiff {
    #[serde(default)]
    transactions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ProbeMetadata {
    #[serde(default)]
    block_number: Option<u64>,
}

/// 块内全局 tx_index 拼接（与 `XlayerTxCountTracker` 同款）
struct PayloadTxCounter {
    counts: HashMap<String, u64>,
    order: VecDeque<String>,
    max_payloads: usize,
}

impl PayloadTxCounter {
    fn new(max_payloads: usize) -> Self {
        Self {
            counts: HashMap::new(),
            order: VecDeque::new(),
            max_payloads,
        }
    }

    fn base(&self, payload_id: &str) -> u64 {
        self.counts.get(payload_id).copied().unwrap_or(0)
    }

    fn advance(&mut self, payload_id: &str, count: u64) {
        let entry = self.counts.entry(payload_id.to_string()).or_insert(0);
        *entry += count;
        if *entry == count {
            self.order.push_back(payload_id.to_string());
        }
        while self.order.len() > self.max_payloads {
            if let Some(evicted) = self.order.pop_front() {
                self.counts.remove(&evicted);
            }
        }
    }
}

/// `data` 基址 = keccak256(pairId || uint256(7))，data+0 即该槽本身
fn pair_data_slot(pair_id: B256) -> B256 {
    let mut input = [0u8; 64];
    input[..32].copy_from_slice(pair_id.as_ref());
    input[32..].copy_from_slice(&U256::from(7u64).to_be_bytes::<32>());
    B256::from(keccak256(input))
}

// ─────────────────────────────────────────────
// 链上交叉验证
// ─────────────────────────────────────────────

struct VerifyStats {
    tx_ok: usize,
    tx_fail: usize,
    storage_ok: usize,
    storage_fail: usize,
    storage_unavailable: usize,
}

/// 等待规范链可用（flashblock 先于区块封装到达），最多 `attempts` 次
async fn wait_available<F, Fut, T>(mut f: F, attempts: usize, delay: Duration) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    for _ in 0..attempts {
        if let Some(v) = f().await {
            return Some(v);
        }
        tokio::time::sleep(delay).await;
    }
    None
}

/// 单笔 caliber 更新交易的链上验证：
/// 1. tx 级：`eth_getTransactionByHash` 对比 to/input；
/// 2. 状态级：交易所在块读 `data+0`，对比 field0/flags/deadline。
async fn verify_update(
    provider: Arc<impl Provider>,
    contract: Address,
    tx_hash: B256,
    stream_input: Vec<u8>,
    block_num: Option<u64>,
    updates: Vec<CaliberBatchUpdate>,
) -> VerifyStats {
    let mut stats = VerifyStats {
        tx_ok: 0,
        tx_fail: 0,
        storage_ok: 0,
        storage_fail: 0,
        storage_unavailable: 0,
    };

    use alloy::consensus::Transaction as _;

    // 1. tx 级验证
    let tx = wait_available(
        || {
            let provider = provider.clone();
            async move {
                provider
                    .get_transaction_by_hash(tx_hash)
                    .await
                    .ok()
                    .flatten()
            }
        },
        10,
        Duration::from_millis(500),
    )
    .await;

    match tx {
        Some(tx)
            if tx.inner.to() == Some(contract)
                && tx.inner.input().as_ref() == stream_input.as_slice() =>
        {
            stats.tx_ok += 1;
        }
        Some(tx) => {
            stats.tx_fail += 1;
            eprintln!(
                "[verify][FAIL] tx={tx_hash:#x} to={:?} input_match={}",
                tx.inner.to(),
                tx.inner.input().as_ref() == stream_input.as_slice()
            );
        }
        None => {
            stats.tx_fail += 1;
            eprintln!(
                "[verify][FAIL] tx={tx_hash:#x} not found on canonical chain within retry window"
            );
        }
    }

    // 2. 状态级验证（需要已知区块号；读 post-block-N 状态，即包含本更新）
    if let Some(block_num) = block_num {
        for u in updates.iter().take(5) {
            let slot = pair_data_slot(u.pair_id);
            let storage = wait_available(
                || {
                    let provider = provider.clone();
                    async move {
                        provider
                            .get_storage_at(contract, U256::from_be_bytes(slot.0))
                            .block_id(BlockId::Number(BlockNumberOrTag::Number(block_num)))
                            .await
                            .ok()
                    }
                },
                10,
                Duration::from_millis(500),
            )
            .await;

            let Some(data0) = storage else {
                stats.storage_unavailable += 1;
                eprintln!(
                    "[verify][WARN] storage unavailable block={block_num} pair={}",
                    u.pair_id
                );
                continue;
            };

            let field0 = data0 & U256::from(u64::MAX);
            let field1 = (data0 >> U256::from(64)) & U256::from(u32::MAX);
            let ts_x = (data0 >> U256::from(96)) & U256::from(u32::MAX);

            let field0_ok = field0 == u.price;
            let field1_ok = field1 == U256::from(u.flags);
            let deadline_ok = u.deadline <= u32::MAX as u64 && ts_x == U256::from(u.deadline);

            if field0_ok && field1_ok && deadline_ok {
                stats.storage_ok += 1;
            } else {
                stats.storage_fail += 1;
                eprintln!(
                    "[verify][FAIL] block={block_num} pair={} got(field0={field0}, field1={field1}, tsX={ts_x}) expected(field0={}, field1={}, tsX={})",
                    u.pair_id, u.price, u.flags, u.deadline
                );
            }
        }
    }

    stats
}

// ─────────────────────────────────────────────
// 主流程
// ─────────────────────────────────────────────

#[tokio::main]
async fn main() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("XLAYER_FLASHBLOCKS_WS").unwrap_or_else(|_| DEFAULT_WS.to_string());
    let rpc_url = std::env::var("XLAYER_RPC").unwrap_or_else(|_| DEFAULT_RPC.to_string());
    let contract: Address = std::env::var("CALIBER_CONTRACT")
        .unwrap_or_else(|_| DEFAULT_CONTRACT.to_string())
        .parse()?;
    let run_secs: u64 = std::env::var("RUN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let verify: bool = std::env::var("VERIFY")
        .ok()
        .and_then(|v| v.parse::<u8>().ok())
        .map(|v| v != 0)
        .unwrap_or(true);

    println!("=== Caliber propAMM XLayer flashblocks probe ===");
    println!("ws: {ws_url}");
    println!("rpc: {rpc_url}");
    println!("contract: {contract:#x}");
    println!("run_secs: {run_secs}, verify: {verify}");

    let provider = Arc::new(ProviderBuilder::new().connect_http(rpc_url.parse()?));
    let (mut ws_stream, _) = connect_async(ws_url.clone())
        .await
        .with_context(|| format!("failed to connect to {ws_url}"))?;
    println!("[probe] connected to Xlayer flashblocks WS");

    let started = Instant::now();
    let deadline = started + Duration::from_secs(run_secs);

    let mut tx_counter = PayloadTxCounter::new(8);
    let mut messages = 0usize;
    let mut txs_scanned = 0usize;
    let mut caliber_txs = 0usize;
    let mut caliber_pairs = 0usize;
    let mut decode_fail = 0usize;
    let mut verify_handles: Vec<tokio::task::JoinHandle<VerifyStats>> = Vec::new();

    loop {
        if Instant::now() >= deadline {
            println!("[probe] deadline reached");
            break;
        }

        let next = tokio::time::timeout(Duration::from_secs(5), ws_stream.next()).await;
        let maybe_message_result = match next {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(message_result) = maybe_message_result else {
            println!("[probe] WS stream ended");
            break;
        };

        let message = match message_result {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[probe][WARN] ws receive error: {e}");
                continue;
            }
        };

        let payload = match message {
            Message::Text(text) => text.as_bytes().to_vec(),
            Message::Binary(bin) => bin.to_vec(),
            Message::Ping(v) => {
                let _ = ws_stream.send(Message::Pong(v)).await;
                continue;
            }
            Message::Pong(_) => continue,
            Message::Close(_) => {
                println!("[probe] WS close frame received");
                break;
            }
            Message::Frame(_) => continue,
        };

        let fb: ProbeMessage = match serde_json::from_slice(&payload) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[probe][WARN] JSON parse fail: {e}");
                continue;
            }
        };
        messages += 1;

        let block_num = fb
            .metadata
            .as_ref()
            .and_then(|m| m.block_number)
            .or_else(|| {
                fb.base
                    .as_ref()
                    .and_then(|b| b.block_number.as_deref())
                    .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
            });

        let tx_base = tx_counter.base(&fb.payload_id);

        if let Some(diff) = fb.diff.as_ref() {
            for (real_idx, raw_hex) in diff.transactions.iter().enumerate() {
                txs_scanned += 1;
                let raw_hex = raw_hex.strip_prefix("0x").unwrap_or(raw_hex);
                let Ok(raw) = alloy::hex::decode(raw_hex) else {
                    continue;
                };

                // 1. 轻量 RLP 定位 to，过滤非目标交易
                let Some(to) = extract_to_from_raw_tx(&raw) else {
                    continue;
                };
                if to != contract {
                    continue;
                }

                // 2. 命中后取 calldata 并 ABI 解码
                let Some(input) = extract_input_from_raw_tx(&raw) else {
                    decode_fail += 1;
                    continue;
                };
                let Some(updates) = decode_batch_update_parameters(&input) else {
                    decode_fail += 1;
                    continue;
                };

                let tx_hash = keccak256(&raw);
                let tx_index = tx_base + real_idx as u64;
                caliber_txs += 1;
                caliber_pairs += updates.len();

                let first = &updates[0];
                println!(
                    "[caliber] block={block_num:?} tx_index={tx_index} hash={tx_hash:#x} pairs={} first=(pair={:#x}, field0={}, flags={}, deadline={})",
                    updates.len(),
                    first.pair_id,
                    first.price,
                    first.flags,
                    first.deadline
                );

                // 3. 链上交叉验证（异步）
                if verify {
                    let provider = provider.clone();
                    let updates = updates.clone();
                    let input = input.clone();
                    verify_handles.push(tokio::spawn(async move {
                        verify_update(provider, contract, tx_hash, input, block_num, updates).await
                    }));
                }
            }
        }

        tx_counter.advance(
            &fb.payload_id,
            fb.diff.as_ref().map_or(0, |d| d.transactions.len() as u64),
        );
    }

    println!("\n=== probe summary ===");
    println!(
        "messages={} txs_scanned={} caliber_txs={} caliber_pairs={} decode_fail={}",
        messages, txs_scanned, caliber_txs, caliber_pairs, decode_fail
    );
    if caliber_txs == 0 {
        println!(
            "[probe] 未捕获到 caliber 更新交易（可能流中暂无更新，或 WS 端点/合约地址配置不符）"
        );
    }

    if !verify_handles.is_empty() {
        println!("\n=== verification results ===");
        let mut total = VerifyStats {
            tx_ok: 0,
            tx_fail: 0,
            storage_ok: 0,
            storage_fail: 0,
            storage_unavailable: 0,
        };
        for handle in verify_handles {
            if let Ok(s) = handle.await {
                total.tx_ok += s.tx_ok;
                total.tx_fail += s.tx_fail;
                total.storage_ok += s.storage_ok;
                total.storage_fail += s.storage_fail;
                total.storage_unavailable += s.storage_unavailable;
            }
        }
        println!(
            "tx_verify: ok={} fail={} | storage_verify: ok={} fail={} unavailable={}",
            total.tx_ok,
            total.tx_fail,
            total.storage_ok,
            total.storage_fail,
            total.storage_unavailable
        );
    }

    Ok(())
}

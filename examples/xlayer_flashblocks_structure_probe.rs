use alloy::primitives::{keccak256, Address, B256};
use eyre::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::str::FromStr;
use std::time::{Duration, Instant};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::Message;

#[derive(Debug, Deserialize)]
struct RawFlashblockMessage {
    payload_id: Option<String>,
    index: Option<u64>,
    #[serde(default)]
    base: Option<Value>,
    #[serde(default)]
    diff: Option<Value>,
    #[serde(default)]
    metadata: Option<Value>,
}

#[derive(Debug)]
struct ReceiptProbe {
    tx_hash: String,
    cumulative_gas_used: Option<u64>,
    transaction_index: Option<u64>,
    logs_len: usize,
    matching_logs_len: usize,
    first_log_index: Option<u64>,
    /// receipt 中每个匹配 log 的完整 event data (用于 content_hash 分析)
    matching_log_data: Vec<(usize, String, Vec<String>, String)>,
}

#[derive(Default)]
struct BlockSummary {
    payload_ids: BTreeSet<String>,
    indexes: Vec<u64>,
    messages: usize,
    diff_txs: usize,
    receipts: usize,
    receipt_hashes: HashSet<String>,
    duplicate_receipts: usize,
    logs: usize,
    matching_logs: usize,
}

/// 全局去重跟踪：记录每个 tx_hash 每次出现的 (block_number, payload_id, index, log_index)
#[derive(Default)]
struct GlobalDuplicateTracker {
    /// key: tx_hash → list of (block_number, payload_id, index, log_index)
    seen: HashMap<String, Vec<(u64, String, u64, usize)>>,
    /// 所有 detected 的重复
    duplicates: Vec<(String, Vec<(u64, String, u64, usize)>)>,
}

impl GlobalDuplicateTracker {
    fn record(&mut self, tx_hash: String, block: u64, payload: String, index: u64, log_idx: usize) {
        let entry = self.seen.entry(tx_hash.clone()).or_default();
        entry.push((block, payload, index, log_idx));
        if entry.len() == 2 {
            // 第一次发现重复
            self.duplicates.push((tx_hash.clone(), entry.clone()));
        }
    }

    fn print_summary(&self) {
        if self.duplicates.is_empty() {
            println!("\n[global-dedup] ✅ 全程无重复 tx_hash");
            return;
        }
        println!(
            "\n[global-dedup] ⚠️ 发现 {} 个重复 tx_hash:",
            self.duplicates.len()
        );
        for (tx_hash, occurrences) in &self.duplicates {
            println!("  tx_hash={}", tx_hash);
            for (i, (bn, pid, idx, log_i)) in occurrences.iter().enumerate() {
                println!(
                    "    occurrence[{}]: block={} payload={} index={} log_position={}",
                    i, bn, pid, idx, log_i
                );
            }
        }
    }
}

fn strip_0x(s: &str) -> &str {
    s.strip_prefix("0x").unwrap_or(s)
}

fn parse_u64_value(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => u64::from_str_radix(strip_0x(s), 16)
            .ok()
            .or_else(|| s.parse().ok()),
        _ => None,
    }
}

fn block_number(msg: &RawFlashblockMessage) -> Option<u64> {
    msg.metadata
        .as_ref()
        .and_then(|m| m.get("block_number"))
        .and_then(parse_u64_value)
        .or_else(|| {
            msg.base
                .as_ref()
                .and_then(|b| b.get("number"))
                .and_then(parse_u64_value)
        })
}

fn tx_hash_from_raw(raw: &str) -> Option<String> {
    let bytes = alloy::hex::decode(strip_0x(raw)).ok()?;
    Some(format!("{:?}", B256::from(keccak256(bytes))))
}

fn diff_tx_hashes(msg: &RawFlashblockMessage) -> Vec<String> {
    msg.diff
        .as_ref()
        .and_then(|d| d.get("transactions"))
        .and_then(Value::as_array)
        .map(|txs| {
            txs.iter()
                .filter_map(Value::as_str)
                .filter_map(tx_hash_from_raw)
                .collect()
        })
        .unwrap_or_default()
}

fn log_index(log: &Value) -> Option<u64> {
    log.get("logIndex")
        .or_else(|| log.get("log_index"))
        .and_then(parse_u64_value)
}

fn log_address(log: &Value) -> Option<Address> {
    log.get("address")
        .and_then(Value::as_str)
        .and_then(|s| Address::from_str(s).ok())
}

fn receipt_logs(receipt: &Value) -> Vec<&Value> {
    receipt
        .get("logs")
        .and_then(Value::as_array)
        .map(|logs| logs.iter().collect())
        .unwrap_or_default()
}

fn receipts(msg: &RawFlashblockMessage, target_pool: Option<Address>) -> Vec<ReceiptProbe> {
    let Some(receipts) = msg
        .metadata
        .as_ref()
        .and_then(|m| m.get("receipts"))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };

    receipts
        .iter()
        .map(|(tx_hash, receipt)| {
            let logs = receipt_logs(receipt);
            let mut matching_log_data = Vec::new();
            for (log_idx, log) in logs.iter().enumerate() {
                let addr = log_address(log);
                if target_pool.is_some() && addr != target_pool {
                    continue;
                }
                let topics = log
                    .get("topics")
                    .and_then(|t| t.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|t| t.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let data = log
                    .get("data")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();
                matching_log_data.push((
                    log_idx,
                    addr.map(|a| format!("{:?}", a)).unwrap_or_default(),
                    topics,
                    data,
                ));
            }

            ReceiptProbe {
                tx_hash: tx_hash.clone(),
                cumulative_gas_used: receipt
                    .get("cumulativeGasUsed")
                    .or_else(|| receipt.get("cumulative_gas_used"))
                    .and_then(parse_u64_value),
                transaction_index: receipt
                    .get("transactionIndex")
                    .or_else(|| receipt.get("transaction_index"))
                    .and_then(parse_u64_value),
                logs_len: logs.len(),
                matching_logs_len: matching_log_data.len(),
                first_log_index: matching_log_data.first().map(|(i, _, _, _)| *i as u64),
                matching_log_data,
            }
        })
        .collect()
}

fn parse_message(message: Message) -> Result<Option<RawFlashblockMessage>> {
    let payload = match message {
        Message::Text(text) => text.as_bytes().to_vec(),
        Message::Binary(bin) => bin.to_vec(),
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => return Ok(None),
        Message::Close(_) => return Ok(None),
    };

    if let Ok(msg) = serde_json::from_slice::<RawFlashblockMessage>(&payload) {
        return Ok(Some(msg));
    }

    let mut decompressed = Vec::new();
    let mut reader = brotli::Decompressor::new(payload.as_slice(), 4096);
    reader
        .read_to_end(&mut decompressed)
        .context("failed to brotli-decompress raw flashblock message")?;
    let msg = serde_json::from_slice::<RawFlashblockMessage>(&decompressed)
        .context("failed to decode decompressed raw flashblock message")?;
    Ok(Some(msg))
}

fn print_block_summary(block: u64, summary: &BlockSummary) {
    if summary.messages == 0 {
        return;
    }

    let monotonic = summary.indexes.windows(2).all(|w| w[0] <= w[1]);
    println!(
        "[block-summary] block={} messages={} payload_ids={:?} indexes={:?} indexes_monotonic={} diff_txs={} receipts={} unique_receipts={} duplicate_receipts={} logs={} matching_logs={}",
        block,
        summary.messages,
        summary.payload_ids,
        summary.indexes,
        monotonic,
        summary.diff_txs,
        summary.receipts,
        summary.receipt_hashes.len(),
        summary.duplicate_receipts,
        summary.logs,
        summary.matching_logs,
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();

    let raw_ws = std::env::var("RAW_FLASHBLOCKS_WS")
        .unwrap_or_else(|_| "wss://ws.xlayer.tech/flashblocks".to_string());
    let run_secs = std::env::var("RUN_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(120); // default 2 minutes
    let target_pool = std::env::var("TARGET_POOL")
        .ok()
        .and_then(|v| Address::from_str(&v).ok());

    println!("=== XLayer Flashblocks Structure Probe ===");
    println!("raw_ws={raw_ws}");
    println!("run_secs={run_secs} target_pool={target_pool:?}");

    let (mut ws, _) = connect_async(raw_ws.clone())
        .await
        .with_context(|| format!("failed to connect to {raw_ws}"))?;
    println!("[probe] connected");

    let started = Instant::now();
    let mut messages = 0usize;
    let mut current_block: Option<u64> = None;
    let mut block_summary = BlockSummary::default();
    let mut global_tracker = GlobalDuplicateTracker::default();

    while started.elapsed() < Duration::from_secs(run_secs) {
        let Some(message_result) = tokio::time::timeout(Duration::from_secs(5), ws.next()).await?
        else {
            break;
        };
        let message = message_result?;

        if matches!(message, Message::Ping(_)) {
            if let Message::Ping(v) = message {
                let _ = ws.send(Message::Pong(v)).await;
            }
            continue;
        }

        let Some(msg) = parse_message(message)? else {
            continue;
        };
        messages += 1;

        let block = block_number(&msg).unwrap_or_default();
        if current_block.is_some_and(|b| b != block) {
            print_block_summary(current_block.unwrap(), &block_summary);
            block_summary = BlockSummary::default();
        }
        current_block = Some(block);

        let payload_id = msg.payload_id.clone().unwrap_or_else(|| "<none>".into());
        let index = msg.index.unwrap_or_default();
        let diff_hashes = diff_tx_hashes(&msg);
        let receipt_rows = receipts(&msg, target_pool);
        let mut by_cumulative: Vec<&ReceiptProbe> = receipt_rows.iter().collect();
        by_cumulative.sort_by_key(|r| r.cumulative_gas_used.unwrap_or_default());

        let diff_pos: HashMap<&str, usize> = diff_hashes
            .iter()
            .enumerate()
            .map(|(idx, hash)| (hash.as_str(), idx))
            .collect();
        let receipt_order_matches_diff = by_cumulative
            .iter()
            .filter_map(|r| diff_pos.get(r.tx_hash.as_str()).copied())
            .collect::<Vec<_>>()
            .windows(2)
            .all(|w| w[0] <= w[1]);

        block_summary.messages += 1;
        block_summary.payload_ids.insert(payload_id.clone());
        block_summary.indexes.push(index);
        block_summary.diff_txs += diff_hashes.len();
        block_summary.receipts += receipt_rows.len();
        block_summary.logs += receipt_rows.iter().map(|r| r.logs_len).sum::<usize>();
        block_summary.matching_logs += receipt_rows
            .iter()
            .map(|r| r.matching_logs_len)
            .sum::<usize>();
        for receipt in &receipt_rows {
            if !block_summary.receipt_hashes.insert(receipt.tx_hash.clone()) {
                block_summary.duplicate_receipts += 1;
                eprintln!(
                    "\n⚠️  WITHIN-BLOCK DUPLICATE: tx_hash={} block={}",
                    receipt.tx_hash, block
                );
            }
        }

        // 全局去重跟踪：记录所有匹配目标池的 tx_hash
        for receipt in &receipt_rows {
            if receipt.matching_logs_len > 0 {
                global_tracker.record(
                    receipt.tx_hash.clone(),
                    block,
                    payload_id.clone(),
                    index,
                    receipt
                        .matching_log_data
                        .first()
                        .map(|(i, _, _, _)| *i)
                        .unwrap_or(0),
                );
            }
        }

        // 打印每条消息
        if receipt_rows.iter().any(|r| r.matching_logs_len > 0) {
            println!(
                "[msg#{messages}][target-f0und] block={} payload={} index={} matching_receipts={}",
                block,
                payload_id,
                index,
                receipt_rows
                    .iter()
                    .filter(|r| r.matching_logs_len > 0)
                    .count(),
            );
            for receipt in &receipt_rows {
                if receipt.matching_logs_len == 0 {
                    continue;
                }
                for (log_idx, addr, topics, data) in &receipt.matching_log_data {
                    println!(
                        "  tx={} log[{}] addr={} topic0={:?} data_len={}",
                        receipt.tx_hash,
                        log_idx,
                        addr,
                        topics
                            .first()
                            .map(|s| &s[..40.min(s.len())])
                            .unwrap_or("none"),
                        data.len(),
                    );
                    // 打印 content_hash (keccak256 of address+tx_hash+block+topics+data)
                    let content_input = format!(
                        "{}|{}|{}|{:?}|{}",
                        addr, receipt.tx_hash, block, topics, data
                    );
                    let content_hash = keccak256(content_input.as_bytes());
                    println!("    content_hash={:?}", content_hash);
                }
            }
        }

        // 每 30 秒打印一次进度
        if messages % 200 == 0 {
            println!(
                "[progress] messages={} elapsed={:.0}s",
                messages,
                started.elapsed().as_secs_f64()
            );
        }
    }

    if let Some(block) = current_block {
        print_block_summary(block, &block_summary);
    }

    global_tracker.print_summary();

    println!(
        "\n[summary] messages={} elapsed_secs={:.0}",
        messages,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

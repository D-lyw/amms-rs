//! XLayer flashblocks receipts 覆盖性与 status 字段 probe。
//!
//! 验证 caliber 实时同步按 receipt status 过滤（2026-08-09 P0 修复）所需前提：
//! 1. 每个 slice 的 `metadata.receipts` 是否覆盖 `diff.transactions` 全部交易
//!    （含无日志的 caliber `batchUpdateParameters` 交易）——键是否为 raw 交易 keccak256；
//! 2. receipt 是否携带 `status` 字段（`0x1` 成功 / `0x0` 回滚）；
//! 3. 跨 slice 视角：同一 payload（区块）内 receipts 是否会滞后到达。
//!
//! 用法:
//! ```bash
//! cargo run --example xlayer_flashblocks_receipt_probe
//! ```
//! 环境变量:
//! - `XLAYER_FLASHBLOCKS_WS`  WS 端点（默认 `wss://ws.xlayer.tech/flashblocks`）
//! - `RUN_SECS`               运行时长（默认 90）
//! - `MAX_FRAMES`             最多处理帧数（默认 0 = 不限制）

use std::collections::HashMap;
use std::io::Read;
use std::time::{Duration, Instant};

use alloy::primitives::keccak256;
use amms::amms::caliber_prop::{extract_input_from_raw_tx, extract_to_from_raw_tx};
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::Message;

const DEFAULT_WS: &str = "wss://ws.xlayer.tech/flashblocks";
const CALIBER_CONTRACT: &str = "0x154586b2479b9a11e3d4db90024dc0e26f097312";

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let ws_url = std::env::var("XLAYER_FLASHBLOCKS_WS").unwrap_or_else(|_| DEFAULT_WS.to_string());
    let run_secs: u64 = std::env::var("RUN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(90);
    let max_frames: usize = std::env::var("MAX_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    println!("=== XLayer flashblocks receipts coverage probe ===");
    println!("ws: {ws_url}, run_secs: {run_secs}, max_frames: {max_frames}");

    let (mut ws, _) = connect_async(ws_url.clone()).await?;
    println!("connected");

    let deadline = Instant::now() + Duration::from_secs(run_secs);

    let mut frames = 0usize;
    let mut tx_gt_receipts = 0usize; // 本帧 tx 数 > receipts 数
    let mut tx_lt_receipts = 0usize; // 本帧 tx 数 < receipts 数
    let mut matched = 0usize; // raw keccak 命中 receipt 键
    let mut unmatched = 0usize; // raw keccak 未命中
    let mut missing_status = 0usize;
    let mut status_0 = 0usize; // status=0x0 回滚
    let mut status_1 = 0usize; // status=0x1 成功
    let mut status_other: HashMap<String, usize> = HashMap::new();
    let mut caliber_to_hits = 0usize; // to == caliber 合约（任意选择器）
    let mut caliber_txs = 0usize;
    let mut caliber_with_receipt = 0usize;
    let mut caliber_status_1 = 0usize;
    let mut caliber_status_0 = 0usize;
    let mut frames_with_caliber = 0usize;
    // 跨 slice 视角：payload_id -> (累计 tx 数, 累计 receipts 数, 是否打印过)
    let mut payload_cum: HashMap<String, (usize, usize)> = HashMap::new();

    while Instant::now() < deadline {
        let next = tokio::time::timeout(Duration::from_secs(15), ws.next()).await;
        let Some(Ok(msg)) = next.ok().flatten() else {
            break;
        };
        let payload = match msg {
            Message::Text(t) => t.as_bytes().to_vec(),
            Message::Binary(b) => {
                if serde_json::from_slice::<Value>(&b).is_ok() {
                    b.to_vec()
                } else {
                    let mut reader = brotli::Decompressor::new(b.as_ref(), 4096);
                    let mut buf = Vec::new();
                    if reader.read_to_end(&mut buf).is_err() {
                        continue;
                    }
                    buf
                }
            }
            Message::Ping(v) => {
                let _ = ws.send(Message::Pong(v)).await;
                continue;
            }
            Message::Pong(_) => continue,
            Message::Close(_) => break,
            Message::Frame(_) => continue,
        };
        let Ok(fb) = serde_json::from_slice::<Value>(&payload) else {
            continue;
        };
        frames += 1;

        let pid = fb
            .get("payload_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let idx = fb.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
        let txs = fb
            .get("diff")
            .and_then(|d| d.get("transactions"))
            .and_then(|t| t.as_array());
        let receipts = fb
            .get("metadata")
            .and_then(|m| m.get("receipts"))
            .and_then(|r| r.as_object());

        let tx_count = txs.map(|t| t.len()).unwrap_or(0);
        let rcpt_count = receipts.map(|r| r.len()).unwrap_or(0);
        if tx_count > rcpt_count {
            tx_gt_receipts += 1;
        } else if tx_count < rcpt_count {
            tx_lt_receipts += 1;
        }

        let cum = payload_cum.entry(pid.clone()).or_insert((0, 0));
        cum.0 += tx_count;
        cum.1 += rcpt_count;

        let mut frame_caliber = 0usize;
        let mut first_receipt_keys = None;

        if let Some(txs) = txs {
            for raw_hex in txs {
                let Some(s) = raw_hex.as_str() else { continue };
                let raw_hex = s.strip_prefix("0x").unwrap_or(s);
                let Ok(raw) = alloy::hex::decode(raw_hex) else {
                    continue;
                };
                let h = format!("{:#x}", keccak256(&raw));
                let rcpt = receipts.and_then(|r| r.get(&h));
                match rcpt {
                    Some(r) => {
                        matched += 1;
                        if first_receipt_keys.is_none() {
                            first_receipt_keys =
                                r.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>());
                        }
                        match r.get("status").and_then(|s| s.as_str()) {
                            Some("0x1") => status_1 += 1,
                            Some("0x0") => status_0 += 1,
                            Some(other) => *status_other.entry(other.to_string()).or_insert(0) += 1,
                            None => missing_status += 1,
                        }
                    }
                    None => unmatched += 1,
                }

                // caliber 交易检测
                if let Some(to) = extract_to_from_raw_tx(&raw) {
                    if to.to_string().eq_ignore_ascii_case(CALIBER_CONTRACT) {
                        caliber_to_hits += 1;
                        let is_batch = extract_input_from_raw_tx(&raw)
                            .map(|i| i.starts_with(&[0x00, 0x8d, 0xcc, 0x8e]))
                            .unwrap_or(false);
                        if is_batch {
                            caliber_txs += 1;
                            frame_caliber += 1;
                            if rcpt.is_some() {
                                caliber_with_receipt += 1;
                            }
                            match rcpt.and_then(|r| r.get("status")).and_then(|s| s.as_str()) {
                                Some("0x1") => caliber_status_1 += 1,
                                Some("0x0") => caliber_status_0 += 1,
                                _ => {}
                            }
                        }
                    }
                }
            }
        }

        if frame_caliber > 0 {
            frames_with_caliber += 1;
        }

        println!(
            "frame={frames} payload={pid} idx={idx} txs={tx_count} receipts={rcpt_count} caliber={frame_caliber} unmatched={unmatched}"
        );
        if let Some(keys) = first_receipt_keys {
            println!("  receipt keys sample: {keys:?}");
            first_receipt_keys = None;
        }

        if max_frames > 0 && frames >= max_frames {
            break;
        }
    }

    println!("\n==== SUMMARY ====");
    println!("frames={frames}");
    println!("frames with txs>receipts: {tx_gt_receipts}");
    println!("frames with txs<receipts: {tx_lt_receipts}");
    println!("receipt-key matched: {matched}");
    println!("receipt-key unmatched: {unmatched}");
    println!("receipt status=0x1: {status_1}");
    println!("receipt status=0x0: {status_0}");
    println!("receipt status missing: {missing_status}");
    if !status_other.is_empty() {
        println!("receipt status other: {status_other:?}");
    }
    println!("txs to caliber contract (any selector): {caliber_to_hits}");
    println!("caliber batchUpdateParameters txs: {caliber_txs}");
    println!("  with receipt: {caliber_with_receipt}");
    println!("  status=0x1: {caliber_status_1}");
    println!("  status=0x0: {caliber_status_0}");
    println!("frames containing caliber tx: {frames_with_caliber}");
    println!("payload cumulative (payload_id -> (txs, receipts)):");
    for (k, (t, r)) in &payload_cum {
        println!("  {k}: txs={t} receipts={r} diff={}", *t as i64 - *r as i64);
    }
    Ok(())
}

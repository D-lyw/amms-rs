//! Robinhood Chain 订阅源协议探测脚本。
//!
//! Robinhood Chain 是 Arbitrum Orbit Nitro 链，推测其 sequencer feed
//! 使用标准的 Arbitrum Nitro BroadcastMessage JSON 格式（与 Arbitrum One 相同）。
//! 此脚本同时尝试 Arbitrum 格式和 flashblock 格式来自动识别。
//!
//! 使用:
//!   ROBINHOOD_WS=wss://feed.mainnet.chain.robinhood.com cargo run --example robinhood_feed_probe
//!   ROBINHOOD_WS=wss://feed.testnet.chain.robinhood.com cargo run --example robinhood_feed_probe
//!   RUN_SECS=60 cargo run --example robinhood_feed_probe

use eyre::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use std::time::{Duration, Instant};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::Message;

// ============================================================
// Arbitrum Nitro BroadcastMessage 格式
// ============================================================

/// Arbitrum Nitro 顶层广播消息
#[derive(Debug, Deserialize)]
struct ArbitrumBroadcastMessage {
    version: Option<i32>,
    #[serde(default)]
    messages: Vec<ArbitrumBroadcastFeedMessage>,
    confirmed_sequence_number_message: Option<Value>,
    #[serde(flatten)]
    extra: std::collections::HashMap<String, Value>,
}

/// Arbitrum Nitro 每条 feed 消息
#[derive(Debug, Deserialize)]
struct ArbitrumBroadcastFeedMessage {
    sequence_number: Option<Value>, // uint64, JSON 中可能是数字或字符串
    message: Option<Value>,
    signature: Option<Value>,
    block_metadata: Option<Value>,
    #[serde(flatten)]
    extra: std::collections::HashMap<String, Value>,
}

// ============================================================
// Flashblock 格式 (XLayer / Base 风格)
// ============================================================

#[derive(Debug, Deserialize)]
struct FlashblockMessage {
    payload_id: Option<String>,
    index: Option<u64>,
    #[serde(default)]
    base: Option<Value>,
    #[serde(default)]
    diff: Option<Value>,
    #[serde(default)]
    metadata: Option<Value>,
}

// ============================================================
// Helper
// ============================================================

fn parse_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => {
            s.strip_prefix("0x")
                .or_else(|| s.strip_prefix("0X"))
                .and_then(|h| u64::from_str_radix(h, 16).ok())
                .or_else(|| s.parse().ok())
        }
        _ => None,
    }
}

fn format_preview(val: &Value, max_depth: usize) -> String {
    match val {
        Value::Null => "null".into(),
        Value::Bool(b) => format!("{b}"),
        Value::Number(n) => format!("{n}"),
        Value::String(s) => {
            if s.len() > 80 {
                format!("\"{}...\" ({} chars)", &s[..80], s.len())
            } else {
                format!("\"{s}\"")
            }
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                "[]".into()
            } else if max_depth == 0 {
                format!("[{} items]", arr.len())
            } else {
                let items: String = arr
                    .iter()
                    .take(5)
                    .map(|v| format_preview(v, max_depth - 1))
                    .collect::<Vec<_>>()
                    .join(", ");
                let rest = if arr.len() > 5 {
                    format!(" ... ({} total)", arr.len())
                } else {
                    String::new()
                };
                format!("[{items}{rest}]")
            }
        }
        Value::Object(obj) => {
            if obj.is_empty() {
                "{}".into()
            } else if max_depth == 0 {
                format!("{{{}}}", obj.keys().cloned().collect::<Vec<_>>().join(", "))
            } else {
                let mut s = String::from("{");
                for (k, v) in obj.iter().take(6) {
                    s.push_str(&format!("\n    {k}: {}", format_preview(v, max_depth - 1)));
                }
                if obj.len() > 6 {
                    s.push_str(&format!("\n    ... and {} more fields", obj.len() - 6));
                }
                s.push_str("\n  }");
                s
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("ROBINHOOD_WS")
        .unwrap_or_else(|_| "wss://feed.mainnet.chain.robinhood.com".to_string());
    let run_secs: u64 = std::env::var("RUN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let max_messages: Option<usize> =
        std::env::var("MAX_MESSAGES").ok().and_then(|v| v.parse().ok());

    println!("================================================");
    println!("  Robinhood Chain Feed Protocol Probe");
    println!("================================================");
    println!("WS URL:       {ws_url}");
    println!("Run seconds:  {run_secs}s");
    println!("Max messages: {max_messages:?}");
    println!();

    let (mut ws, _) = connect_async(&ws_url)
        .await
        .with_context(|| format!("Failed to connect to {ws_url}"))?;
    println!("[probe] Connected to feed\n");

    let started = Instant::now();
    let deadline = started + Duration::from_secs(run_secs);

    let mut total_messages = 0usize;
    let mut msg_sizes: Vec<usize> = Vec::new();
    let mut msg_intervals: Vec<Duration> = Vec::new();
    let mut last_time: Option<Instant> = None;

    // 格式探测结果
    let mut arbitrum_format_ok = false;
    let mut flashblock_format_ok = false;
    let mut arbitrum_messages = 0usize;
    let mut flashblock_messages = 0usize;
    let mut unknown_format = 0usize;
    let mut max_seq: Option<u64> = None;
    let mut min_seq: Option<u64> = None;

    loop {
        if max_messages.map_or(false, |m| total_messages >= m) {
            println!("[probe] max_messages ({max_messages:?}) reached");
            break;
        }
        if Instant::now() >= deadline {
            println!("[probe] deadline reached");
            break;
        }

        let next = tokio::time::timeout(Duration::from_secs(5), ws.next()).await;
        let maybe_msg = match next {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(msg_result) = maybe_msg else {
            println!("[probe] Stream ended");
            break;
        };
        let message = match msg_result {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[probe][warn] Receive error: {e}");
                continue;
            }
        };

        total_messages += 1;
        if let Some(t) = last_time {
            msg_intervals.push(Instant::now().duration_since(t));
        }
        last_time = Some(Instant::now());

        let (payload, msg_type_label) = match message {
            Message::Text(text) => {
                msg_sizes.push(text.len());
                (text.as_bytes().to_vec(), "text")
            }
            Message::Binary(bin) => {
                msg_sizes.push(bin.len());
                (bin.to_vec(), "binary")
            }
            Message::Ping(v) => {
                let _ = ws.send(Message::Pong(v)).await;
                continue;
            }
            Message::Pong(_) => continue,
            Message::Close(_) => {
                println!("[probe] Close frame received");
                break;
            }
            Message::Frame(_) => continue,
        };

        // --- 格式探测 ---
        // 先尝试 Arbitrum Nitro 格式
        if let Ok(arb_msg) = serde_json::from_slice::<ArbitrumBroadcastMessage>(&payload) {
            if !arbitrum_format_ok {
                arbitrum_format_ok = true;
                println!(
                    "[detect] ✅ 消息 #{total_messages}: 匹配 Arbitrum Nitro BroadcastMessage 格式! \
                     version={:?}, messages_count={}",
                    arb_msg.version,
                    arb_msg.messages.len()
                );
                // 检查额外的顶层字段
                if !arb_msg.extra.is_empty() {
                    println!(
                        "  [extra top-level fields]: {}",
                        arb_msg.extra.keys().cloned().collect::<Vec<_>>().join(", ")
                    );
                }
            }
            arbitrum_messages += 1;

            // 解析序列号
            for feed_msg in &arb_msg.messages {
                if let Some(ref sn) = feed_msg.sequence_number {
                    if let Some(seq) = parse_u64(sn) {
                        max_seq = Some(max_seq.map_or(seq, |m| m.max(seq)));
                        min_seq = Some(min_seq.map_or(seq, |m| m.min(seq)));
                    }
                }
            }

            // 打印前 5 条
            if total_messages <= 5 {
                println!("\n--- Arbitrum Message #{total_messages} ({msg_type_label}, {} bytes) ---", payload.len());
                for (i, feed_msg) in arb_msg.messages.iter().enumerate() {
                    println!("  messages[{i}]:");
                    println!("    sequence_number: {:?}", feed_msg.sequence_number);
                    if let Some(ref meta) = feed_msg.block_metadata {
                        println!("    block_metadata: {}", format_preview(meta, 1));
                    }
                    if let Some(ref msg) = feed_msg.message {
                        println!("    message: {}", format_preview(msg, 1));
                    }
                    println!("    signature: {:?}", feed_msg.signature.as_ref().map(|_| "<present>"));
                }
                if let Some(ref confirmed) = arb_msg.confirmed_sequence_number_message {
                    println!("  confirmed_sequence_number_message: {confirmed:?}");
                }
                println!();
            }
        } else if let Ok(fb_msg) = serde_json::from_slice::<FlashblockMessage>(&payload) {
            if !flashblock_format_ok {
                flashblock_format_ok = true;
                println!(
                    "[detect] ✅ 消息 #{total_messages}: 匹配 Flashblock 格式! \
                     payload_id={:?}, index={:?}",
                    fb_msg.payload_id, fb_msg.index
                );
            }
            flashblock_messages += 1;

            if total_messages <= 5 {
                println!("\n--- Flashblock Message #{total_messages} ({msg_type_label}, {} bytes) ---", payload.len());
                println!("  payload_id: {:?}", fb_msg.payload_id);
                println!("  index: {:?}", fb_msg.index);
                if let Some(base) = fb_msg.base {
                    println!("  base: {}", format_preview(&base, 1));
                }
                if let Some(diff) = fb_msg.diff {
                    println!("  diff: {}", format_preview(&diff, 1));
                }
                if let Some(meta) = fb_msg.metadata {
                    println!("  metadata: {}", format_preview(&meta, 1));
                }
                println!();
            }
        } else {
            unknown_format += 1;
            if total_messages <= 5 {
                println!(
                    "\n--- Unknown Message #{total_messages} ({msg_type_label}, {} bytes) ---",
                    payload.len()
                );
                let preview = String::from_utf8_lossy(&payload[..payload.len().min(300)]);
                println!("  raw preview: {preview}");
                println!();
            }
        }
    }

    // ============================================================
    // 汇总报告
    // ============================================================
    let elapsed = started.elapsed();

    msg_sizes.sort_unstable();
    msg_intervals.sort_unstable();

    let p50_size = msg_sizes.get(msg_sizes.len() / 2).copied().unwrap_or(0);
    let p95_size = msg_sizes
        .get((msg_sizes.len() as f64 * 0.95) as usize)
        .copied()
        .unwrap_or(0);
    let p50_int = msg_intervals
        .get(msg_intervals.len() / 2)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let p95_int = msg_intervals
        .get((msg_intervals.len() as f64 * 0.95) as usize)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    println!("\n\n================================================");
    println!("  Probe Summary Report");
    println!("================================================");
    println!("Elapsed:             {:.1}s", elapsed.as_secs_f64());
    println!("Total messages:      {total_messages}");
    println!("Message rate:        {:.1}/s", total_messages as f64 / elapsed.as_secs_f64().max(0.001));
    println!("Message size (P50):  {p50_size} bytes");
    println!("Message size (P95):  {p95_size} bytes");
    println!("Msg interval (P50):  {p50_int}ms");
    println!("Msg interval (P95):  {p95_int}ms");
    println!();
    println!("Arbitrum format:     {arbitrum_messages} msgs");
    println!("Flashblock format:   {flashblock_messages} msgs");
    println!("Unknown format:      {unknown_format} msgs");
    if let (Some(min), Some(max)) = (min_seq, max_seq) {
        println!("Sequence range:      {min} .. {max} ({} total)", max - min + 1);
    }

    println!("\n--- FORMAT CONCLUSION ---");
    if arbitrum_messages > 0 && flashblock_messages == 0 {
        println!("✅ Robinhood Chain 使用标准的 Arbitrum Nitro BroadcastMessage 格式。");
        println!("   这与 Robinhood 官方文档 'Arbitrum Orbit (Nitro) 链' 的描述一致。");
        println!("\n   实现策略: 复用现有的 ArbitrumFeedPull 机制，主要差异点:");
        println!("     - WebSocket URL: 自定义 (feed.mainnet.chain.robinhood.com)");
        println!("     - L2_OFFSET: 需要确定 (链部署的 L1 区块号)");
        println!("     - Feed URL 和 L2_OFFSET 通过 Robinhood 链 ID 4663 映射");
    } else if flashblock_messages > 0 && arbitrum_messages == 0 {
        println!("✅ Robinhood Chain 使用 Flashblock 格式 (XLayer/Base 风格)。");
        println!("   实现策略: 参考 xlayer_flashblocks.rs 实现类似的解析器。");
    } else if arbitrum_messages > 0 && flashblock_messages > 0 {
        println!("⚠️  混合格式! 部分消息是 Arbitrum 格式，部分是 Flashblock 格式。");
        println!("   需要进一步分析两种消息的上下文关系。");
    } else {
        println!("❌ 无法识别 Robinhood Chain feed 的消息格式。");
        println!("   需要进一步调研。");
    }
    println!("================================================\n");

    Ok(())
}

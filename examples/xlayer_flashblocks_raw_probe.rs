use eyre::Context;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::io::Read;
use std::time::{Duration, Instant};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::Message;

/// 极简消息结构 —— 仅探测顶层字段和 receipts 中是否包含 logs
#[derive(Debug, Deserialize)]
struct ProbeMessage {
    payload_id: Option<String>,
    index: Option<u64>,

    #[serde(default)]
    base: Option<serde_json::Value>,

    #[serde(default)]
    diff: Option<serde_json::Value>,

    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

/// 打印结构体字段（递归深度 ≤ 3 层，避免爆炸）
fn print_value_preview(label: &str, v: &serde_json::Value, max_depth: usize) {
    match v {
        serde_json::Value::Object(map) => {
            println!("  {label}: {{");
            for (k, val) in map.iter() {
                let preview = match val {
                    serde_json::Value::String(s) => {
                        let truncated = if s.len() > 80 {
                            format!("{}...", &s[..80])
                        } else {
                            s.clone()
                        };
                        format!("\"{truncated}\"")
                    }
                    serde_json::Value::Number(n) => format!("{n}"),
                    serde_json::Value::Bool(b) => format!("{b}"),
                    serde_json::Value::Array(arr) => {
                        if arr.is_empty() {
                            "[]".to_string()
                        } else if max_depth == 0 {
                            format!("[{} items]", arr.len())
                        } else {
                            let items: Vec<String> = arr
                                .iter()
                                .take(3)
                                .map(|item| {
                                    let s = match item {
                                        serde_json::Value::String(s) => {
                                            if s.len() > 40 {
                                                format!("\"{}...\"", &s[..40])
                                            } else {
                                                format!("\"{s}\"")
                                            }
                                        }
                                        other => format!("{other}"),
                                    };
                                    s
                                })
                                .collect();
                            let more = if arr.len() > 3 {
                                format!(" ... ({} total)", arr.len())
                            } else {
                                String::new()
                            };
                            format!("[{}{}]", items.join(", "), more)
                        }
                    }
                    serde_json::Value::Object(_) if max_depth == 0 => {
                        "{\"…\"}".to_string()
                    }
                    serde_json::Value::Object(inner) => {
                        let fields: Vec<String> =
                            inner.keys().take(8).map(|k| k.clone()).collect();
                        let more = if inner.len() > 8 { ", …" } else { "" };
                        format!("{{{}}}{}", fields.join(", "), more)
                    }
                    serde_json::Value::Null => "null".to_string(),
                };
                println!("    {k}: {preview}");
            }
            println!("  }}");
        }
        serde_json::Value::Array(arr) => {
            println!("  {label}: [{} items]", arr.len());
            if max_depth > 0 {
                for (i, item) in arr.iter().take(5).enumerate() {
                    print_value_preview(&format!("  [{i}]"), item, max_depth - 1);
                }
            }
        }
        other => {
            println!("  {label}: {other}");
        }
    }
}

fn analyze_metadata(meta: &serde_json::Value) {
    let Some(obj) = meta.as_object() else {
        println!("  [metadata] 不是 Object, 是: {:?}", meta);
        return;
    };

    for (k, v) in obj.iter() {
        match k.as_str() {
            "block_number" => {
                println!("  metadata.block_number: {v}");
            }
            "receipts" => {
                let Some(rcpts) = v.as_object() else {
                    println!("  metadata.receipts: {v} (not an object)");
                    continue;
                };
                println!("  metadata.receipts: {} receipts", rcpts.len());

                // 查看前 3 个收据的结构
                for (i, (tx_hash, receipt)) in rcpts.iter().take(3).enumerate() {
                    println!("    receipt[{}] tx={}", i, &tx_hash[..16.min(tx_hash.len())]);

                    match receipt {
                        serde_json::Value::Object(fields) => {
                            for (fk, fv) in fields.iter() {
                                match fk.as_str() {
                                    "logs" => {
                                        match fv {
                                            serde_json::Value::Array(logs) => {
                                                println!("      logs: [{} items]", logs.len());
                                                // 打印第一个日志的结构
                                                if let Some(first_log) = logs.first() {
                                                    print_value_preview("      log[0]", first_log, 1);
                                                }
                                            }
                                            other => {
                                                println!("      logs: {other} (not an array, type={})",
                                                    serde_json::value::to_value(other).map(|v| format!("{v}")).unwrap_or_default()
                                                );
                                            }
                                        }
                                    }
                                    "transactionIndex" => {
                                        println!("      {fk}: {fv}");
                                    }
                                    _ => {
                                        let preview = match fv {
                                            serde_json::Value::String(s)
                                                if s.len() > 60 =>
                                            {
                                                format!("\"{}...\"", &s[..60])
                                            }
                                            serde_json::Value::Object(o) => {
                                                format!("{{{}}}", o.keys().cloned().collect::<Vec<_>>().join(", "))
                                            }
                                            other => format!("{other}"),
                                        };
                                        println!("      {fk}: {preview}");
                                    }
                                }
                            }
                        }
                        other => {
                            println!("    receipt value: {other}");
                        }
                    }
                }
                if rcpts.len() > 3 {
                    println!("    ... and {} more receipts", rcpts.len() - 3);
                }
            }
            "new_account_balances" => {
                if let Some(balances) = v.as_object() {
                    println!("  metadata.new_account_balances: {} accounts", balances.len());
                } else {
                    println!("  metadata.new_account_balances: {v}");
                }
            }
            _ => {
                println!("  metadata.{k}: (len={})", match v {
                    serde_json::Value::String(s) => s.len(),
                    serde_json::Value::Array(a) => a.len(),
                    serde_json::Value::Object(o) => o.len(),
                    _ => 0,
                });
            }
        }
    }
}

fn analyze_diff(diff: &serde_json::Value) {
    let Some(obj) = diff.as_object() else {
        println!("  [diff] 不是 Object, 是: {:?}", diff);
        return;
    };

    for (k, v) in obj.iter() {
        match k.as_str() {
            "logs_bloom" => {
                let s = v.as_str().unwrap_or("");
                let truncated = if s.len() > 40 {
                    format!("{}... ({} hex chars)", &s[..40], s.len())
                } else {
                    s.to_string()
                };
                println!("  diff.logs_bloom: {truncated}");
            }
            "transactions" => {
                if let Some(txs) = v.as_array() {
                    println!("  diff.transactions: [{} txs]", txs.len());
                    for (i, tx) in txs.iter().enumerate().take(3) {
                        let s = tx.as_str().unwrap_or("");
                        println!("    tx[{}]: {}... ({} hex chars)", i, &s[..20.min(s.len())], s.len());
                    }
                    if txs.len() > 3 {
                        println!("    ... and {} more", txs.len() - 3);
                    }
                }
            }
            _ => {
                let preview = match v {
                    serde_json::Value::String(s) => {
                        let truncated = if s.len() > 40 {
                            format!("{}...", &s[..40])
                        } else {
                            s.clone()
                        };
                        format!("\"{truncated}\"")
                    }
                    serde_json::Value::Number(n) => format!("{n}"),
                    serde_json::Value::Array(a) => format!("[{} items]", a.len()),
                    serde_json::Value::Object(o) => {
                        format!("{{{}}}", o.keys().cloned().collect::<Vec<_>>().join(", "))
                    }
                    serde_json::Value::Bool(b) => format!("{b}"),
                    serde_json::Value::Null => "null".to_string(),
                };
                println!("  diff.{k}: {preview}");
            }
        }
    }
}

fn analyze_base(base: &serde_json::Value) {
    let Some(obj) = base.as_object() else {
        println!("  [base] 不是 Object, 是: {:?}", base);
        return;
    };

    for (k, v) in obj.iter() {
        match v {
            serde_json::Value::String(s) => {
                let truncated = if s.len() > 40 {
                    format!("{}...", &s[..40])
                } else {
                    s.clone()
                };
                println!("  base.{k}: \"{truncated}\"");
            }
            serde_json::Value::Number(n) => println!("  base.{k}: {n}"),
            serde_json::Value::Array(a) => println!("  base.{k}: [{} items]", a.len()),
            serde_json::Value::Object(o) => {
                println!("  base.{k}: {{{}}}", o.keys().cloned().collect::<Vec<_>>().join(", "));
            }
            serde_json::Value::Bool(b) => println!("  base.{k}: {b}"),
            serde_json::Value::Null => println!("  base.{k}: null"),
        }
    }
}

fn print_full_message(fb: &ProbeMessage, raw_preview: &str) {
    println!("\n========================================");
    println!("=== Flashblock Message Full Dump ===");
    println!("========================================");
    println!("payload_id: {:?}", fb.payload_id);
    println!("index: {:?}", fb.index);
    println!("raw_preview: {raw_preview}");

    if let Some(base) = &fb.base {
        println!("\n>> base:");
        analyze_base(base);
    } else {
        println!("\n>> base: None");
    }

    if let Some(diff) = &fb.diff {
        println!("\n>> diff:");
        analyze_diff(diff);
    } else {
        println!("\n>> diff: None");
    }

    if let Some(meta) = &fb.metadata {
        println!("\n>> metadata:");
        analyze_metadata(meta);
    } else {
        println!("\n>> metadata: None");
    }
    println!("========================================\n");
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt::init();

    let raw_ws = std::env::var("RAW_FLASHBLOCKS_WS")
        .unwrap_or_else(|_| "wss://ws.xlayer.tech/flashblocks".to_string());

    let run_secs: u64 = std::env::var("RUN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let max_messages: Option<usize> = std::env::var("MAX_MESSAGES")
        .ok()
        .and_then(|v| v.parse().ok());

    // dump_first_n: 打印前 N 条消息的完整结构，之后只打印统计
    let dump_first: usize = std::env::var("DUMP_FIRST")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);

    println!("=== Xlayer Flashblocks Raw Stream Probe ===");
    println!("raw_ws: {raw_ws}");
    println!("run_secs: {run_secs}, max_messages: {:?}", max_messages);
    println!("dump_first: {dump_first}");

    let (mut ws_stream, _) = connect_async(raw_ws.clone())
        .await
        .with_context(|| format!("failed to connect to {raw_ws}"))?;

    println!("[probe] connected to Xlayer flashblocks WS");

    let started = Instant::now();
    let deadline = started + Duration::from_secs(run_secs);

    let mut raw_messages = 0usize;
    let mut parse_success = 0usize;
    let mut parse_fail = 0usize;
    let mut messages_without_base = 0usize;
    let mut messages_without_diff = 0usize;
    let mut messages_without_metadata = 0usize;
    let mut messages_with_receipts = 0usize;
    let mut total_receipts = 0usize;
    let mut total_logs_in_receipts = 0usize;
    let mut has_any_logs = false;
    let mut has_logs_bloom = false;

    let mut prev_msg_time: Option<Instant> = None;
    let mut msg_intervals_ms: Vec<u128> = Vec::new();
    let mut msg_sizes: Vec<usize> = Vec::new();

    loop {
        if Instant::now() >= deadline {
            println!("[probe] deadline reached");
            break;
        }

        if let Some(limit) = max_messages {
            if raw_messages >= limit {
                println!("[probe] max_messages ({limit}) reached");
                break;
            }
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
                parse_fail += 1;
                eprintln!("[probe][WARN] ws receive error: {e}");
                continue;
            }
        };

        raw_messages += 1;

        let now = Instant::now();
        if let Some(prev) = prev_msg_time {
            msg_intervals_ms.push(now.duration_since(prev).as_millis());
        }
        prev_msg_time = Some(now);

        let (payload, msg_type) = match message {
            Message::Text(text) => {
                msg_sizes.push(text.len());
                (text.as_bytes().to_vec(), "text")
            }
            Message::Binary(bin) => {
                msg_sizes.push(bin.len());
                (bin.to_vec(), "binary")
            }
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

        // 尝试直接 JSON 解析，不尝试 Brotli（Xlayer 文档未提及 Brotli）
        let fb: ProbeMessage = match serde_json::from_slice(&payload) {
            Ok(v) => v,
            Err(e) => {
                // 如果 JSON 解析失败，尝试 Brotli 解压
                let mut decompressed = Vec::new();
                let mut reader = brotli::Decompressor::new(payload.as_slice(), 4096);
                if reader.read_to_end(&mut decompressed).is_err() {
                    parse_fail += 1;
                    if raw_messages <= dump_first {
                        println!("[probe] msg#{raw_messages} ({msg_type}, {} bytes): JSON parse fail + Brotli fail: {e}", payload.len());
                    }
                    continue;
                }
                match serde_json::from_slice::<ProbeMessage>(&decompressed) {
                    Ok(v) => {
                        // 记录使用了 Brotli
                        if raw_messages <= dump_first {
                            println!("[probe] msg#{raw_messages} ({msg_type}, {} bytes): parsed via Brotli decompress ({} -> {} bytes)", payload.len(), payload.len(), decompressed.len());
                        }
                        v
                    }
                    Err(e2) => {
                        parse_fail += 1;
                        if raw_messages <= dump_first {
                            println!("[probe] msg#{raw_messages} ({msg_type}, {} bytes): JSON parse fail after Brotli: {e2}", payload.len());
                            // 打印原始内容前 200 字节帮助调试
                            let preview = String::from_utf8_lossy(&decompressed[..decompressed.len().min(200)]);
                            println!("  decompressed preview: {preview}");
                        }
                        continue;
                    }
                }
            }
        };

        parse_success += 1;

        // 收集统计
        if fb.base.is_none() {
            messages_without_base += 1;
        }
        if fb.diff.is_none() {
            messages_without_diff += 1;
        }

        // 检查 logs_bloom
        if let Some(diff) = &fb.diff {
            if let Some(obj) = diff.as_object() {
                if obj.contains_key("logs_bloom") {
                    has_logs_bloom = true;
                }
            }
        }

        // 检查 metadata.receipts 中是否包含 logs
        if let Some(meta) = &fb.metadata {
            if let Some(obj) = meta.as_object() {
                if let Some(receipts_val) = obj.get("receipts") {
                    if let Some(rcpts) = receipts_val.as_object() {
                        if !rcpts.is_empty() {
                            messages_with_receipts += 1;
                            total_receipts += rcpts.len();

                            // 检查每个收据是否有 logs 字段
                            for (_tx_hash, receipt) in rcpts.iter() {
                                if let Some(rcpt_obj) = receipt.as_object() {
                                    if let Some(logs_val) = rcpt_obj.get("logs") {
                                        if let Some(logs_arr) = logs_val.as_array() {
                                            total_logs_in_receipts += logs_arr.len();
                                            if logs_arr.len() > 0 {
                                                has_any_logs = true;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        } else {
            messages_without_metadata += 1;
        }

        // 打印前 N 条消息的完整结构
        if raw_messages <= dump_first {
            let raw_preview = String::from_utf8_lossy(&payload[..payload.len().min(150)]);
            let raw_preview = if payload.len() > 150 {
                format!("{}... ({} bytes)", raw_preview, payload.len())
            } else {
                raw_preview.to_string()
            };
            print_full_message(&fb, &raw_preview);
        } else if raw_messages == dump_first + 1 {
            println!("\n[probe] ... suppressing detailed dumps, showing summary stats only ...\n");
        }

        // 每 10 条消息打印一次实时统计
        if raw_messages % 10 == 0 {
            println!(
                "[progress] msgs={} success={} fail={} receipts={} total_logs_in_receipts={} elapsed={}s",
                raw_messages,
                parse_success,
                parse_fail,
                total_receipts,
                total_logs_in_receipts,
                started.elapsed().as_secs(),
            );
        }
    }

    let elapsed = started.elapsed();
    let elapsed_s = elapsed.as_secs_f64().max(1e-9);

    // 计算百分位
    msg_intervals_ms.sort_unstable();
    msg_sizes.sort_unstable();
    let p50_iv = msg_intervals_ms.get((msg_intervals_ms.len() as f64 * 0.5) as usize).copied().unwrap_or(0);
    let p95_iv = msg_intervals_ms.get((msg_intervals_ms.len() as f64 * 0.95) as usize).copied().unwrap_or(0);
    let p50_sz = msg_sizes.get((msg_sizes.len() as f64 * 0.5) as usize).copied().unwrap_or(0);
    let p95_sz = msg_sizes.get((msg_sizes.len() as f64 * 0.95) as usize).copied().unwrap_or(0);

    println!("\n\n==============================================");
    println!("=== Xlayer Flashblocks Probe Final Report ===");
    println!("==============================================");
    println!("elapsed:            {}s", elapsed.as_secs_f64());
    println!("raw_messages:       {}", raw_messages);
    println!("parse_success:      {}", parse_success);
    println!("parse_fail:         {}", parse_fail);
    println!("messages_per_sec:   {:.2}", parse_success as f64 / elapsed_s);
    println!("msg_interval_p50:   {}ms", p50_iv);
    println!("msg_interval_p95:   {}ms", p95_iv);
    println!("msg_size_p50:       {} bytes", p50_sz);
    println!("msg_size_p95:       {} bytes", p95_sz);
    println!();
    println!("messages_without_base:     {}", messages_without_base);
    println!("messages_without_diff:     {}", messages_without_diff);
    println!("messages_without_metadata: {}", messages_without_metadata);
    println!("messages_with_receipts:    {}", messages_with_receipts);
    println!();
    println!("total_receipts:          {}", total_receipts);
    println!("total_logs_in_receipts:  {}", total_logs_in_receipts);
    println!("has_any_logs_in_receipt: {}", has_any_logs);
    println!("has_logs_bloom_in_diff:  {}", has_logs_bloom);
    println!();

    // 最终结论
    println!("=== CONCLUSION ===");
    if has_any_logs {
        println!(
            "✅ Xlayer flashblocks receipts CONTAIN full log objects (address, topics, data). \
             We can extract logs directly, similar to Base chain."
        );
    } else if has_logs_bloom {
        println!(
            "⚠️  Xlayer flashblocks does NOT contain full logs in receipts. \
             Instead, it provides diff.logs_bloom (Bloom filter). \
             We will need to: flashblocks WS -> bloom pre-filter -> get_logs RPC (like NewHeadsPull)."
        );
    } else if total_receipts > 0 {
        println!(
            "❓ Xlayer flashblocks has receipts but without embedded logs. \
             Check the receipt structure above for where log data might be located."
        );
    } else {
        println!(
            "❓ Xlayer flashblocks messages have no receipts and no logs_bloom detected. \
             Need further investigation of the message format."
        );
    }
    println!("==============================================");

    Ok(())
}

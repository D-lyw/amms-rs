use eyre::Context;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Write};
use std::time::{Duration, Instant};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::Message;

fn sorted_object_keys(value: Option<&Value>) -> Vec<String> {
    match value.and_then(Value::as_object) {
        Some(map) => {
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort();
            keys
        }
        None => Vec::new(),
    }
}

fn joined_keys(keys: &[String]) -> String {
    if keys.is_empty() {
        "<none>".to_string()
    } else {
        keys.join(",")
    }
}

fn increment(map: &mut BTreeMap<String, usize>, key: String) {
    *map.entry(key).or_insert(0) += 1;
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let raw_ws = std::env::var("RAW_FLASHBLOCKS_WS")
        .unwrap_or_else(|_| "wss://mainnet.flashblocks.base.org/ws".to_string());
    let run_secs: u64 = std::env::var("RUN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15);
    let sample_limit: usize = std::env::var("SAMPLE_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let out_path = std::env::var("OUT_PATH").ok();

    println!("=== Base Flashblocks Schema Probe ===");
    println!("raw_ws: {raw_ws}");
    println!("run_secs: {run_secs}");
    println!("sample_limit: {sample_limit}");
    if let Some(path) = &out_path {
        println!("out_path: {path}");
    }

    let (mut ws_stream, _) = connect_async(raw_ws.clone())
        .await
        .with_context(|| format!("failed to connect raw ws: {raw_ws}"))?;

    let mut out_file = match out_path {
        Some(path) => {
            Some(File::create(&path).with_context(|| format!("failed to create {path}"))?)
        }
        None => None,
    };

    let started = Instant::now();
    let deadline = started + Duration::from_secs(run_secs);

    let mut raw_messages = 0usize;
    let mut parsed_messages = 0usize;
    let mut text_messages = 0usize;
    let mut binary_messages = 0usize;
    let mut decoded_brotli_messages = 0usize;
    let mut decode_fail_messages = 0usize;
    let mut messages_without_metadata = 0usize;
    let mut messages_without_receipts = 0usize;
    let mut sampled_messages = 0usize;

    let mut top_key_shapes: BTreeMap<String, usize> = BTreeMap::new();
    let mut metadata_key_shapes: BTreeMap<String, usize> = BTreeMap::new();
    let mut base_key_shapes: BTreeMap<String, usize> = BTreeMap::new();
    let mut diff_key_shapes: BTreeMap<String, usize> = BTreeMap::new();
    let mut first_receipt_key_shapes: BTreeMap<String, usize> = BTreeMap::new();
    let mut unique_topics0: BTreeSet<String> = BTreeSet::new();

    loop {
        if Instant::now() >= deadline {
            break;
        }

        let next = tokio::time::timeout(Duration::from_secs(3), ws_stream.next()).await;
        let maybe_message = match next {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(message_result) = maybe_message else {
            break;
        };

        let message = match message_result {
            Ok(m) => m,
            Err(e) => {
                decode_fail_messages += 1;
                eprintln!("[schema][WARN] ws receive error: {e}");
                continue;
            }
        };

        raw_messages += 1;

        let payload = match message {
            Message::Text(text) => {
                text_messages += 1;
                text.as_bytes().to_vec()
            }
            Message::Binary(bin) => {
                binary_messages += 1;
                bin.to_vec()
            }
            Message::Ping(v) => {
                let _ = ws_stream.send(Message::Pong(v)).await;
                continue;
            }
            Message::Pong(_) => continue,
            Message::Close(_) => break,
            Message::Frame(_) => continue,
        };

        let parsed: Value = match serde_json::from_slice::<Value>(&payload) {
            Ok(v) => v,
            Err(_) => {
                let mut decompressed = Vec::new();
                let mut reader = brotli::Decompressor::new(payload.as_slice(), 4096);
                if reader.read_to_end(&mut decompressed).is_err() {
                    decode_fail_messages += 1;
                    continue;
                }
                match serde_json::from_slice::<Value>(&decompressed) {
                    Ok(v) => {
                        decoded_brotli_messages += 1;
                        v
                    }
                    Err(_) => {
                        decode_fail_messages += 1;
                        continue;
                    }
                }
            }
        };

        parsed_messages += 1;

        let top_keys = sorted_object_keys(Some(&parsed));
        increment(&mut top_key_shapes, joined_keys(&top_keys));

        let metadata = parsed.get("metadata");
        let metadata_keys = sorted_object_keys(metadata);
        increment(&mut metadata_key_shapes, joined_keys(&metadata_keys));
        if metadata.is_none() || metadata_keys.is_empty() {
            messages_without_metadata += 1;
        }

        let base_keys = sorted_object_keys(parsed.get("base"));
        increment(&mut base_key_shapes, joined_keys(&base_keys));

        let diff_keys = sorted_object_keys(parsed.get("diff"));
        increment(&mut diff_key_shapes, joined_keys(&diff_keys));

        let receipt_count = metadata
            .and_then(|m| m.get("receipts"))
            .and_then(Value::as_object)
            .map(|m| m.len())
            .unwrap_or(0);
        if receipt_count == 0 {
            messages_without_receipts += 1;
        }

        let first_receipt_keys = metadata
            .and_then(|m| m.get("receipts"))
            .and_then(Value::as_object)
            .and_then(|receipts| receipts.values().next())
            .map(|receipt| sorted_object_keys(Some(receipt)))
            .unwrap_or_default();
        increment(
            &mut first_receipt_key_shapes,
            joined_keys(&first_receipt_keys),
        );

        if let Some(topics) = metadata
            .and_then(|m| m.get("receipts"))
            .and_then(Value::as_object)
            .and_then(|receipts| receipts.values().next())
            .and_then(|receipt| receipt.get("logs"))
            .and_then(Value::as_array)
            .and_then(|logs| logs.first())
            .and_then(|log| log.get("topics"))
            .and_then(Value::as_array)
        {
            if let Some(topic0) = topics.first().and_then(Value::as_str) {
                unique_topics0.insert(topic0.to_string());
            }
        }

        if sampled_messages < sample_limit {
            sampled_messages += 1;
            let payload_id = parsed
                .get("payload_id")
                .and_then(Value::as_str)
                .unwrap_or("<none>");
            let index = parsed
                .get("index")
                .and_then(Value::as_u64)
                .map(|v| v.to_string())
                .unwrap_or_else(|| "<none>".to_string());
            let block_number = metadata
                .and_then(|m| m.get("block_number"))
                .and_then(Value::as_u64)
                .map(|v| v.to_string())
                .or_else(|| {
                    parsed
                        .get("base")
                        .and_then(|b| b.get("block_number"))
                        .and_then(Value::as_str)
                        .map(|v| v.to_string())
                })
                .unwrap_or_else(|| "<none>".to_string());

            println!(
                "[sample #{sampled_messages}] payload_id={payload_id} index={index} block_number={block_number} receipts={receipt_count}"
            );
            println!("  top_keys: {}", joined_keys(&top_keys));
            println!("  metadata_keys: {}", joined_keys(&metadata_keys));
            println!("  base_keys: {}", joined_keys(&base_keys));
            println!("  diff_keys: {}", joined_keys(&diff_keys));
            println!("  first_receipt_keys: {}", joined_keys(&first_receipt_keys));

            if let Some(file) = out_file.as_mut() {
                serde_json::to_writer_pretty(&mut *file, &parsed)?;
                file.write_all(b"\n")?;
                file.write_all(b"---\n")?;
            }
        }
    }

    println!("\n=== Summary ===");
    println!("elapsed_ms: {}", started.elapsed().as_millis());
    println!("raw_messages: {}", raw_messages);
    println!("parsed_messages: {}", parsed_messages);
    println!("text_messages: {}", text_messages);
    println!("binary_messages: {}", binary_messages);
    println!("decoded_brotli_messages: {}", decoded_brotli_messages);
    println!("decode_fail_messages: {}", decode_fail_messages);
    println!("messages_without_metadata: {}", messages_without_metadata);
    println!("messages_without_receipts: {}", messages_without_receipts);

    println!("\nTop-level key shapes:");
    for (shape, count) in top_key_shapes {
        println!("  {count:>4}  {shape}");
    }

    println!("\nMetadata key shapes:");
    for (shape, count) in metadata_key_shapes {
        println!("  {count:>4}  {shape}");
    }

    println!("\nBase key shapes:");
    for (shape, count) in base_key_shapes {
        println!("  {count:>4}  {shape}");
    }

    println!("\nDiff key shapes:");
    for (shape, count) in diff_key_shapes {
        println!("  {count:>4}  {shape}");
    }

    println!("\nFirst receipt key shapes:");
    for (shape, count) in first_receipt_key_shapes {
        println!("  {count:>4}  {shape}");
    }

    println!("\nObserved topic0 samples:");
    for topic0 in unique_topics0.into_iter().take(10) {
        println!("  {topic0}");
    }

    Ok(())
}

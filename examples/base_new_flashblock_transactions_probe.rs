#[path = "common/base_preconf_probe_support.rs"]
mod support;

use alloy::{
    network::Ethereum,
    providers::{Provider, ProviderBuilder, WsConnect},
    rpc::types::eth::Log,
};
use amms::state_space::StateSpaceBuilder;
use eyre::Context;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};
use support::{
    apply_followups, build_local_log_matcher, joined_keys, load_amms_from_graph, percentile,
    resolve_graph_path, sorted_object_keys,
};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

fn extract_logs_from_tx_result(result: &Value) -> Vec<Value> {
    if let Some(arr) = result.get("logs").and_then(Value::as_array) {
        return arr.clone();
    }
    if let Some(arr) = result
        .get("receipt")
        .and_then(|v| v.get("logs"))
        .and_then(Value::as_array)
    {
        return arr.clone();
    }
    Vec::new()
}

fn dedup_key(log: &Log) -> Option<(String, u64, u64, String)> {
    let tx_hash = log.transaction_hash.map(|h| format!("{h:?}"))?;
    let log_index = log.log_index?;
    let block_number = log.block_number?;
    Some((
        tx_hash,
        log_index,
        block_number,
        format!("{:?}", log.address()),
    ))
}

fn normalize_log_value(raw_log: &Value, tx_hash: Option<&str>) -> Value {
    let mut value = raw_log.clone();
    if let Some(map) = value.as_object_mut() {
        if map.get("transactionHash").is_none() {
            if let Some(tx_hash) = tx_hash {
                map.insert(
                    "transactionHash".to_string(),
                    Value::String(tx_hash.to_string()),
                );
            }
        }
        if map.get("removed").is_none() {
            map.insert("removed".to_string(), Value::Bool(false));
        }
    }
    value
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let preconf_ws = std::env::var("BASE_PRECONF_WS")
        .or_else(|_| std::env::var("PRECONF_WS"))
        .ok();
    let preconf_ws_candidates = preconf_ws.map_or_else(
        || vec!["wss://mainnet-preconf.base.org".to_string()],
        |url| vec![url],
    );
    let rpc_ws = std::env::var("BASE_RPC_WS")
        .or_else(|_| std::env::var("BASE_WS"))
        .unwrap_or_else(|_| {
            "wss://base-mainnet.core.chainstack.com/fc5f8eef2b27bee75a83ca6ab5a02634".to_string()
        });
    let graph_path = resolve_graph_path();
    let pool_limit = std::env::var("POOL_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok());
    let run_secs: u64 = std::env::var("RUN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let max_messages: Option<usize> = std::env::var("MAX_MESSAGES")
        .ok()
        .and_then(|v| v.parse().ok());
    let sample_limit: usize = std::env::var("SAMPLE_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);

    println!("=== Base newFlashblockTransactions(full=true) Probe ===");
    println!(
        "preconf_ws_candidates: {}",
        preconf_ws_candidates.join(", ")
    );
    println!("rpc_ws: {rpc_ws}");
    println!("graph_path: {graph_path}");
    println!("run_secs: {run_secs}, max_messages: {:?}", max_messages);
    println!("sample_limit: {sample_limit}");

    let provider = Arc::new(
        ProviderBuilder::new()
            .connect_ws(WsConnect::new(rpc_ws.clone()))
            .await
            .with_context(|| format!("failed to connect rpc ws: {rpc_ws}"))?,
    );

    let chain_id = provider.get_chain_id().await?;
    println!("connected chain_id={chain_id}");

    let amms = load_amms_from_graph(&graph_path, pool_limit)?;
    if amms.is_empty() {
        return Err(eyre::eyre!("no AMMs loaded from graph"));
    }

    let manager = StateSpaceBuilder::new(provider.clone())
        .with_amms(amms.clone())
        .sync()
        .await
        .context("initial sync failed")?;

    let matcher = build_local_log_matcher(&provider, &amms, chain_id).await;
    println!(
        "[probe] local matcher: topic_addresses={} topic_signatures={} address_only_addresses={}",
        matcher.topic_addresses.len(),
        matcher.topic_signatures.len(),
        matcher.address_only_addresses.len()
    );

    let mut preconf_errors = Vec::new();
    let mut connected = None;
    for candidate in &preconf_ws_candidates {
        match connect_async(candidate.clone()).await {
            Ok((stream, response)) => {
                connected = Some((stream, response, candidate.clone()));
                break;
            }
            Err(e) => {
                eprintln!("[probe][WARN] preconf ws connect failed: {candidate}: {e}");
                preconf_errors.push(format!("{candidate}: {e}"));
            }
        }
    }
    let (mut ws_stream, _, chosen_preconf_ws) = connected.ok_or_else(|| {
        eyre::eyre!(
            "failed to connect any preconf ws candidate: {}",
            if preconf_errors.is_empty() {
                "<unknown>".to_string()
            } else {
                preconf_errors.join(" | ")
            }
        )
    })?;
    println!("[probe] connected preconf ws: {chosen_preconf_ws}");

    ws_stream
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_subscribe",
                "params": ["newFlashblockTransactions", true],
            })
            .to_string()
            .into(),
        ))
        .await?;

    let subscribe_reply = tokio::time::timeout(Duration::from_secs(10), ws_stream.next()).await?;
    let Some(reply) = subscribe_reply else {
        return Err(eyre::eyre!(
            "newFlashblockTransactions subscribe stream ended immediately"
        ));
    };
    let reply = reply?;
    let reply_text = match reply {
        Message::Text(text) => text.to_string(),
        Message::Binary(bin) => String::from_utf8(bin.to_vec())
            .context("newFlashblockTransactions subscribe reply was non-utf8 binary")?,
        other => return Err(eyre::eyre!("unexpected subscribe reply: {other:?}")),
    };
    let reply_json: Value = serde_json::from_str(&reply_text)?;
    let subscription_id = reply_json
        .get("result")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre::eyre!("newFlashblockTransactions subscribe failed: {reply_json}"))?
        .to_string();
    println!("[probe] subscribed newFlashblockTransactions id={subscription_id}");

    let started = Instant::now();
    let deadline = started + Duration::from_secs(run_secs);

    let mut ws_messages = 0usize;
    let mut notifications = 0usize;
    let mut sampled = 0usize;
    let mut decode_fail = 0usize;
    let mut tx_without_logs = 0usize;
    let mut tx_with_logs = 0usize;
    let mut candidate_logs_total = 0usize;
    let mut matched_logs_total = 0usize;
    let mut dedup_dropped_logs = 0usize;
    let mut sync_batches_total = 0usize;
    let mut trigger_batches = 0usize;
    let mut affected_pools_total = 0usize;
    let mut total_resync = 0usize;
    let mut total_async_update = 0usize;
    let mut msg_intervals_ms = Vec::new();
    let mut batch_sizes = Vec::new();
    let mut affected_sizes = Vec::new();
    let mut prev_msg_time: Option<Instant> = None;
    let mut seen_logs = HashSet::new();

    loop {
        if Instant::now() >= deadline {
            break;
        }
        if let Some(limit) = max_messages {
            if ws_messages >= limit {
                break;
            }
        }

        let next = tokio::time::timeout(Duration::from_secs(3), ws_stream.next()).await;
        let Some(message_result) = (match next {
            Ok(v) => v,
            Err(_) => continue,
        }) else {
            break;
        };
        let message = match message_result {
            Ok(v) => v,
            Err(e) => {
                decode_fail += 1;
                eprintln!("[probe][WARN] ws receive error: {e}");
                continue;
            }
        };

        ws_messages += 1;
        let now = Instant::now();
        if let Some(prev) = prev_msg_time {
            msg_intervals_ms.push(now.duration_since(prev).as_millis());
        }
        prev_msg_time = Some(now);

        let payload = match message {
            Message::Text(text) => text.to_string(),
            Message::Binary(bin) => String::from_utf8(bin.to_vec())
                .context("newFlashblockTransactions notification was non-utf8 binary")?,
            Message::Ping(v) => {
                let _ = ws_stream.send(Message::Pong(v)).await;
                continue;
            }
            Message::Pong(_) => continue,
            Message::Close(_) => break,
            Message::Frame(_) => continue,
        };

        let root: Value = match serde_json::from_str(&payload) {
            Ok(v) => v,
            Err(e) => {
                decode_fail += 1;
                eprintln!("[probe][WARN] json decode failed: {e}");
                continue;
            }
        };

        if root.get("method").and_then(Value::as_str) != Some("eth_subscription") {
            continue;
        }
        if root
            .get("params")
            .and_then(|v| v.get("subscription"))
            .and_then(Value::as_str)
            != Some(subscription_id.as_str())
        {
            continue;
        }

        let result = root
            .get("params")
            .and_then(|v| v.get("result"))
            .cloned()
            .unwrap_or(Value::Null);
        notifications += 1;

        if sampled < sample_limit {
            sampled += 1;
            let result_keys = sorted_object_keys(Some(&result));
            let first_log_keys = extract_logs_from_tx_result(&result)
                .first()
                .map(|v| sorted_object_keys(Some(v)))
                .unwrap_or_default();
            println!(
                "[sample #{sampled}] tx_keys={} hash={} logs={} first_log_keys={}",
                joined_keys(&result_keys),
                result
                    .get("hash")
                    .or_else(|| result.get("transactionHash"))
                    .and_then(Value::as_str)
                    .unwrap_or("<none>"),
                extract_logs_from_tx_result(&result).len(),
                joined_keys(&first_log_keys),
            );
        }

        let tx_hash = result
            .get("hash")
            .or_else(|| result.get("transactionHash"))
            .and_then(Value::as_str);
        let raw_logs = extract_logs_from_tx_result(&result);
        if raw_logs.is_empty() {
            tx_without_logs += 1;
            continue;
        }

        tx_with_logs += 1;
        candidate_logs_total += raw_logs.len();

        let mut logs = Vec::new();
        for raw_log in raw_logs {
            match serde_json::from_value::<Log>(normalize_log_value(&raw_log, tx_hash)) {
                Ok(log) => {
                    if !matcher.matches(&log) {
                        continue;
                    }
                    if let Some(key) = dedup_key(&log) {
                        if !seen_logs.insert(key) {
                            dedup_dropped_logs += 1;
                            continue;
                        }
                    }
                    logs.push(log);
                }
                Err(e) => {
                    decode_fail += 1;
                    eprintln!("[probe][WARN] log decode failed: {e}");
                }
            }
        }

        matched_logs_total += logs.len();
        if logs.is_empty() {
            continue;
        }

        sync_batches_total += 1;
        batch_sizes.push(logs.len() as u128);

        let max_block = logs
            .iter()
            .filter_map(|l| l.block_number)
            .max()
            .unwrap_or(0);
        let (affected, needs_resync, needs_async_update) = {
            let mut guard = manager.state.write().await;
            guard.sync(&logs)?
        };

        let affected_len = affected.len();
        if affected_len > 0 {
            trigger_batches += 1;
        }
        affected_pools_total += affected_len;
        affected_sizes.push(affected_len as u128);

        let (resynced, async_updated) = apply_followups::<Ethereum, _>(
            &manager.state,
            provider.clone(),
            max_block,
            needs_resync,
            needs_async_update,
        )
        .await;
        total_resync += resynced;
        total_async_update += async_updated;
    }

    let elapsed = started.elapsed();
    let elapsed_s = elapsed.as_secs_f64().max(1e-9);
    let mut iv50 = msg_intervals_ms.clone();
    let mut iv95 = msg_intervals_ms.clone();
    let mut bs50 = batch_sizes.clone();
    let mut bs95 = batch_sizes.clone();
    let mut af50 = affected_sizes.clone();
    let mut af95 = affected_sizes.clone();

    println!("\n=== Summary ===");
    println!("elapsed_ms: {}", elapsed.as_millis());
    println!("ws_messages: {}", ws_messages);
    println!("notifications: {}", notifications);
    println!(
        "notifications_per_sec: {:.2}",
        notifications as f64 / elapsed_s
    );
    println!("tx_without_logs: {}", tx_without_logs);
    println!("tx_with_logs: {}", tx_with_logs);
    println!("candidate_logs_total: {}", candidate_logs_total);
    println!("matched_logs_total: {}", matched_logs_total);
    println!("dedup_dropped_logs: {}", dedup_dropped_logs);
    println!("decode_fail: {}", decode_fail);
    println!("sync_batches_total: {}", sync_batches_total);
    println!("trigger_batches(affected>0): {}", trigger_batches);
    println!("affected_pools_total: {}", affected_pools_total);
    println!("total_resync: {}", total_resync);
    println!("total_async_update: {}", total_async_update);
    println!(
        "msg_interval_p50_ms: {}",
        percentile(&mut iv50, 0.5).unwrap_or(0)
    );
    println!(
        "msg_interval_p95_ms: {}",
        percentile(&mut iv95, 0.95).unwrap_or(0)
    );
    println!(
        "batch_size_p50: {}",
        percentile(&mut bs50, 0.5).unwrap_or(0)
    );
    println!(
        "batch_size_p95: {}",
        percentile(&mut bs95, 0.95).unwrap_or(0)
    );
    println!(
        "affected_size_p50: {}",
        percentile(&mut af50, 0.5).unwrap_or(0)
    );
    println!(
        "affected_size_p95: {}",
        percentile(&mut af95, 0.95).unwrap_or(0)
    );
    println!("\n=== Assessment ===");
    println!("newFlashblockTransactions(full=true) can carry enough data to reconstruct logs, but it is transaction-shaped rather than log-shaped.");
    println!("That means more payload bytes, more decode work, and usually more local filtering before reaching amms::StateSpace::sync(logs).");
    println!("It is useful for schema inspection and tx-level debugging, but pendingLogs should usually be the cleaner production fit if the goal is just affected-pool log sync.");

    Ok(())
}

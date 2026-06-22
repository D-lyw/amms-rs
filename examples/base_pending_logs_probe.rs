#[path = "common/base_preconf_probe_support.rs"]
mod support;

use alloy::{
    network::Ethereum,
    primitives::{Address, FixedBytes},
    providers::{Provider, ProviderBuilder, WsConnect},
    rpc::types::eth::Log,
    sol_types::SolEvent,
};
use amms::amms::{
    aerodrome_slipstream::{ICLPoolEvents, ICustomFeeModule},
    aerodrome_v2::IAerodromeV2Pool,
    amm::{AutomatedMarketMaker, AMM},
};
use amms::state_space::StateSpaceBuilder;
use eyre::Context;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};
use support::{
    apply_followups, build_local_log_matcher, joined_keys, load_amms_from_graph, parse_bool_env,
    percentile, resolve_graph_path, sorted_object_keys,
};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

fn collect_log_values(result: &Value) -> Vec<Value> {
    if let Some(arr) = result.as_array() {
        return arr.clone();
    }
    if result.is_object() {
        return vec![result.clone()];
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

fn protocol_label(amm: &AMM) -> Option<&'static str> {
    match amm {
        AMM::AerodromeV2Pool(_) => Some("AerodromeV2"),
        AMM::AerodromeSlipstreamPool(_) => Some("AerodromeSlipstream"),
        _ => None,
    }
}

fn build_protocol_by_address(amms: &[AMM]) -> HashMap<Address, &'static str> {
    let mut map = HashMap::new();
    for amm in amms {
        if let Some(label) = protocol_label(amm) {
            map.insert(amm.address(), label);
        }
    }
    map
}

fn event_label(topic0: Option<FixedBytes<32>>) -> &'static str {
    match topic0 {
        Some(sig) if sig == IAerodromeV2Pool::Sync::SIGNATURE_HASH => "AerodromeV2::Sync",
        Some(sig) if sig == ICLPoolEvents::Mint::SIGNATURE_HASH => "Slipstream::Mint",
        Some(sig) if sig == ICLPoolEvents::Burn::SIGNATURE_HASH => "Slipstream::Burn",
        Some(sig) if sig == ICLPoolEvents::Swap::SIGNATURE_HASH => "Slipstream::Swap",
        Some(sig) if sig == ICustomFeeModule::CustomFeeSet::SIGNATURE_HASH => {
            "Slipstream::CustomFeeSet"
        }
        _ => "Unknown",
    }
}

#[derive(Default, Debug)]
struct TxDiagnostic {
    block_number: Option<u64>,
    log_count: usize,
    with_log_index: usize,
    non_monotonic_steps: usize,
    last_log_index: Option<u64>,
    addresses: HashSet<String>,
    protocol_counts: HashMap<&'static str, usize>,
    per_address_counts: HashMap<String, usize>,
    sequence: Vec<String>,
}

impl TxDiagnostic {
    fn record(&mut self, log: &Log, protocol: &'static str) {
        self.block_number = self.block_number.or(log.block_number);
        self.log_count += 1;
        if log.log_index.is_some() {
            self.with_log_index += 1;
        }
        if let Some(log_index) = log.log_index {
            if let Some(prev) = self.last_log_index {
                if log_index < prev {
                    self.non_monotonic_steps += 1;
                }
            }
            self.last_log_index = Some(log_index);
        }

        let address = format!("{:?}", log.address());
        self.addresses.insert(address.clone());
        *self.protocol_counts.entry(protocol).or_insert(0) += 1;
        *self.per_address_counts.entry(address.clone()).or_insert(0) += 1;

        if self.sequence.len() < 20 {
            let topic0 = log.topics().first().copied();
            let item = format!(
                "{} {} {} {}",
                log.log_index
                    .map(|v| format!("log_index={v:#x}"))
                    .unwrap_or_else(|| "log_index=<none>".to_string()),
                protocol,
                event_label(topic0),
                address
            );
            self.sequence.push(item);
        }
    }
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
    let tx_sample_limit: usize = std::env::var("TX_SAMPLE_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let use_topics_filter = parse_bool_env("USE_TOPICS_FILTER");
    let skip_initial_sync = parse_bool_env("SKIP_INITIAL_SYNC");

    println!("=== Base pendingLogs Probe ===");
    println!(
        "preconf_ws_candidates: {}",
        preconf_ws_candidates.join(", ")
    );
    println!("rpc_ws: {rpc_ws}");
    println!("graph_path: {graph_path}");
    println!("run_secs: {run_secs}, max_messages: {:?}", max_messages);
    println!("sample_limit: {sample_limit}");
    println!("tx_sample_limit: {tx_sample_limit}");
    println!("use_topics_filter: {use_topics_filter}");
    println!("skip_initial_sync: {skip_initial_sync}");

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
    let manager = if skip_initial_sync {
        None
    } else {
        Some(
            StateSpaceBuilder::new(provider.clone())
                .with_amms(amms.clone())
                .sync()
                .await
                .context("initial sync failed")?,
        )
    };

    let matcher = build_local_log_matcher(&provider, &amms, chain_id).await;
    let protocol_by_address = build_protocol_by_address(&amms);
    let mut address_filter: Vec<String> = matcher
        .topic_addresses
        .iter()
        .chain(matcher.address_only_addresses.iter())
        .map(|addr| format!("{addr:?}"))
        .collect();
    address_filter.sort();
    address_filter.dedup();

    let topics_filter: Vec<String> = matcher
        .topic_signatures
        .iter()
        .map(|topic| format!("{topic:?}"))
        .collect();

    println!(
        "[probe] local matcher: topic_addresses={} topic_signatures={} address_only_addresses={}",
        matcher.topic_addresses.len(),
        matcher.topic_signatures.len(),
        matcher.address_only_addresses.len()
    );
    println!(
        "[probe] subscription filter: addresses={} topic0_candidates={}",
        address_filter.len(),
        topics_filter.len()
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

    let filter = if use_topics_filter {
        json!({
            "address": address_filter,
            "topics": [topics_filter],
        })
    } else {
        json!({
            "address": address_filter,
        })
    };

    ws_stream
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_subscribe",
                "params": ["pendingLogs", filter],
            })
            .to_string()
            .into(),
        ))
        .await?;

    let subscribe_reply = tokio::time::timeout(Duration::from_secs(10), ws_stream.next()).await?;
    let Some(reply) = subscribe_reply else {
        return Err(eyre::eyre!(
            "pendingLogs subscribe stream ended immediately"
        ));
    };
    let reply = reply?;
    let reply_text = match reply {
        Message::Text(text) => text.to_string(),
        Message::Binary(bin) => String::from_utf8(bin.to_vec())
            .context("pendingLogs subscribe reply was non-utf8 binary")?,
        other => return Err(eyre::eyre!("unexpected subscribe reply: {other:?}")),
    };
    let reply_json: Value = serde_json::from_str(&reply_text)?;
    let subscription_id = reply_json
        .get("result")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre::eyre!("pendingLogs subscribe failed: {reply_json}"))?
        .to_string();
    println!("[probe] subscribed pendingLogs id={subscription_id}");

    let started = Instant::now();
    let deadline = started + Duration::from_secs(run_secs);

    let mut ws_messages = 0usize;
    let mut notifications = 0usize;
    let mut sampled = 0usize;
    let mut decode_fail = 0usize;
    let mut result_object_notifications = 0usize;
    let mut result_array_notifications = 0usize;
    let mut result_other_notifications = 0usize;
    let mut candidate_logs_total = 0usize;
    let mut matched_logs_total = 0usize;
    let mut dedup_dropped_logs = 0usize;
    let mut logs_with_log_index = 0usize;
    let mut logs_without_log_index = 0usize;
    let mut matched_logs_by_protocol: HashMap<&'static str, usize> = HashMap::new();
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
    let mut tx_diagnostics: HashMap<String, TxDiagnostic> = HashMap::new();

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
                .context("pendingLogs notification was non-utf8 binary")?,
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
        if result.is_object() {
            result_object_notifications += 1;
        } else if result.is_array() {
            result_array_notifications += 1;
        } else {
            result_other_notifications += 1;
        }

        if sampled < sample_limit {
            sampled += 1;
            let result_keys = sorted_object_keys(Some(&result));
            println!(
                "[sample #{sampled}] result_kind={} result_keys={} address={} topic_count={} block={} tx={} log_index={}",
                if result.is_object() {
                    "object"
                } else if result.is_array() {
                    "array"
                } else {
                    "other"
                },
                joined_keys(&result_keys),
                result
                    .get("address")
                    .and_then(Value::as_str)
                    .unwrap_or("<none>"),
                result
                    .get("topics")
                    .and_then(Value::as_array)
                    .map(|v| v.len())
                    .unwrap_or(0),
                result
                    .get("blockNumber")
                    .and_then(Value::as_str)
                    .unwrap_or("<none>"),
                result
                    .get("transactionHash")
                    .and_then(Value::as_str)
                    .unwrap_or("<none>"),
                result
                    .get("logIndex")
                    .and_then(Value::as_str)
                    .unwrap_or("<none>")
            );
        }

        let raw_logs = collect_log_values(&result);
        candidate_logs_total += raw_logs.len();

        let mut logs = Vec::new();
        for raw_log in raw_logs {
            match serde_json::from_value::<Log>(raw_log) {
                Ok(log) => {
                    if !matcher.matches(&log) {
                        continue;
                    }
                    let protocol = protocol_by_address
                        .get(&log.address())
                        .copied()
                        .unwrap_or("Unknown");
                    if log.log_index.is_some() {
                        logs_with_log_index += 1;
                    } else {
                        logs_without_log_index += 1;
                    }
                    if let Some(key) = dedup_key(&log) {
                        if !seen_logs.insert(key) {
                            dedup_dropped_logs += 1;
                            continue;
                        }
                    }
                    *matched_logs_by_protocol.entry(protocol).or_insert(0) += 1;
                    if let Some(tx_hash) = log.transaction_hash {
                        tx_diagnostics
                            .entry(format!("{tx_hash:?}"))
                            .or_default()
                            .record(&log, protocol);
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

        if let Some(manager) = &manager {
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
    }

    let elapsed = started.elapsed();
    let elapsed_s = elapsed.as_secs_f64().max(1e-9);
    let mut iv50 = msg_intervals_ms.clone();
    let mut iv95 = msg_intervals_ms.clone();
    let mut bs50 = batch_sizes.clone();
    let mut bs95 = batch_sizes.clone();
    let mut af50 = affected_sizes.clone();
    let mut af95 = affected_sizes.clone();
    let mut protocol_rows: Vec<_> = matched_logs_by_protocol.into_iter().collect();
    protocol_rows.sort_by(|a, b| a.0.cmp(b.0));

    let mut multi_log_txs: Vec<_> = tx_diagnostics
        .iter()
        .filter(|(_, diag)| diag.log_count > 1)
        .collect();
    multi_log_txs.sort_by(|a, b| b.1.log_count.cmp(&a.1.log_count).then_with(|| a.0.cmp(b.0)));
    let txs_with_same_pool_multi_logs = tx_diagnostics
        .values()
        .filter(|diag| diag.per_address_counts.values().any(|count| *count > 1))
        .count();
    let txs_with_non_monotonic_log_index = tx_diagnostics
        .values()
        .filter(|diag| diag.non_monotonic_steps > 0)
        .count();

    println!("\n=== Summary ===");
    println!("elapsed_ms: {}", elapsed.as_millis());
    println!("ws_messages: {}", ws_messages);
    println!("notifications: {}", notifications);
    println!(
        "notification_result_kind: object={} array={} other={}",
        result_object_notifications, result_array_notifications, result_other_notifications
    );
    println!(
        "notifications_per_sec: {:.2}",
        notifications as f64 / elapsed_s
    );
    println!("candidate_logs_total: {}", candidate_logs_total);
    println!("matched_logs_total: {}", matched_logs_total);
    for (protocol, count) in protocol_rows {
        println!("matched_logs_{protocol}: {}", count);
    }
    println!("logs_with_log_index: {}", logs_with_log_index);
    println!("logs_without_log_index: {}", logs_without_log_index);
    println!("dedup_dropped_logs: {}", dedup_dropped_logs);
    println!("decode_fail: {}", decode_fail);
    println!("sync_batches_total: {}", sync_batches_total);
    println!("trigger_batches(affected>0): {}", trigger_batches);
    println!("affected_pools_total: {}", affected_pools_total);
    println!("total_resync: {}", total_resync);
    println!("total_async_update: {}", total_async_update);
    println!("unique_matched_txs: {}", tx_diagnostics.len());
    println!("txs_with_multiple_logs: {}", multi_log_txs.len());
    println!(
        "txs_with_same_pool_multiple_logs: {}",
        txs_with_same_pool_multi_logs
    );
    println!(
        "txs_with_non_monotonic_log_index: {}",
        txs_with_non_monotonic_log_index
    );
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
    if !multi_log_txs.is_empty() {
        println!("\n=== Tx Diagnostics ===");
        for (idx, (tx_hash, diag)) in multi_log_txs.into_iter().take(tx_sample_limit).enumerate() {
            let mut protocols: Vec<_> = diag
                .protocol_counts
                .iter()
                .map(|(protocol, count)| format!("{protocol}:{count}"))
                .collect();
            protocols.sort();
            let mut same_pool_counts: Vec<_> = diag
                .per_address_counts
                .iter()
                .filter(|(_, count)| **count > 1)
                .map(|(address, count)| format!("{address}:{count}"))
                .collect();
            same_pool_counts.sort();
            println!(
                "[tx #{idx}] tx={} block={} logs={} with_log_index={} non_monotonic_steps={} protocols={} same_pool_multi={}",
                tx_hash,
                diag.block_number.unwrap_or_default(),
                diag.log_count,
                diag.with_log_index,
                diag.non_monotonic_steps,
                if protocols.is_empty() {
                    "<none>".to_string()
                } else {
                    protocols.join(",")
                },
                if same_pool_counts.is_empty() {
                    "<none>".to_string()
                } else {
                    same_pool_counts.join(",")
                }
            );
            println!("  seq={}", diag.sequence.join(" | "));
        }
    }
    println!("\n=== Assessment ===");
    println!("pendingLogs returns log-shaped payloads directly.");
    println!("This is the closest match to amms::StateSpace::sync(logs) and should require the least transform work.");
    println!("Server-side filtering is available through the subscription filter, so this path should usually be the simpler and lower-overhead option.");

    Ok(())
}

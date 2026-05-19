use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Filter;
use eyre::{eyre, Result};
use futures::StreamExt;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::time::{Duration, Instant};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

const DEFAULT_FEED_WS: &str = "wss://arb1-feed.arbitrum.io/feed";
const ARB1_GENESIS_OFFSET: u64 = 22_207_817;
const RETRY_BASE_MS: u64 = 50;
const RETRY_MAX_MS: u64 = 1_000;

fn parse_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse::<u64>().ok(),
        _ => None,
    }
}

fn extract_seqs(root: &Value, out: &mut Vec<u64>) {
    let Some(arr) = root.get("messages").and_then(|v| v.as_array()) else {
        return;
    };
    for msg in arr {
        if let Some(seq) = msg.get("sequenceNumber").and_then(parse_u64) {
            out.push(seq);
        }
    }
}

fn is_temporarily_unreadable_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("block not found")
        || m.contains("header not found")
        || m.contains("requested to block")
}

fn next_retry_delay_ms(attempt: u32) -> u64 {
    let shift = attempt.min(31);
    let delay = RETRY_BASE_MS.saturating_mul(1u64 << shift);
    delay.min(RETRY_MAX_MS)
}

struct PendingRetry {
    block: u64,
    attempt: u32,
    next_retry_at: Instant,
}

#[tokio::main]
async fn main() -> Result<()> {
    let feed_ws = std::env::var("ARBITRUM_FEED_WS").unwrap_or_else(|_| DEFAULT_FEED_WS.to_string());
    let rpc_http =
        std::env::var("ARBITRUM_RPC_HTTP").map_err(|_| eyre!("missing ARBITRUM_RPC_HTTP"))?;
    let run_secs: u64 = std::env::var("RUN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let poll_ms: u64 = std::env::var("POLL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let max_blocks_per_cycle: u64 = std::env::var("MAX_BLOCKS_PER_CYCLE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64);
    let worker_sleep_ms: u64 = std::env::var("WORKER_SLEEP_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);

    println!("=== Arbitrum feed head verify ===");
    println!("feed_ws={feed_ws}");
    println!("rpc_http={rpc_http}");
    println!("run_secs={run_secs}, poll_ms={poll_ms}");
    println!("max_blocks_per_cycle={max_blocks_per_cycle}, worker_sleep_ms={worker_sleep_ms}");

    let provider = ProviderBuilder::new().connect_http(rpc_http.parse()?);
    let deadline = Instant::now() + Duration::from_secs(run_secs);
    let mut tick = tokio::time::interval(Duration::from_millis(poll_ms));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let shutdown = Arc::new(AtomicBool::new(false));
    let has_seq = Arc::new(AtomicBool::new(false));
    let max_seq = Arc::new(AtomicU64::new(0));
    let msg_count = Arc::new(AtomicU64::new(0));
    let seq_count = Arc::new(AtomicU64::new(0));
    let seq_non_monotonic_count = Arc::new(AtomicU64::new(0));
    let seq_duplicate_count = Arc::new(AtomicU64::new(0));
    let reconnect_count = Arc::new(AtomicU64::new(0));

    let feed_ws_for_task = feed_ws.clone();
    let shutdown_feed = Arc::clone(&shutdown);
    let has_seq_feed = Arc::clone(&has_seq);
    let max_seq_feed = Arc::clone(&max_seq);
    let msg_count_feed = Arc::clone(&msg_count);
    let seq_count_feed = Arc::clone(&seq_count);
    let seq_non_monotonic_count_feed = Arc::clone(&seq_non_monotonic_count);
    let seq_duplicate_count_feed = Arc::clone(&seq_duplicate_count);
    let reconnect_count_feed = Arc::clone(&reconnect_count);

    let feed_task = tokio::spawn(async move {
        let mut last_seq_seen: Option<u64> = None;
        let mut first_connect = true;

        while !shutdown_feed.load(Ordering::Relaxed) {
            if !first_connect {
                reconnect_count_feed.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            first_connect = false;

            let connect = connect_async(&feed_ws_for_task).await;
            let (mut ws, _) = match connect {
                Ok(pair) => pair,
                Err(e) => {
                    eprintln!("feed connect failed: {e}");
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    continue;
                }
            };

            while !shutdown_feed.load(Ordering::Relaxed) {
                let maybe_msg = ws.next().await;
                let Some(msg_res) = maybe_msg else {
                    break;
                };
                let msg = match msg_res {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("feed stream error: {e}");
                        break;
                    }
                };

                let payload = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                    Message::Close(_) => break,
                };

                let json: Value = match serde_json::from_str(&payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                let mut seqs = Vec::new();
                extract_seqs(&json, &mut seqs);
                if seqs.is_empty() {
                    continue;
                }

                msg_count_feed.fetch_add(1, Ordering::Relaxed);
                for seq in seqs {
                    seq_count_feed.fetch_add(1, Ordering::Relaxed);
                    if let Some(prev) = last_seq_seen {
                        if seq <= prev {
                            seq_non_monotonic_count_feed.fetch_add(1, Ordering::Relaxed);
                        }
                        if seq == prev {
                            seq_duplicate_count_feed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    last_seq_seen = Some(seq);

                    let mut current = max_seq_feed.load(Ordering::Relaxed);
                    while seq > current {
                        match max_seq_feed.compare_exchange_weak(
                            current,
                            seq,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => break,
                            Err(observed) => current = observed,
                        }
                    }
                    has_seq_feed.store(true, Ordering::Relaxed);
                }
            }
        }
    });

    let mut diff_samples: Vec<i64> = Vec::new();
    let mut sync_lag_samples: Vec<i64> = Vec::new();
    let mut last_checked_block: Option<u64> = None;
    let mut pending_retry: Option<PendingRetry> = None;
    let mut getlogs_attempted_blocks: u64 = 0;
    let mut getlogs_success_blocks: u64 = 0;
    let mut getlogs_unreadable_blocks: u64 = 0;
    let mut getlogs_other_error_blocks: u64 = 0;
    let mut getlogs_total_logs: u64 = 0;
    let mut getlogs_retry_attempts: u64 = 0;
    let mut tip_probe_attempted: u64 = 0;
    let mut tip_probe_success: u64 = 0;
    let mut tip_probe_unreadable: u64 = 0;
    let mut tip_probe_other_error: u64 = 0;
    let mut last_l2_head: Option<u64> = None;

    loop {
        if Instant::now() >= deadline {
            break;
        }

        if has_seq.load(Ordering::Relaxed) {
            let seq = max_seq.load(Ordering::Relaxed);
            let l2_head = seq.saturating_add(ARB1_GENESIS_OFFSET);
            last_l2_head = Some(l2_head);
            if last_checked_block.is_none() {
                last_checked_block = Some(l2_head.saturating_sub(1));
            }

            let mut processed_in_cycle = 0u64;
            while processed_in_cycle < max_blocks_per_cycle {
                let now = Instant::now();
                let block = if let Some(retry) = &pending_retry {
                    if now < retry.next_retry_at {
                        break;
                    }
                    retry.block
                } else if let Some(last) = last_checked_block {
                    if last >= l2_head {
                        break;
                    }
                    last.saturating_add(1)
                } else {
                    break;
                };

                getlogs_attempted_blocks = getlogs_attempted_blocks.saturating_add(1);
                if pending_retry.is_some() {
                    getlogs_retry_attempts = getlogs_retry_attempts.saturating_add(1);
                }

                let filter = Filter::new().from_block(block).to_block(block);
                match provider.get_logs(&filter).await {
                    Ok(logs) => {
                        getlogs_success_blocks = getlogs_success_blocks.saturating_add(1);
                        getlogs_total_logs = getlogs_total_logs.saturating_add(logs.len() as u64);
                        last_checked_block = Some(block);
                        pending_retry = None;
                        processed_in_cycle = processed_in_cycle.saturating_add(1);
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if is_temporarily_unreadable_error(&msg) {
                            getlogs_unreadable_blocks = getlogs_unreadable_blocks.saturating_add(1);
                        } else {
                            getlogs_other_error_blocks =
                                getlogs_other_error_blocks.saturating_add(1);
                        }

                        let next_attempt = pending_retry
                            .as_ref()
                            .map(|r| r.attempt.saturating_add(1))
                            .unwrap_or(0);
                        let delay_ms = next_retry_delay_ms(next_attempt);
                        pending_retry = Some(PendingRetry {
                            block,
                            attempt: next_attempt,
                            next_retry_at: Instant::now() + Duration::from_millis(delay_ms),
                        });
                        break;
                    }
                }
            }
        }

        tokio::select! {
            _ = tick.tick() => {
                let seq = max_seq.load(Ordering::Relaxed);
                if !has_seq.load(Ordering::Relaxed) {
                    println!("[check] waiting_for_seq");
                    continue;
                }
                let l2_head = seq.saturating_add(ARB1_GENESIS_OFFSET);
                let rpc_head = provider.get_block_number().await?;
                let diff = l2_head as i64 - rpc_head as i64;
                diff_samples.push(diff);

                tip_probe_attempted = tip_probe_attempted.saturating_add(1);
                let tip_filter = Filter::new().from_block(l2_head).to_block(l2_head);
                match provider.get_logs(&tip_filter).await {
                    Ok(_) => {
                        tip_probe_success = tip_probe_success.saturating_add(1);
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if is_temporarily_unreadable_error(&msg) {
                            tip_probe_unreadable = tip_probe_unreadable.saturating_add(1);
                        } else {
                            tip_probe_other_error = tip_probe_other_error.saturating_add(1);
                        }
                    }
                }

                let checked = last_checked_block.unwrap_or(l2_head.saturating_sub(1));
                let sync_lag = l2_head.saturating_sub(checked);
                sync_lag_samples.push(sync_lag as i64);

                println!(
                    "[check] max_seq={} l2_from_seq={} rpc_head={} diff={} synced_head={} sync_lag={} getlogs_attempted={} getlogs_success={} unreadable={} other_err={} retries={} tip_probe_attempted={} tip_probe_success={} tip_probe_unreadable={} tip_probe_other={}",
                    seq,
                    l2_head,
                    rpc_head,
                    diff,
                    checked,
                    sync_lag,
                    getlogs_attempted_blocks,
                    getlogs_success_blocks,
                    getlogs_unreadable_blocks,
                    getlogs_other_error_blocks,
                    getlogs_retry_attempts,
                    tip_probe_attempted,
                    tip_probe_success,
                    tip_probe_unreadable,
                    tip_probe_other_error
                );
            }
            _ = tokio::time::sleep(Duration::from_millis(worker_sleep_ms)) => {}
        }
    }

    shutdown.store(true, Ordering::Relaxed);
    let _ = tokio::time::timeout(Duration::from_secs(1), feed_task).await;

    if diff_samples.is_empty() {
        println!("summary: no diff samples collected");
        return Ok(());
    }

    let mut min_d = diff_samples[0];
    let mut max_d = diff_samples[0];
    let mut sum_d: i128 = 0;
    let mut pos = 0usize;
    let mut zero = 0usize;
    let mut neg = 0usize;
    for d in &diff_samples {
        min_d = min_d.min(*d);
        max_d = max_d.max(*d);
        sum_d += *d as i128;
        if *d > 0 {
            pos += 1;
        } else if *d == 0 {
            zero += 1;
        } else {
            neg += 1;
        }
    }

    let mut lag_min = sync_lag_samples[0];
    let mut lag_max = sync_lag_samples[0];
    let mut lag_sum: i128 = 0;
    for lag in &sync_lag_samples {
        lag_min = lag_min.min(*lag);
        lag_max = lag_max.max(*lag);
        lag_sum += *lag as i128;
    }

    let max_seq_val = max_seq.load(Ordering::Relaxed);
    let l2_from_atomic = max_seq_val.saturating_add(ARB1_GENESIS_OFFSET);
    let final_l2_head = last_l2_head
        .map(|cached| cached.max(l2_from_atomic))
        .unwrap_or(l2_from_atomic);
    let final_synced_head = last_checked_block.unwrap_or(final_l2_head.saturating_sub(1));
    let final_sync_lag = final_l2_head.saturating_sub(final_synced_head);

    println!(
        "summary: msg_count={} seq_count={} seq_duplicate_count={} seq_non_monotonic_count={} reconnect_count={} sample_count={} diff_min={} diff_max={} diff_avg={:.3} pos={} zero={} neg={} lag_min={} lag_max={} lag_avg={:.3} final_l2_head={} final_synced_head={} final_sync_lag={}",
        msg_count.load(Ordering::Relaxed),
        seq_count.load(Ordering::Relaxed),
        seq_duplicate_count.load(Ordering::Relaxed),
        seq_non_monotonic_count.load(Ordering::Relaxed),
        reconnect_count.load(Ordering::Relaxed),
        diff_samples.len(),
        min_d,
        max_d,
        (sum_d as f64) / (diff_samples.len() as f64),
        pos,
        zero,
        neg,
        lag_min,
        lag_max,
        (lag_sum as f64) / (sync_lag_samples.len() as f64),
        final_l2_head,
        final_synced_head,
        final_sync_lag
    );

    let success_rate = if getlogs_attempted_blocks == 0 {
        0.0
    } else {
        (getlogs_success_blocks as f64) * 100.0 / (getlogs_attempted_blocks as f64)
    };
    let tip_probe_success_rate = if tip_probe_attempted == 0 {
        0.0
    } else {
        (tip_probe_success as f64) * 100.0 / (tip_probe_attempted as f64)
    };
    println!(
        "getlogs_summary: attempted_blocks={} success_blocks={} unreadable_blocks={} other_error_blocks={} retry_attempts={} success_rate={:.2}% total_logs={} tip_probe_attempted={} tip_probe_success={} tip_probe_unreadable={} tip_probe_other_error={} tip_probe_success_rate={:.2}%",
        getlogs_attempted_blocks,
        getlogs_success_blocks,
        getlogs_unreadable_blocks,
        getlogs_other_error_blocks,
        getlogs_retry_attempts,
        success_rate,
        getlogs_total_logs,
        tip_probe_attempted,
        tip_probe_success,
        tip_probe_unreadable,
        tip_probe_other_error,
        tip_probe_success_rate
    );

    Ok(())
}

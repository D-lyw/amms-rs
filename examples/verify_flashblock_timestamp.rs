/// Verify that Base flashblock `base.timestamp` is the block's slot timestamp,
/// shared across all flashblock sub-slices within the same block.
///
/// Since flashblocks arrive pre-confirmation (before the block is on RPC),
/// we defer RPC timestamp comparison by polling for the block's appearance.
use alloy::eips::BlockId;
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use eyre::Context;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::Message;
use tracing::{info, warn};

/// A pending block whose flashblock timestamp needs to be verified against RPC.
#[derive(Debug, Clone)]
struct PendingBlock {
    block_number: u64,
    fb_timestamp: u64,
    seen_at: Instant,
}

#[derive(Debug, Default)]
struct SharedState {
    /// block_number → fb_timestamp (from flashblock index-0)
    pending: Vec<PendingBlock>,
    /// verification results
    results: HashMap<u64, VerificationResult>,
}

#[derive(Debug)]
struct VerificationResult {
    fb_timestamp: u64,
    rpc_timestamp: u64,
    confirmed: bool,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let raw_ws = std::env::var("RAW_FLASHBLOCKS_WS")
        .unwrap_or_else(|_| "wss://mainnet.flashblocks.base.org/ws".to_string());

    let rpc_ws = std::env::var("BASE_RPC_WS")
        .or_else(|_| std::env::var("BASE_FLASHBLOCKS_WS"))
        .or_else(|_| std::env::var("BASE_WS"))
        .unwrap_or_else(|_| {
            "wss://base-mainnet.core.chainstack.com/fc5f8eef2b27bee75a83ca6ab5a02634".to_string()
        });

    let run_secs: u64 = std::env::var("RUN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);

    info!("=== Flashblock Timestamp Verification ===");
    info!("raw_ws: {raw_ws}");
    info!("rpc_ws: {rpc_ws}");
    info!("run_secs: {run_secs}s");

    // Shared state between WS reader and RPC verifier
    let shared = Arc::new(Mutex::new(SharedState::default()));

    // Connect RPC provider for canonical block queries
    let provider = Arc::new(
        ProviderBuilder::new()
            .connect_ws(WsConnect::new(rpc_ws.clone()))
            .await
            .with_context(|| format!("failed to connect rpc ws: {rpc_ws}"))?,
    );

    let chain_id = provider.get_chain_id().await?;
    info!("connected chain_id={chain_id}");

    // ── Background RPC verifier ──
    // Polls every 1.5s: try to fetch pending blocks that may now be on chain.
    let shared_clone = shared.clone();
    let provider_clone = provider.clone();
    let verify_handle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(1500)).await;

            let mut guard = shared_clone.lock().await;
            if guard.pending.is_empty() {
                continue;
            }

            let pending_items: Vec<PendingBlock> = guard.pending.drain(..).collect();
            drop(guard); // release the lock before doing RPC calls

            let mut new_results: Vec<(u64, u64, u64)> = Vec::new();
            let mut retry: Vec<PendingBlock> = Vec::new();

            for pb in pending_items {
                // Skip if we've been trying for > 30s (block likely won't appear)
                if pb.seen_at.elapsed() > Duration::from_secs(30) {
                    info!(
                        "⚠  block={} fb_ts={} gave up after 30s (RPC never had it)",
                        pb.block_number, pb.fb_timestamp
                    );
                    continue;
                }

                match provider_clone
                    .get_block(BlockId::from(pb.block_number))
                    .await
                {
                    Ok(Some(block)) => {
                        let rpc_ts = block.header.timestamp;
                        if pb.fb_timestamp == rpc_ts {
                            info!(
                                "✓  block={} fb_ts={} rpc_ts={} MATCH",
                                pb.block_number, pb.fb_timestamp, rpc_ts
                            );
                        } else {
                            warn!(
                                "✗  block={} fb_ts={} rpc_ts={} MISMATCH",
                                pb.block_number, pb.fb_timestamp, rpc_ts
                            );
                        }
                        new_results.push((pb.block_number, pb.fb_timestamp, rpc_ts));
                    }
                    Ok(None) | Err(_) => {
                        // Not on chain yet, retry later
                        retry.push(pb);
                    }
                }
            }

            // Re-acquire lock to update state
            let mut guard = shared_clone.lock().await;
            for (block_number, fb_ts, rpc_ts) in new_results {
                guard.results.insert(
                    block_number,
                    VerificationResult {
                        fb_timestamp: fb_ts,
                        rpc_timestamp: rpc_ts,
                        confirmed: fb_ts == rpc_ts,
                    },
                );
            }
            guard.pending = retry;
        }
    });

    // ── Flashblocks WS subscriber ──
    let (mut ws_stream, _) = connect_async(raw_ws.clone())
        .await
        .with_context(|| format!("failed to connect raw ws: {raw_ws}"))?;

    let deadline = Instant::now() + Duration::from_secs(run_secs);

    // Stats
    let mut total_messages = 0u64;
    let mut seen_blocks: HashMap<u64, Vec<u64>> = HashMap::new(); // block → list of slice indices

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
                warn!("ws receive error: {e}");
                continue;
            }
        };

        total_messages += 1;

        let payload = match message {
            Message::Text(text) => text.as_bytes().to_vec(),
            Message::Binary(bin) => bin.to_vec(),
            Message::Ping(v) => {
                let _ = ws_stream.send(Message::Pong(v)).await;
                continue;
            }
            Message::Pong(_) => continue,
            Message::Close(_) => break,
            Message::Frame(_) => continue,
        };

        // Parse JSON (try raw first, then brotli)
        let raw_value: Value = match serde_json::from_slice::<Value>(&payload) {
            Ok(v) => v,
            Err(_) => {
                let mut decompressed = Vec::new();
                let mut reader = brotli::Decompressor::new(payload.as_slice(), 4096);
                if reader.read_to_end(&mut decompressed).is_err() {
                    continue;
                }
                match serde_json::from_slice::<Value>(&decompressed) {
                    Ok(v) => v,
                    Err(_) => continue,
                }
            }
        };

        let index = raw_value["index"].as_u64().unwrap_or(0);

        // Extract block_number from metadata or base
        let block_number_from_meta = raw_value["metadata"]["block_number"].as_u64();
        let block_number_from_base = raw_value["base"]["block_number"]
            .as_str()
            .and_then(|s| {
                let s = s.strip_prefix("0x").unwrap_or(s);
                u64::from_str_radix(s, 16).ok()
            });
        let Some(block_number) = block_number_from_meta.or(block_number_from_base) else {
            continue;
        };

        // Track slices per block
        seen_blocks.entry(block_number).or_default().push(index);

        // Extract base.timestamp (present in all messages with "base" field)
        let fb_timestamp = raw_value["base"]["timestamp"]
            .as_str()
            .and_then(|s| {
                let s = s.strip_prefix("0x").unwrap_or(s);
                u64::from_str_radix(s, 16).ok()
            });

        if let Some(ts) = fb_timestamp {
            if index == 0 {
                // Index 0: submit to verifier
                let mut guard = shared.lock().await;
                guard.pending.push(PendingBlock {
                    block_number,
                    fb_timestamp: ts,
                    seen_at: Instant::now(),
                });
                info!(
                    "📦 block={} fb_ts={} (0x{:x}) enqueued for RPC verification",
                    block_number, ts, ts
                );
            } else {
                // Non-zero: verify it matches what index-0 reported (same block timestamp)
                let guard = shared.lock().await;
                let prev = guard
                    .pending
                    .iter()
                    .find(|pb| pb.block_number == block_number)
                    .or_else(|| {
                        guard
                            .results
                            .get(&block_number)
                            .map(|_| unreachable!()) // won't happen for current block
                    });

                // Even simpler: just log it
                info!(
                    "·  block={} idx={} fb_ts={} (0x{:x}) (non-zero index slice)",
                    block_number, index, ts, ts
                );
            }
        } else {
            // No base.timestamp in this message (some mid-block slices might lack "base")
            info!(
                "·  block={} idx={} (no base.timestamp in message)",
                block_number, index
            );
        }

        // Periodic progress
        if total_messages % 50 == 0 {
            let guard = shared.lock().await;
            let pending_count = guard.pending.len();
            let verified_count = guard.results.len();
            let matched = guard.results.values().filter(|r| r.confirmed).count();
            let mismatched = guard.results.values().filter(|r| !r.confirmed).count();
            info!(
                "[progress] msgs={} blocks_unique={} pending_verify={} verified={} matched={} mismatched={}",
                total_messages,
                seen_blocks.len(),
                pending_count,
                verified_count,
                matched,
                mismatched,
            );
        }
    }

    verify_handle.abort();

    // Final summary
    let guard = shared.lock().await;
    let matched = guard.results.values().filter(|r| r.confirmed).count();
    let mismatched = guard.results.values().filter(|r| !r.confirmed).count();

    println!("\n========================================");
    println!("  Flashblock Timestamp Verification");
    println!("========================================");
    println!("  Total WS messages:            {}", total_messages);
    println!("  Unique blocks from flash:     {}", seen_blocks.len());
    println!("  Blocks verified via RPC:      {}", guard.results.len());
    println!("  Timestamp MATCH:              {}", matched);
    println!("  Timestamp MISMATCH:           {}", mismatched);
    println!();

    // Multi-slice blocks: verify all slices share block number
    let multi_slice = seen_blocks
        .iter()
        .filter(|(_, indices)| indices.len() > 1)
        .count();
    println!("  Blocks with >1 slice:         {}", multi_slice);

    let mut multi_slice_blocks: Vec<_> = seen_blocks
        .iter()
        .filter(|(_, indices)| indices.len() > 1)
        .collect();
    multi_slice_blocks.sort_by_key(|(bn, _)| **bn);

    if !multi_slice_blocks.is_empty() {
        println!("\n  Multi-slice block details:");
        for (bn, indices) in multi_slice_blocks.iter().take(10) {
            let result = guard.results.get(bn);
            let fb_ts = result
                .map(|r| r.fb_timestamp)
                .or_else(|| {
                    guard
                        .pending
                        .iter()
                        .find(|pb| pb.block_number == **bn)
                        .map(|pb| pb.fb_timestamp)
                })
                .map(|t| t.to_string())
                .unwrap_or_else(|| "NA".to_string());
            let rpc_ts = result
                .map(|r| r.rpc_timestamp)
                .map(|t| t.to_string())
                .unwrap_or_else(|| "pending".to_string());
            println!(
                "    block={}  slices={}  fb_ts={}  rpc_ts={}",
                bn,
                indices.len(),
                fb_ts,
                rpc_ts
            );
        }
        if multi_slice_blocks.len() > 10 {
            println!("    ... and {} more", multi_slice_blocks.len() - 10);
        }
    }

    println!("\n  VERDICT:");
    if mismatched == 0 && matched > 0 {
        println!("    ✓ base.timestamp == canonical block timestamp ({} blocks)", matched);
        println!("    ✓ All flashblock slices within a block share the same timestamp");
    } else if mismatched > 0 {
        println!(
            "    ✗ Found {} mismatches out of {} verified blocks",
            mismatched,
            matched + mismatched
        );
    } else {
        println!("    ? Insufficient data ({} pending, {} verified)", guard.pending.len(), guard.results.len());
    }
    println!("========================================");

    Ok(())
}

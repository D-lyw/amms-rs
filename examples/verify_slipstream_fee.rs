/// Deterministic verification of Slipstream dynamic fee computation.
///
/// Strategy:
///   1. Use StateSpaceBuilder::block(N) to initialize all pools at a specific historical block
///   2. Fetch all event logs for the next N blocks via get_logs
///   3. Replay events block-by-block, updating local pool state via sync()
///   4. After each full block, compare local compute_fee() with on-chain fee()
use alloy::eips::BlockId;
use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::Filter;
use amms::amms::{
    aerodrome_slipstream::pool::{AerodromeSlipstreamPool, ICLPool},
    amm::{AutomatedMarketMaker, AMM},
};
use amms::state_space::StateSpaceBuilder;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::info;

// const POOLS: &[&str] = &[
//     "0x0652202C4b2D09CB93aEDeFAdc14B36869483a98",
//     "0x17f707CF3EDBbd5d9251D4bCDF9Ad70a247D7B84",
//     "0xDB20b9455DEB2D616727cbdae4aC9F8eeB9AC899",
//     "0xF81d3c731b3AC5a4DFC968f514860beDEEbeBAf2",
//     "0x8BB9eAF3C5A906D20c4CC10eA2F73A3Ac2D5d41A",
//     "0x2ae9DF02539887d4EbcE0230168a302d34784c82",
//     "0x5d4e504EB4c526995E0cC7A6E327FDa75D8B52b5",
//     "0x948e80fBB383694b462f79557a3A44a25416dc72",
//     "0xCDD442e2De893c07146B2F1072f8e077559f9aa4",
//     "0xFCda5ab6BBC1fe5B8e1a185e86bb5f24b12e2278",
//     "0xC200F21EfE67c7F41B81A854c26F9cdA80593065",
// ];

const GRAPH_PATH: &str = "/Users/d-lyw/D-lyw/dex-arbitrage/configs/8453_graph.ndjson";
const POOL_LIMIT: usize = 15;
const VERIFY_BLOCKS: u64 = 30;

/// Load AerodromeSlipstream pool addresses from the graph ndjson file
fn load_slipstream_pools(path: &str, limit: usize) -> Vec<Address> {
    let content = std::fs::read_to_string(path).expect("Failed to read graph file");
    content
        .lines()
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            if v.get("pool_type")?.as_str()? == "AerodromeSlipstream" {
                let addr = v.get("address")?.as_str()?;
                Address::from_str(addr).ok()
            } else {
                None
            }
        })
        .take(limit)
        .collect()
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let rpc_ws = std::env::var("BASE_RPC_WS").unwrap_or_else(|_| {
        "wss://base-mainnet.core.chainstack.com/fc5f8eef2b27bee75a83ca6ab5a02634".to_string()
    });
    let provider = Arc::new(
        ProviderBuilder::new()
            .connect_ws(WsConnect::new(rpc_ws))
            .await?,
    );

    let pool_addrs = load_slipstream_pools(GRAPH_PATH, POOL_LIMIT);
    info!(
        "Loaded {} AerodromeSlipstream pools from graph",
        pool_addrs.len()
    );

    let start_block: u64 = 45_278_200;
    let end_block = start_block + VERIFY_BLOCKS;
    info!(
        "Initializing at block {start_block}, verifying blocks {} to {end_block}",
        start_block + 1
    );

    // ── Step 1: Initialize all pools at start_block via StateSpaceBuilder ──
    let amms: Vec<AMM> = pool_addrs
        .iter()
        .map(|&addr| AerodromeSlipstreamPool::new(addr).into())
        .collect();

    let manager = StateSpaceBuilder::new(provider.clone())
        .with_amms(amms)
        .block(start_block)
        .with_pending_sync_worker_interval(Duration::from_secs(3600)) // effectively disable
        .sync()
        .await?;
    info!("Initialization complete at block {start_block}");

    // Print initial state for all pools
    {
        let guard = manager.state.read().await;
        for &addr in &pool_addrs {
            if let Some(amm) = guard.state.get(&addr) {
                if let AMM::AerodromeSlipstreamPool(p) = amm.as_ref() {
                    info!(
                        "  pool={} tick={} fee={} dfc={:?} obs_seeded={} obs_card={} obs_idx={}",
                        addr,
                        p.tick,
                        p.fee,
                        p.dynamic_fee_config,
                        p.observations_cache.seeded,
                        p.observations_cache.cardinality,
                        p.observations_cache.index,
                    );
                }
            }
        }
    }

    // ── Step 2: Extract pools from manager into local mutable HashMap ──
    // We clone pools out so we can mutate them without holding the manager lock
    let mut pools: std::collections::HashMap<Address, AerodromeSlipstreamPool> = {
        let guard = manager.state.read().await;
        let mut map = std::collections::HashMap::new();
        for &addr in &pool_addrs {
            if let Some(amm) = guard.state.get(&addr) {
                if let AMM::AerodromeSlipstreamPool(p) = amm.as_ref() {
                    map.insert(addr, p.clone());
                }
            }
        }
        map
    };

    // ── Step 3: Fetch all event logs from start_block+1 to end_block ──
    info!(
        "Fetching event logs for blocks {} to {end_block}...",
        start_block + 1
    );

    let sync_topics: Vec<_> = pools
        .values()
        .next()
        .map(|p| {
            let amm: AMM = AMM::AerodromeSlipstreamPool(p.clone());
            amm.sync_events()
        })
        .unwrap_or_default();

    let filter = Filter::new()
        .address(pool_addrs.clone())
        .event_signature(sync_topics)
        .from_block(start_block + 1)
        .to_block(end_block);
    let all_logs = provider.get_logs(&filter).await?;
    info!("Fetched {} event logs", all_logs.len());

    // Group logs by block number (preserving order within each block)
    let mut logs_by_block: std::collections::BTreeMap<u64, Vec<&alloy::rpc::types::Log>> =
        std::collections::BTreeMap::new();
    for log in &all_logs {
        if let Some(bn) = log.block_number {
            logs_by_block.entry(bn).or_default().push(log);
        }
    }

    // ── Step 4: Replay block by block and verify ──
    let mut total = 0u32;
    let mut matched = 0u32;
    let mut mismatches: Vec<String> = Vec::new();

    for block_num in (start_block + 1)..=end_block {
        // Get block timestamp
        let block = provider
            .get_block(BlockId::from(block_num))
            .await?
            .ok_or_else(|| eyre::eyre!("Block {block_num} not found"))?;
        let timestamp = block.header.timestamp;

        // Process all logs for this block (already in correct order)
        if let Some(block_logs) = logs_by_block.get(&block_num) {
            for log in block_logs {
                let addr = log.address();
                if let Some(pool) = pools.get_mut(&addr) {
                    let mut amm = AMM::AerodromeSlipstreamPool(pool.clone());
                    if amm.sync(log).is_ok() {
                        if let AMM::AerodromeSlipstreamPool(updated) = amm {
                            *pool = updated;
                        }
                    }
                }
            }
        }

        // Verify fee for each pool at this block
        for &addr in &pool_addrs {
            let pool = pools.get(&addr).unwrap();
            let local_fee = pool.compute_fee(timestamp as u32);

            let rpc_fee = ICLPool::new(addr, provider.clone())
                .fee()
                .block(BlockId::from(block_num))
                .call()
                .await?
                .to::<u32>();

            total += 1;
            if local_fee == rpc_fee {
                matched += 1;
            } else {
                let msg = format!(
                    "✗ {} block={} local={} rpc={} tick={} dfc={:?} last_obs_ts={:?}",
                    addr,
                    block_num,
                    local_fee,
                    rpc_fee,
                    pool.tick,
                    pool.dynamic_fee_config,
                    pool.observations_cache.last().map(|o| o.block_timestamp),
                );
                mismatches.push(msg);
            }
        }
        sleep(Duration::from_millis(50)).await;
    }

    // ── Summary ──
    println!("\n{matched}/{total} MATCH, {} MISMATCH", total - matched);
    for m in &mismatches {
        println!("{m}");
    }
    Ok(())
}

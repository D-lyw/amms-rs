use alloy::{
    primitives::{
        aliases::{I24, U24},
        Address, FixedBytes, U256,
    },
    providers::{Provider, ProviderBuilder},
};
use amms::{
    amms::{
        aerodrome_slipstream::AerodromeSlipstreamPool,
        aerodrome_v2::AerodromeV2Pool,
        amm::{AutomatedMarketMaker, AMM},
        balancer_v2::{BalancerV2Pool, BalancerV2PoolType},
        balancer_v3::{BalancerV3Pool, BalancerV3PoolType},
        curve_legacy::{CurveLegacyPool, CurveLegacyPoolType},
        curve_ng::{CurveNGPool, CurveNGPoolType},
        ekubo::{EkuboPool, EkuboPoolKey},
        fluid_dex::FluidDexPool,
        pancake_v2::PancakeV2Pool,
        pancake_v3::PancakeV3Pool,
        sky::{SkyConverter, SkyConverterType},
        sushi_v2::SushiV2Pool,
        uniswap_v2::UniswapV2Pool,
        uniswap_v3::UniswapV3Pool,
        uniswap_v4::{IPoolManager, UniswapV4Pool},
    },
    state_space::StateSpaceBuilder,
};
use eyre::Context;
use futures::StreamExt;
use serde::Deserialize;
use std::{
    collections::HashMap,
    path::Path,
    str::FromStr,
    sync::atomic::Ordering,
    sync::Arc,
    time::{Duration, Instant},
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

const ARBITRUM_CHAIN_ID: u64 = 42161;
const DEFAULT_ARBITRUM_RPC_HTTP: &str =
    "https://arbitrum-mainnet.core.chainstack.com/99c0428eb4644b9e6265d42506b1071e";

#[derive(Debug, Deserialize)]
struct PoolRecord {
    #[serde(rename = "type")]
    kind: String,
    address: Option<String>,
    pool_type: Option<String>,
    token0: Option<String>,
    token1: Option<String>,
    fee: Option<u64>,
    tick_spacing: Option<i32>,
    hooks: Option<String>,
    pool_id: Option<String>,
    pool_subtype: Option<String>,
    vault_address: Option<String>,
    factory: Option<String>,
}

fn parse_addr(v: Option<&str>) -> Option<Address> {
    v.and_then(|s| Address::from_str(s).ok())
}

fn parse_u256(v: Option<&str>) -> Option<U256> {
    v.and_then(|s| U256::from_str(s).ok())
}

fn resolve_graph_path() -> String {
    if let Ok(path) = std::env::var("GRAPH_PATH") {
        return path;
    }

    let candidates = [
        "/Users/d-lyw/D-lyw/dex-arbitrage/configs/42161_graph.ndjson",
        "configs/42161_graph.ndjson",
    ];
    for p in candidates {
        if Path::new(p).exists() {
            return p.to_string();
        }
    }

    "/Users/d-lyw/D-lyw/dex-arbitrage/configs/42161_graph.ndjson".to_string()
}

fn load_amms_from_graph(path: &str, pool_limit: Option<usize>) -> eyre::Result<Vec<AMM>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read graph file: {path}"))?;

    let mut amms = Vec::new();
    let mut skipped = 0usize;
    let mut by_type: HashMap<String, usize> = HashMap::new();

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let rec: PoolRecord = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if rec.kind != "pool" {
            continue;
        }
        if let Some(limit) = pool_limit {
            if amms.len() >= limit {
                break;
            }
        }

        let addr = match parse_addr(rec.address.as_deref()) {
            Some(v) => v,
            None => {
                skipped += 1;
                continue;
            }
        };
        let Some(pool_type) = rec.pool_type.as_deref() else {
            skipped += 1;
            continue;
        };

        let add_ok = match pool_type {
            "UniswapV2" => {
                amms.push(UniswapV2Pool::new(addr).into());
                true
            }
            "SushiswapV2" => {
                amms.push(SushiV2Pool::new(addr).into());
                true
            }
            "PancakeV2" => {
                amms.push(PancakeV2Pool::new(addr).into());
                true
            }
            "UniswapV3" | "SushiswapV3" => {
                amms.push(UniswapV3Pool::new(addr).into());
                true
            }
            "PancakeV3" => {
                amms.push(PancakeV3Pool::new(addr).into());
                true
            }
            "AerodromeV2" => {
                amms.push(AerodromeV2Pool::new(addr).into());
                true
            }
            "AerodromeSlipstream" => {
                amms.push(AerodromeSlipstreamPool::new(addr).into());
                true
            }
            "UniswapV4" => {
                let manager = parse_addr(rec.factory.as_deref());
                let token0 = parse_addr(rec.token0.as_deref());
                let token1 = parse_addr(rec.token1.as_deref());
                let hooks = parse_addr(rec.hooks.as_deref()).unwrap_or(Address::ZERO);
                let fee = rec.fee.unwrap_or(0);
                let tick_spacing = rec.tick_spacing.unwrap_or(0);
                if let (Some(manager), Some(token0), Some(token1)) = (manager, token0, token1) {
                    if tick_spacing > 0 {
                        if let Ok(tick_spacing_i24) = I24::try_from(tick_spacing) {
                            let key = IPoolManager::PoolKey {
                                currency0: token0,
                                currency1: token1,
                                fee: U24::from(fee as u32),
                                tickSpacing: tick_spacing_i24,
                                hooks,
                            };
                            amms.push(UniswapV4Pool::new(manager, key).into());
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            "FluidDex" => {
                let resolver = parse_addr(rec.factory.as_deref()).unwrap_or(Address::ZERO);
                amms.push(FluidDexPool::new(addr, resolver).into());
                true
            }
            "BalancerV2" => {
                let vault = parse_addr(rec.vault_address.as_deref());
                let pool_id = rec
                    .pool_id
                    .as_deref()
                    .and_then(|s| FixedBytes::<32>::from_str(s).ok());
                if let (Some(vault), Some(pool_id)) = (vault, pool_id) {
                    let subtype = rec.pool_subtype.as_deref().unwrap_or("Weighted");
                    let pool_type = match subtype {
                        "Stable" | "MetaStable" => BalancerV2PoolType::Stable,
                        "ComposableStable" => BalancerV2PoolType::ComposableStable,
                        _ => BalancerV2PoolType::Weighted,
                    };
                    amms.push(BalancerV2Pool::new(addr, vault, pool_id, pool_type).into());
                    true
                } else {
                    false
                }
            }
            "BalancerV3" => {
                let vault = parse_addr(rec.vault_address.as_deref());
                if let Some(vault) = vault {
                    let subtype = rec.pool_subtype.as_deref().unwrap_or("Weighted");
                    let pool_type = match subtype {
                        "Stable" | "Boosted" => BalancerV3PoolType::Stable,
                        _ => BalancerV3PoolType::Weighted,
                    };
                    amms.push(BalancerV3Pool::new(addr, vault, pool_type).into());
                    true
                } else {
                    false
                }
            }
            "CurveNG" => {
                let subtype = rec.pool_subtype.as_deref().unwrap_or("StableSwapNG");
                let pool_type = match subtype {
                    "TwoCryptoNG" => CurveNGPoolType::TwoCrypto,
                    "TriCryptoNG" => CurveNGPoolType::TriCrypto,
                    _ => CurveNGPoolType::StableSwap,
                };
                amms.push(CurveNGPool::new(addr, pool_type).into());
                true
            }
            "CurveLegacy" => {
                let subtype = rec.pool_subtype.as_deref().unwrap_or("LegacyStable");
                let pool_type = match subtype {
                    "LegacyCrypto" => CurveLegacyPoolType::CryptoSwap,
                    _ => CurveLegacyPoolType::StableSwap,
                };
                amms.push(CurveLegacyPool::new(addr, pool_type).into());
                true
            }
            "Ekubo" => {
                let token0 = parse_addr(rec.token0.as_deref());
                let token1 = parse_addr(rec.token1.as_deref());
                let config = parse_u256(rec.pool_subtype.as_deref());
                if let (Some(token0), Some(token1), Some(config)) = (token0, token1, config) {
                    let key = EkuboPoolKey::from_raw(token0, token1, config);
                    amms.push(EkuboPool::new(addr, key).into());
                    true
                } else {
                    false
                }
            }
            "Sky" => {
                let converter_type = match rec.pool_subtype.as_deref().unwrap_or("DaiUsds") {
                    "LitePsm" | "lite_psm" => SkyConverterType::LitePsm,
                    "LitePsmWrapper" | "lite_psm_wrapper" => SkyConverterType::LitePsmWrapper,
                    _ => SkyConverterType::DaiUsds,
                };
                amms.push(SkyConverter::new(addr, converter_type).into());
                true
            }
            _ => false,
        };

        if add_ok {
            *by_type.entry(pool_type.to_string()).or_insert(0) += 1;
        } else {
            skipped += 1;
        }
    }

    println!(
        "[arb-prod-probe] loaded amms: {}, skipped: {}",
        amms.len(),
        skipped
    );
    let mut by_type_vec: Vec<_> = by_type.into_iter().collect();
    by_type_vec.sort_by(|a, b| a.0.cmp(&b.0));
    for (pool_type, count) in by_type_vec {
        println!("[arb-prod-probe]   {} => {}", pool_type, count);
    }

    Ok(amms)
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let rust_log = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| "info,amms::state_space=info,amms=warn".to_string());
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_new(rust_log)
                .unwrap_or_else(|_| EnvFilter::new("info,amms::state_space=info,amms=warn")),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let rpc_http = std::env::var("ARBITRUM_RPC_HTTP")
        .or_else(|_| std::env::var("ARBITRUM_RPC_URL"))
        .unwrap_or_else(|_| DEFAULT_ARBITRUM_RPC_HTTP.to_string());
    let graph_path = resolve_graph_path();
    let run_secs: u64 = std::env::var("RUN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(180);
    let heartbeat_secs: u64 = std::env::var("HEARTBEAT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15);
    let pool_limit: Option<usize> = std::env::var("POOL_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok());
    let init_timeout_secs: u64 = std::env::var("INIT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600);

    println!("=== Arbitrum Production StateSpace Probe ===");
    println!("rpc_http: {}", rpc_http);
    println!("graph_path: {}", graph_path);
    println!(
        "run_secs: {}, heartbeat_secs: {}, pool_limit: {:?}, init_timeout_secs: {}",
        run_secs, heartbeat_secs, pool_limit, init_timeout_secs
    );

    let provider = Arc::new(
        ProviderBuilder::new()
            .connect_http(rpc_http.parse()?)
    );
    let chain_id = provider.get_chain_id().await?;
    println!("[arb-prod-probe] connected chain_id={}", chain_id);
    if chain_id != ARBITRUM_CHAIN_ID {
        return Err(eyre::eyre!(
            "expected Arbitrum chain_id={}, got {}",
            ARBITRUM_CHAIN_ID,
            chain_id
        ));
    }

    let amms = load_amms_from_graph(&graph_path, pool_limit)?;
    if amms.is_empty() {
        return Err(eyre::eyre!("no AMMs loaded from graph"));
    }

    let init_start = Instant::now();
    let manager = tokio::time::timeout(
        Duration::from_secs(init_timeout_secs),
        StateSpaceBuilder::new(provider.clone())
            .with_amms(amms)
            .with_rate_sync_interval(Duration::from_secs(300))
            .with_curve_sync_interval(Duration::from_secs(120))
            .with_maintenance_interval(Duration::from_secs(366))
            .sync(),
    )
    .await
    .map_err(|_| eyre::eyre!("state space initial sync timed out"))?
    .context("state space initial sync failed")?;
    let init_ms = init_start.elapsed().as_millis();

    println!(
        "[arb-prod-probe] init done: init_ms={} realtime_head={} canonical_head={} reconcile_cursor={}",
        init_ms,
        manager.realtime_head.load(Ordering::Relaxed),
        manager.canonical_head.load(Ordering::Relaxed),
        manager.reconcile_cursor.load(Ordering::Relaxed),
    );

    let mut stream = manager
        .subscribe()
        .await
        .context("subscribe stream init failed")?;

    let started = Instant::now();
    let deadline = started + Duration::from_secs(run_secs);
    let mut next_heartbeat = started + Duration::from_secs(heartbeat_secs);

    let mut updates_total = 0usize;
    let mut affected_total = 0usize;
    let mut stream_errors = 0usize;
    let mut last_rpc_head: u64 = 0;

    while Instant::now() < deadline {
        let now = Instant::now();
        if now >= next_heartbeat {
            let realtime = manager.realtime_head.load(Ordering::Relaxed);
            let canonical = manager.canonical_head.load(Ordering::Relaxed);
            let reconcile = manager.reconcile_cursor.load(Ordering::Relaxed);
            let head_lag = realtime.saturating_sub(canonical);
            let reconcile_lag = canonical.saturating_sub(reconcile);
            let rpc_head = provider.get_block_number().await.unwrap_or(last_rpc_head);
            last_rpc_head = rpc_head;
            println!(
                "[heartbeat] realtime={} canonical={} reconcile={} rpc_head={} realtime-canonical={} canonical-reconcile={} updates={} affected={} stream_errors={}",
                realtime,
                canonical,
                reconcile,
                rpc_head,
                head_lag,
                reconcile_lag,
                updates_total,
                affected_total,
                stream_errors
            );
            next_heartbeat += Duration::from_secs(heartbeat_secs);
        }

        let timeout = std::cmp::min(Duration::from_secs(2), deadline.saturating_duration_since(now));
        match tokio::time::timeout(timeout, stream.next()).await {
            Ok(Some(Ok(affected))) => {
                updates_total += 1;
                affected_total += affected.len();
            }
            Ok(Some(Err(e))) => {
                stream_errors += 1;
                eprintln!("[arb-prod-probe][WARN] stream error: {}", e);
            }
            Ok(None) => break,
            Err(_) => {}
        }
    }

    let realtime_head = manager.realtime_head.load(Ordering::Relaxed);
    let canonical_head = manager.canonical_head.load(Ordering::Relaxed);
    let reconcile_cursor = manager.reconcile_cursor.load(Ordering::Relaxed);
    let rpc_head = provider.get_block_number().await.unwrap_or(last_rpc_head);

    let state = manager.state.read().await;
    let mut max_pool_lag_blocks = 0u64;
    let mut lag_gt_3_blocks = 0usize;
    let mut lag_gt_30_blocks = 0usize;
    let mut lag_gt_300_blocks = 0usize;
    for amm in state.state.values() {
        let lag = canonical_head.saturating_sub(amm.last_synced_block());
        max_pool_lag_blocks = max_pool_lag_blocks.max(lag);
        if lag > 3 {
            lag_gt_3_blocks += 1;
        }
        if lag > 30 {
            lag_gt_30_blocks += 1;
        }
        if lag > 300 {
            lag_gt_300_blocks += 1;
        }
    }

    println!("\n=== Arbitrum Probe Summary ===");
    println!("init_ms: {}", init_ms);
    println!("runtime_secs: {}", run_secs);
    println!("pools_total: {}", state.state.len());
    println!("updates_total: {}", updates_total);
    println!("affected_total: {}", affected_total);
    println!("stream_errors: {}", stream_errors);
    println!("realtime_head: {}", realtime_head);
    println!("canonical_head: {}", canonical_head);
    println!("reconcile_cursor: {}", reconcile_cursor);
    println!("rpc_head: {}", rpc_head);
    println!(
        "realtime_minus_canonical: {}",
        realtime_head.saturating_sub(canonical_head)
    );
    println!(
        "rpc_minus_canonical: {}",
        rpc_head.saturating_sub(canonical_head)
    );
    println!(
        "canonical_minus_reconcile: {}",
        canonical_head.saturating_sub(reconcile_cursor)
    );
    println!("max_pool_lag_blocks: {}", max_pool_lag_blocks);
    println!("lag_gt_3_blocks: {}", lag_gt_3_blocks);
    println!("lag_gt_30_blocks: {}", lag_gt_30_blocks);
    println!("lag_gt_300_blocks: {}", lag_gt_300_blocks);

    Ok(())
}

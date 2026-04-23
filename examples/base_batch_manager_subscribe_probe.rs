use alloy::{
    primitives::{
        aliases::{I24, U24},
        Address, FixedBytes, U256,
    },
    providers::{Provider, ProviderBuilder, WsConnect},
};
use amms::{
    amms::{
        aerodrome_slipstream::AerodromeSlipstreamPool,
        aerodrome_v2::AerodromeV2Pool,
        amm::AMM,
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
    cmp::min,
    collections::HashMap,
    path::Path,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

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

#[derive(Debug)]
struct BatchReport {
    batch_id: usize,
    pools: usize,
    init_ms: u128,
    updates_total: usize,
    affected_total: usize,
    stream_errors: usize,
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
        "/Users/d-lyw/D-lyw/dex-arbitrage/configs/8453_graph.ndjson",
        "configs/8453_graph.ndjson",
    ];
    for p in candidates {
        if Path::new(p).exists() {
            return p.to_string();
        }
    }
    "/Users/d-lyw/D-lyw/dex-arbitrage/configs/8453_graph.ndjson".to_string()
}

fn load_amms_from_graph(path: &str) -> eyre::Result<Vec<AMM>> {
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
        "[batch-probe] loaded amms: {}, skipped: {}",
        amms.len(),
        skipped
    );
    let mut by_type_vec: Vec<_> = by_type.into_iter().collect();
    by_type_vec.sort_by(|a, b| a.0.cmp(&b.0));
    for (t, c) in by_type_vec {
        println!("[batch-probe]   {} => {}", t, c);
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

    let rpc_ws = std::env::var("BASE_RPC_WS")
        .or_else(|_| std::env::var("BASE_FLASHBLOCKS_WS"))
        .or_else(|_| std::env::var("BASE_WS"))
        .unwrap_or_else(|_| {
            "wss://base-mainnet.core.chainstack.com/fc5f8eef2b27bee75a83ca6ab5a02634".to_string()
        });
    let graph_path = resolve_graph_path();
    let batch_size: usize = std::env::var("BATCH_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let batch_count: Option<usize> = std::env::var("BATCH_COUNT")
        .ok()
        .and_then(|v| v.parse().ok());
    let run_secs: u64 = std::env::var("RUN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(420);

    println!("=== Base Batch StateSpaceManager Subscribe Probe ===");
    println!("rpc_ws: {}", rpc_ws);
    println!("graph_path: {}", graph_path);
    println!(
        "batch_size: {}, batch_count: {:?}, run_secs: {}",
        batch_size, batch_count, run_secs
    );

    let provider = Arc::new(
        ProviderBuilder::new()
            .connect_ws(WsConnect::new(rpc_ws.clone()))
            .await
            .with_context(|| format!("failed to connect rpc ws: {rpc_ws}"))?,
    );
    let chain_id = provider.get_chain_id().await?;
    println!("[batch-probe] connected chain_id={}", chain_id);

    let amms = load_amms_from_graph(&graph_path)?;
    if amms.is_empty() {
        return Err(eyre::eyre!("no AMMs loaded from graph"));
    }

    let mut managers = Vec::new();
    for (batch_id, chunk) in amms.chunks(batch_size).enumerate() {
        if let Some(limit) = batch_count {
            if batch_id >= limit {
                break;
            }
        }
        let init_start = Instant::now();
        let manager = StateSpaceBuilder::new(provider.clone())
            .with_amms(chunk.to_vec())
            // align with dex-arbitrage production setup
            .with_non_event_sync_interval(Duration::from_secs(300))
            .with_curve_sync_interval(Duration::from_secs(120))
            .with_pending_sync_worker_interval(Duration::from_secs(2))
            .with_drift_probe_interval(Duration::from_secs(60))
            .with_maintenance_coverage_interval(Duration::from_secs(180))
            .sync()
            .await
            .with_context(|| format!("batch {} init failed", batch_id))?;

        let init_ms = init_start.elapsed().as_millis();
        println!(
            "[batch {}] init pools={} init_ms={}",
            batch_id,
            chunk.len(),
            init_ms
        );
        managers.push((batch_id, chunk.len(), init_ms, manager));
    }

    if managers.is_empty() {
        return Err(eyre::eyre!(
            "no manager initialized (check BATCH_COUNT/BATCH_SIZE)"
        ));
    }

    let mut handles = Vec::new();
    for (batch_id, pools, init_ms, manager) in managers {
        let handle = tokio::spawn(async move {
            let mut stream = manager
                .subscribe()
                .await
                .map_err(|e| eyre::eyre!("batch {} subscribe failed: {}", batch_id, e))?;

            let started = Instant::now();
            let deadline = started + Duration::from_secs(run_secs);
            let mut next_heartbeat = started + Duration::from_secs(15);

            let mut updates_total = 0usize;
            let mut affected_total = 0usize;
            let mut stream_errors = 0usize;

            while Instant::now() < deadline {
                let now = Instant::now();
                if now >= next_heartbeat {
                    println!(
                        "[batch {}] heartbeat updates={} affected={} stream_err={}",
                        batch_id, updates_total, affected_total, stream_errors
                    );
                    next_heartbeat += Duration::from_secs(15);
                }

                let timeout = min(
                    Duration::from_secs(2),
                    deadline.saturating_duration_since(now),
                );
                match tokio::time::timeout(timeout, stream.next()).await {
                    Ok(Some(Ok(affected))) => {
                        updates_total += 1;
                        affected_total += affected.len();
                    }
                    Ok(Some(Err(e))) => {
                        stream_errors += 1;
                        eprintln!("[batch {}][WARN] stream error: {}", batch_id, e);
                    }
                    Ok(None) => break,
                    Err(_) => {}
                }
            }

            Ok::<BatchReport, eyre::Report>(BatchReport {
                batch_id,
                pools,
                init_ms,
                updates_total,
                affected_total,
                stream_errors,
            })
        });
        handles.push(handle);
    }

    let mut reports = Vec::new();
    for handle in handles {
        let report = handle.await??;
        reports.push(report);
    }
    reports.sort_by_key(|r| r.batch_id);

    println!("\n=== Batch Reports ===");
    let mut total_pools = 0usize;
    let mut total_updates = 0usize;
    let mut total_affected = 0usize;
    let mut total_errors = 0usize;

    for r in &reports {
        println!(
            "[batch {}] pools={} init_ms={} updates={} affected={} stream_err={}",
            r.batch_id, r.pools, r.init_ms, r.updates_total, r.affected_total, r.stream_errors
        );
        total_pools += r.pools;
        total_updates += r.updates_total;
        total_affected += r.affected_total;
        total_errors += r.stream_errors;
    }

    println!("\n=== Aggregate Summary ===");
    println!("manager_instances: {}", reports.len());
    println!("total_pools: {}", total_pools);
    println!("total_updates: {}", total_updates);
    println!("total_affected: {}", total_affected);
    println!("total_stream_errors: {}", total_errors);

    Ok(())
}

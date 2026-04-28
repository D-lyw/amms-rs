use alloy::{
    eips::BlockId,
    primitives::{
        aliases::{I24, U24},
        Address, FixedBytes, U256,
    },
    providers::{Provider, ProviderBuilder, WsConnect},
    rpc::types::eth::Log,
    sol,
    sol_types::SolEvent,
};
use amms::{
    amms::{
        aerodrome_slipstream::{
            AerodromeSlipstreamPool, ICLPool, ICLPoolFactory, ICustomFeeModule,
        },
        aerodrome_v2::AerodromeV2Pool,
        amm::{AutomatedMarketMaker, AMM},
        balancer_v2::{self, BalancerV2Pool, BalancerV2PoolType},
        balancer_v3::{self, BalancerV3Pool, BalancerV3PoolType},
        curve_legacy::{CurveLegacyPool, CurveLegacyPoolType},
        curve_ng::{CurveNGPool, CurveNGPoolType},
        ekubo::{self, EkuboPool, EkuboPoolKey},
        fluid_dex::{get_liquidity_layer, FluidDexPool},
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
use futures::stream::FuturesUnordered;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::Message;

const BASE_CHAIN_ID: u64 = 8453;


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

#[derive(Debug, Clone)]
struct LocalLogMatcher {
    topic_addresses: HashSet<Address>,
    topic_signatures: HashSet<FixedBytes<32>>,
    address_only_addresses: HashSet<Address>,
}

impl LocalLogMatcher {
    fn matches(&self, log: &Log) -> bool {
        let address = log.address();

        if self.address_only_addresses.contains(&address) {
            return true;
        }

        if !self.topic_addresses.contains(&address) {
            return false;
        }

        match log.topics().first() {
            Some(topic0) => self.topic_signatures.contains(topic0),
            None => false,
        }
    }
}

#[derive(Debug, Deserialize)]
struct FlashblockMessage {
    payload_id: String,
    index: u64,
    #[serde(default)]
    base: Option<FlashblockBase>,
    #[serde(default)]
    diff: Option<FlashblockDiff>,
    #[serde(default)]
    metadata: Option<FlashblockMetadata>,
}

#[derive(Debug, Deserialize)]
struct FlashblockBase {
    #[serde(default)]
    block_number: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FlashblockDiff {
    #[serde(default)]
    block_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FlashblockMetadata {
    #[serde(default)]
    block_number: Option<u64>,
    #[serde(default)]
    receipts: HashMap<String, FlashblockReceipt>,
}

#[derive(Debug, Deserialize)]
struct FlashblockReceipt {
    #[serde(default, rename = "transactionIndex")]
    transaction_index: Option<String>,
    #[serde(default)]
    logs: Vec<FlashblockLog>,
}

#[derive(Debug, Deserialize)]
struct FlashblockLog {
    address: String,
    #[serde(default)]
    topics: Vec<String>,
    #[serde(default)]
    data: String,
}

#[derive(Default)]
struct ExtractStats {
    total_logs: usize,
    matched_logs: usize,
    decode_fail: usize,
}

fn parse_addr(v: Option<&str>) -> Option<Address> {
    v.and_then(|s| Address::from_str(s).ok())
}

fn parse_u256(v: Option<&str>) -> Option<U256> {
    v.and_then(|s| U256::from_str(s).ok())
}

fn parse_hex_u64(s: &str) -> Option<u64> {
    let raw = s.strip_prefix("0x").unwrap_or(s);
    u64::from_str_radix(raw, 16).ok()
}

fn resolve_graph_path() -> String {
    if let Ok(p) = std::env::var("GRAPH_PATH") {
        return p;
    }

    let candidates = [
        "/Users/d-lyw/D-lyw/dex-arbitrage/configs/8453_graph.ndjson",
        "configs/8453_graph.ndjson",
    ];

    for p in candidates {
        if std::path::Path::new(p).exists() {
            return p.to_string();
        }
    }

    "/Users/d-lyw/D-lyw/dex-arbitrage/configs/8453_graph.ndjson".to_string()
}

fn load_amms_from_graph(path: &str, pool_limit: Option<usize>) -> eyre::Result<Vec<AMM>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read graph file: {path}"))?;

    let skip_pool_types: HashSet<String> = std::env::var("SKIP_POOL_TYPES")
        .ok()
        .map(|v| {
            v.split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();

    let mut amms = Vec::new();
    let mut by_type: HashMap<String, usize> = HashMap::new();
    let mut skipped = 0usize;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let rec: PoolRecord = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                continue;
            }
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

        if skip_pool_types.contains(pool_type) {
            skipped += 1;
            continue;
        }

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
            "UniswapV3" => {
                amms.push(UniswapV3Pool::new(addr).into());
                true
            }
            "SushiswapV3" => {
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

    println!("[probe] loaded amms: {}, skipped: {}", amms.len(), skipped);
    let mut by_type_vec: Vec<_> = by_type.into_iter().collect();
    by_type_vec.sort_by(|a, b| a.0.cmp(&b.0));
    for (t, c) in by_type_vec {
        println!("[probe]   {} => {}", t, c);
    }

    Ok(amms)
}

async fn resolve_slipstream_fee_modules<P, N>(provider: &P, amms: &[AMM]) -> Vec<Address>
where
    P: Provider<N> + Clone,
    N: alloy::network::Network,
{
    let mut fee_modules = std::collections::HashSet::new();
    let mut factory_cache: std::collections::HashMap<Address, Address> = std::collections::HashMap::new();

    for amm in amms {
        let AMM::AerodromeSlipstreamPool(p) = amm else { continue };
        let factory_addr = match ICLPool::new(p.address, provider.clone()).factory().call().await {
            Ok(addr) if addr != Address::ZERO => addr,
            _ => continue,
        };
        let fm_addr = if let Some(&cached) = factory_cache.get(&factory_addr) {
            cached
        } else {
            let fm = ICLPoolFactory::new(factory_addr, provider.clone())
                .swapFeeModule().call().await
                .unwrap_or(Address::ZERO);
            factory_cache.insert(factory_addr, fm);
            fm
        };
        if fm_addr != Address::ZERO {
            fee_modules.insert(fm_addr);
        }
    }
    fee_modules.into_iter().collect()
}

async fn build_local_log_matcher<P, N>(provider: &P, amms: &[AMM], chain_id: u64) -> LocalLogMatcher
where
    P: Provider<N> + Clone,
    N: alloy::network::Network,
{
    let mut topic_addresses = HashSet::new();
    let mut topic_signatures: HashSet<FixedBytes<32>> = HashSet::new();
    let mut address_only_addresses = HashSet::new();
    let mut has_slipstream_pool = false;

    for amm in amms {
        let sync_events = amm.sync_events();
        let has_events = !sync_events.is_empty();

        if has_events {
            for event in sync_events {
                topic_signatures.insert(event);
            }
        }

        match amm {
            AMM::UniswapV4Pool(p) => {
                if has_events {
                    topic_addresses.insert(p.manager_address);
                }
            }
            AMM::PancakeInfinityPool(p) => {
                if has_events {
                    topic_addresses.insert(p.manager_address);
                }
            }
            AMM::FluidDexPool(p) => {
                if has_events {
                    topic_addresses.insert(p.address);
                }
                if let Some(addr) = get_liquidity_layer(chain_id) {
                    topic_addresses.insert(addr);
                }
            }
            AMM::BalancerV2Pool(p) => {
                if has_events {
                    if let Some(vault) = balancer_v2::get_vault_address(chain_id) {
                        topic_addresses.insert(vault);
                    } else {
                        topic_addresses.insert(p.vault_address);
                    }
                }
            }
            AMM::BalancerV3Pool(p) => {
                if has_events {
                    if let Some(vault) = balancer_v3::get_vault_address(chain_id) {
                        topic_addresses.insert(vault);
                    } else {
                        topic_addresses.insert(p.vault_address);
                    }
                }
            }
            AMM::EkuboPool(_) => {
                if let Some(core) = ekubo::get_core_address(chain_id) {
                    address_only_addresses.insert(core);
                }
            }
            AMM::AerodromeSlipstreamPool(_) => {
                has_slipstream_pool = true;
                if has_events {
                    topic_addresses.insert(amm.address());
                }
            }
            _ => {
                if has_events {
                    topic_addresses.insert(amm.address());
                }
            }
        }
    }

    if has_slipstream_pool && chain_id == BASE_CHAIN_ID {
        let fee_modules = resolve_slipstream_fee_modules(provider, amms).await;
        for fm in &fee_modules {
            topic_addresses.insert(*fm);
        }
        if !fee_modules.is_empty() {
            topic_signatures.insert(ICustomFeeModule::CustomFeeSet::SIGNATURE_HASH);
        }
    }

    LocalLogMatcher {
        topic_addresses,
        topic_signatures,
        address_only_addresses,
    }
}

async fn apply_followups<N, P>(
    state: &Arc<tokio::sync::RwLock<amms::state_space::StateSpace>>,
    provider: P,
    block_num: u64,
    needs_resync: Vec<Address>,
    needs_async_update: Vec<Address>,
) -> (usize, usize)
where
    N: alloy::network::Network,
    P: Provider<N> + Clone,
{
    let amms_to_resync: Vec<AMM> = {
        let guard = state.read().await;
        needs_resync
            .iter()
            .filter_map(|addr| guard.state.get(addr).map(|amm| amm.as_ref().clone()))
            .collect()
    };

    let amms_to_update: Vec<AMM> = {
        let guard = state.read().await;
        needs_async_update
            .iter()
            .filter_map(|addr| guard.state.get(addr).map(|amm| amm.as_ref().clone()))
            .collect()
    };

    let mut resynced = 0usize;
    let mut updated = 0usize;

    if !amms_to_resync.is_empty() {
        let mut tasks = FuturesUnordered::new();
        for amm in amms_to_resync {
            let provider = provider.clone();
            tasks.push(async move {
                amm.init::<N, _>(BlockId::Number(block_num.into()), provider)
                    .await
            });
        }

        while let Some(res) = tasks.next().await {
            if let Ok(new_amm) = res {
                state.write().await.insert_amm(new_amm);
                resynced += 1;
            }
        }
    }

    if !amms_to_update.is_empty() {
        let mut tasks = FuturesUnordered::new();
        for mut amm in amms_to_update {
            let provider = provider.clone();
            tasks.push(async move {
                let _ = amm.update::<N, _>(provider).await;
                Ok::<AMM, ()>(amm)
            });
        }

        while let Some(res) = tasks.next().await {
            if let Ok(new_amm) = res {
                state.write().await.insert_amm(new_amm);
                updated += 1;
            }
        }
    }

    (resynced, updated)
}

fn percentile(values: &mut [u128], p: f64) -> Option<u128> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let idx = ((values.len() - 1) as f64 * p).round() as usize;
    values.get(idx).copied()
}

fn extract_logs_from_flashblock(
    fb: &FlashblockMessage,
    matcher: &LocalLogMatcher,
) -> (Vec<Log>, ExtractStats, Option<u64>) {
    let mut out = Vec::new();
    let mut stats = ExtractStats::default();

    let block_number = fb
        .metadata
        .as_ref()
        .and_then(|m| m.block_number)
        .or_else(|| {
            fb.base
                .as_ref()
                .and_then(|b| b.block_number.as_deref())
                .and_then(parse_hex_u64)
        });

    let Some(block_number) = block_number else {
        return (out, stats, None);
    };

    let Some(metadata) = fb.metadata.as_ref() else {
        return (out, stats, Some(block_number));
    };

    let block_number_hex = format!("0x{block_number:x}");

    for (tx_hash, receipt) in &metadata.receipts {
        let tx_index = receipt
            .transaction_index
            .as_deref()
            .and_then(parse_hex_u64)
            .unwrap_or(0);
        let tx_index_hex = format!("0x{tx_index:x}");

        for (log_idx, raw_log) in receipt.logs.iter().enumerate() {
            stats.total_logs += 1;

            let mut log_json = json!({
                "address": raw_log.address,
                "topics": raw_log.topics,
                "data": raw_log.data,
                "blockNumber": block_number_hex,
                "transactionHash": tx_hash,
                "transactionIndex": tx_index_hex,
                "logIndex": format!("0x{log_idx:x}"),
                "removed": false,
            });

            if let Some(block_hash) = fb.diff.as_ref().and_then(|d| d.block_hash.clone()) {
                if let Some(map) = log_json.as_object_mut() {
                    map.insert("blockHash".to_string(), Value::String(block_hash));
                }
            }

            match serde_json::from_value::<Log>(log_json) {
                Ok(log) => {
                    if matcher.matches(&log) {
                        stats.matched_logs += 1;
                        out.push(log);
                    }
                }
                Err(_) => {
                    stats.decode_fail += 1;
                }
            }
        }
    }

    (out, stats, Some(block_number))
}

fn decode_flashblock_message(raw: &[u8]) -> Result<(FlashblockMessage, bool), ()> {
    if let Ok(fb) = serde_json::from_slice::<FlashblockMessage>(raw) {
        return Ok((fb, false));
    }

    let mut decompressed = Vec::new();
    let mut reader = brotli::Decompressor::new(raw, 4096);
    if reader.read_to_end(&mut decompressed).is_err() {
        return Err(());
    }

    match serde_json::from_slice::<FlashblockMessage>(&decompressed) {
        Ok(fb) => Ok((fb, true)),
        Err(_) => Err(()),
    }
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt::init();

    let raw_ws = std::env::var("RAW_FLASHBLOCKS_WS")
        .unwrap_or_else(|_| "wss://mainnet.flashblocks.base.org/ws".to_string());

    let rpc_ws = std::env::var("BASE_RPC_WS")
        .or_else(|_| std::env::var("BASE_FLASHBLOCKS_WS"))
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
    let profile_subscribe = std::env::var("PROFILE_SUBSCRIBE")
        .ok()
        .map(|v| {
            let s = v.to_ascii_lowercase();
            s == "1" || s == "true" || s == "yes" || s == "on"
        })
        .unwrap_or(false);

    println!("=== Base Flashblocks Raw Stream Probe ===");
    println!("raw_ws: {raw_ws}");
    println!("rpc_ws: {rpc_ws}");
    println!("graph_path: {graph_path}");
    println!("run_secs: {run_secs}, max_messages: {:?}", max_messages);

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

    if profile_subscribe {
        println!("[probe] PROFILE_SUBSCRIBE=1: running StateSpaceManager::subscribe() path");
        let mut stream = manager.subscribe().await?;
        let started = Instant::now();
        let deadline = started + Duration::from_secs(run_secs);

        let mut updates_total = 0usize;
        let mut total_affected = 0usize;
        let mut affected_sizes: Vec<u128> = Vec::new();

        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let timeout_dur = std::cmp::min(Duration::from_secs(3), deadline - now);
            match tokio::time::timeout(timeout_dur, stream.next()).await {
                Ok(Some(Ok(affected))) => {
                    updates_total += 1;
                    total_affected += affected.len();
                    affected_sizes.push(affected.len() as u128);
                }
                Ok(Some(Err(e))) => {
                    eprintln!("[probe][WARN] subscribe stream item error: {e}");
                }
                Ok(None) => break,
                Err(_) => continue,
            }
        }

        let elapsed = started.elapsed();
        let elapsed_s = elapsed.as_secs_f64().max(1e-9);
        let mut af50 = affected_sizes.clone();
        let mut af95 = affected_sizes.clone();
        println!("\n=== Subscribe Summary ===");
        println!("elapsed_ms: {}", elapsed.as_millis());
        println!("updates_total: {}", updates_total);
        println!("updates_per_sec: {:.2}", updates_total as f64 / elapsed_s);
        println!("affected_pools_total: {}", total_affected);
        println!(
            "affected_size_p50: {}",
            percentile(&mut af50, 0.5).unwrap_or(0)
        );
        println!(
            "affected_size_p95: {}",
            percentile(&mut af95, 0.95).unwrap_or(0)
        );
        return Ok(());
    }

    let matcher = build_local_log_matcher(&provider, &amms, chain_id).await;
    println!(
        "[probe] local matcher: topic_addresses={} topic_signatures={} address_only_addresses={}",
        matcher.topic_addresses.len(),
        matcher.topic_signatures.len(),
        matcher.address_only_addresses.len()
    );

    let (mut ws_stream, _) = connect_async(raw_ws.clone())
        .await
        .with_context(|| format!("failed to connect raw ws: {raw_ws}"))?;

    let started = Instant::now();
    let deadline = started + Duration::from_secs(run_secs);

    let mut raw_messages = 0usize;
    let mut parsed_messages = 0usize;
    let mut decoded_brotli_messages = 0usize;
    let mut decode_fail_messages = 0usize;
    let mut messages_without_metadata = 0usize;
    let mut messages_without_receipts = 0usize;

    let mut total_logs = 0usize;
    let mut matched_logs = 0usize;
    let mut decode_fail_logs = 0usize;
    let mut dedup_dropped_logs = 0usize;

    let mut sync_batch_count = 0usize;
    let mut trigger_count = 0usize;
    let mut total_affected = 0usize;
    let mut total_resync = 0usize;
    let mut total_async_update = 0usize;
    let mut sync_error_count = 0usize;
    let mut sync_panic_count = 0usize;

    let mut prev_msg_time: Option<Instant> = None;
    let mut msg_intervals_ms: Vec<u128> = Vec::new();
    let mut batch_sizes: Vec<u128> = Vec::new();
    let mut affected_sizes: Vec<u128> = Vec::new();

    let mut seen_logs: HashSet<(String, u64, String, u64)> = HashSet::new();

    loop {
        if Instant::now() >= deadline {
            break;
        }

        if let Some(limit) = max_messages {
            if raw_messages >= limit {
                break;
            }
        }

        let next = tokio::time::timeout(Duration::from_secs(3), ws_stream.next()).await;
        let maybe_message_result = match next {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(message_result) = maybe_message_result else {
            break;
        };

        let message = match message_result {
            Ok(m) => m,
            Err(e) => {
                decode_fail_messages += 1;
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

        let (fb, used_brotli) = match decode_flashblock_message(&payload) {
            Ok(v) => v,
            Err(_) => {
                decode_fail_messages += 1;
                continue;
            }
        };

        parsed_messages += 1;
        if used_brotli {
            decoded_brotli_messages += 1;
        }

        if fb.metadata.is_none() {
            messages_without_metadata += 1;
            continue;
        }

        let receipt_count = fb
            .metadata
            .as_ref()
            .map(|m| m.receipts.len())
            .unwrap_or_default();
        if receipt_count == 0 {
            messages_without_receipts += 1;
            continue;
        }

        let (mut logs, stats, block_number_opt) = extract_logs_from_flashblock(&fb, &matcher);

        total_logs += stats.total_logs;
        matched_logs += stats.matched_logs;
        decode_fail_logs += stats.decode_fail;

        if logs.is_empty() {
            continue;
        }

        logs.retain(|log| {
            let tx_hash = log
                .transaction_hash
                .map(|h| format!("{h:?}"))
                .unwrap_or_else(|| "<none>".to_string());
            let log_index = log.log_index.unwrap_or_default();
            let key = (fb.payload_id.clone(), fb.index, tx_hash, log_index);
            if seen_logs.insert(key) {
                true
            } else {
                dedup_dropped_logs += 1;
                false
            }
        });

        if logs.is_empty() {
            continue;
        }

        sync_batch_count += 1;
        batch_sizes.push(logs.len() as u128);

        let max_block = logs
            .iter()
            .filter_map(|l| l.block_number)
            .max()
            .or(block_number_opt)
            .unwrap_or_else(|| {
                manager
                    .realtime_head
                    .load(std::sync::atomic::Ordering::Relaxed)
            });

        let sync_result = {
            let mut guard = manager.state.write().await;
            catch_unwind(AssertUnwindSafe(|| guard.sync(&logs)))
        };

        let (affected, needs_resync, needs_async_update) = match sync_result {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                sync_error_count += 1;
                eprintln!("[probe][WARN] state.sync error: {e}");
                continue;
            }
            Err(_) => {
                sync_panic_count += 1;
                eprintln!("[probe][WARN] state.sync panicked for one flashblock batch, skipped");
                continue;
            }
        };

        let affected_len = affected.len();
        affected_sizes.push(affected_len as u128);
        total_affected += affected_len;
        if affected_len > 0 {
            trigger_count += 1;
        }

        let (resynced, async_updated) = apply_followups::<alloy::network::Ethereum, _>(
            &manager.state,
            provider.clone(),
            max_block,
            needs_resync,
            needs_async_update,
        )
        .await;
        total_resync += resynced;
        total_async_update += async_updated;

        println!(
            "[fb payload={} idx={}] matched_logs={} affected={} resync={} async_update={} realtime_head={}",
            fb.payload_id,
            fb.index,
            logs.len(),
            affected_len,
            resynced,
            async_updated,
            manager
                .realtime_head
                .load(std::sync::atomic::Ordering::Relaxed)
        );
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
    println!("raw_messages: {}", raw_messages);
    println!("parsed_messages: {}", parsed_messages);
    println!("decoded_brotli_messages: {}", decoded_brotli_messages);
    println!("decode_fail_messages: {}", decode_fail_messages);
    println!("messages_without_metadata: {}", messages_without_metadata);
    println!("messages_without_receipts: {}", messages_without_receipts);
    println!(
        "message_rate_per_sec: {:.2}",
        parsed_messages as f64 / elapsed_s
    );
    println!(
        "message_interval_ms_p50: {}",
        percentile(&mut iv50, 0.5).unwrap_or(0)
    );
    println!(
        "message_interval_ms_p95: {}",
        percentile(&mut iv95, 0.95).unwrap_or(0)
    );

    println!("extracted_logs_total: {}", total_logs);
    println!("matched_logs_total: {}", matched_logs);
    println!("decode_fail_logs: {}", decode_fail_logs);
    println!("dedup_dropped_logs: {}", dedup_dropped_logs);
    println!(
        "matched_logs_per_sec: {:.2}",
        matched_logs as f64 / elapsed_s
    );

    println!("sync_batches_total: {}", sync_batch_count);
    println!("trigger_batches(affected>0): {}", trigger_count);
    println!("trigger_per_sec: {:.2}", trigger_count as f64 / elapsed_s);
    println!(
        "batch_size_p50: {}",
        percentile(&mut bs50, 0.5).unwrap_or(0)
    );
    println!(
        "batch_size_p95: {}",
        percentile(&mut bs95, 0.95).unwrap_or(0)
    );
    println!("affected_pools_total: {}", total_affected);
    println!(
        "affected_size_p50: {}",
        percentile(&mut af50, 0.5).unwrap_or(0)
    );
    println!(
        "affected_size_p95: {}",
        percentile(&mut af95, 0.95).unwrap_or(0)
    );
    println!("resync_total: {}", total_resync);
    println!("async_update_total: {}", total_async_update);
    println!("sync_error_count: {}", sync_error_count);
    println!("sync_panic_count: {}", sync_panic_count);

    Ok(())
}

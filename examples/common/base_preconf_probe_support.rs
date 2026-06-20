use alloy::{
    eips::BlockId,
    primitives::{
        aliases::{I24, U24},
        Address, FixedBytes, U256,
    },
    providers::Provider,
    rpc::types::eth::Log,
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
    state_space::StateSpace,
};
use eyre::Context;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    str::FromStr,
    sync::Arc,
};

pub const BASE_CHAIN_ID: u64 = 8453;

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
pub struct LocalLogMatcher {
    pub topic_addresses: HashSet<Address>,
    pub topic_signatures: HashSet<FixedBytes<32>>,
    pub address_only_addresses: HashSet<Address>,
}

impl LocalLogMatcher {
    pub fn matches(&self, log: &Log) -> bool {
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

fn parse_addr(v: Option<&str>) -> Option<Address> {
    v.and_then(|s| Address::from_str(s).ok())
}

fn parse_u256(v: Option<&str>) -> Option<U256> {
    v.and_then(|s| U256::from_str(s).ok())
}

#[allow(dead_code)]
pub fn parse_bool_env(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .map(|v| {
            let s = v.to_ascii_lowercase();
            s == "1" || s == "true" || s == "yes" || s == "on"
        })
        .unwrap_or(false)
}

pub fn sorted_object_keys(value: Option<&Value>) -> Vec<String> {
    match value.and_then(Value::as_object) {
        Some(map) => {
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort();
            keys
        }
        None => Vec::new(),
    }
}

pub fn joined_keys(keys: &[String]) -> String {
    if keys.is_empty() {
        "<none>".to_string()
    } else {
        keys.join(",")
    }
}

pub fn percentile(values: &mut [u128], p: f64) -> Option<u128> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let idx = ((values.len() - 1) as f64 * p).round() as usize;
    values.get(idx).copied()
}

pub fn resolve_graph_path() -> String {
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

pub fn load_amms_from_graph(path: &str, pool_limit: Option<usize>) -> eyre::Result<Vec<AMM>> {
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
    let mut fee_modules = HashSet::new();
    let mut factory_cache: HashMap<Address, Address> = HashMap::new();

    for amm in amms {
        let AMM::AerodromeSlipstreamPool(p) = amm else {
            continue;
        };
        let factory_addr = match ICLPool::new(p.address, provider.clone())
            .factory()
            .call()
            .await
        {
            Ok(addr) if addr != Address::ZERO => addr,
            _ => continue,
        };
        let fm_addr = if let Some(&cached) = factory_cache.get(&factory_addr) {
            cached
        } else {
            let fm = ICLPoolFactory::new(factory_addr, provider.clone())
                .swapFeeModule()
                .call()
                .await
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

pub async fn build_local_log_matcher<P, N>(
    provider: &P,
    amms: &[AMM],
    chain_id: u64,
) -> LocalLogMatcher
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

pub async fn apply_followups<N, P>(
    state: &Arc<tokio::sync::RwLock<StateSpace>>,
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

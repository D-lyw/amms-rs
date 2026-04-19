use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader},
};

use alloy::{
    eips::BlockId,
    network::Network,
    primitives::Address,
    providers::{Provider, ProviderBuilder, WsConnect},
    rpc::types::Block,
};
use amms::{
    amms::{
        amm::{AutomatedMarketMaker, AMM},
        erc_4626::ERC4626Vault,
        fluid_dex::{FluidDexPool, FLUID_DEX_RESOLVER},
        pancake_v3::PancakeV3Pool,
        sushi_v2::SushiV2Pool,
        uniswap_v2::UniswapV2Pool,
        uniswap_v3::UniswapV3Pool,
    },
    state_space::{StateSpaceBuilder, StateSpaceManager},
};
use futures::StreamExt;

#[derive(serde::Deserialize)]
struct PoolIndexRow {
    address: String,
    dex_type: Option<String>,
}

fn build_amm_from_row(dex_type: &str, addr: Address) -> Option<AMM> {
    match dex_type {
        "uniswap_v3" => Some(UniswapV3Pool::new(addr).into()),
        "pancake_v3" => Some(PancakeV3Pool::new(addr).into()),
        "uniswap_v2" => Some(UniswapV2Pool::new(addr).into()),
        "sushiswap_v2" => Some(SushiV2Pool::new(addr).into()),
        "fluid_dex" => Some(FluidDexPool::new(addr, FLUID_DEX_RESOLVER).into()),
        "erc4626" => Some(ERC4626Vault::new(addr).into()),
        _ => None,
    }
}

fn load_amms_from_pool_index(path: &str, target: usize) -> eyre::Result<Vec<AMM>> {
    // Quota by protocol type: total ~= 20, with broader protocol coverage.
    let quotas: [(&str, usize); 6] = [
        ("uniswap_v3", 6),
        ("uniswap_v2", 4),
        ("sushiswap_v2", 3),
        ("pancake_v3", 3),
        ("fluid_dex", 3),
        ("erc4626", 1),
    ];
    let quota_map: HashMap<&str, usize> = quotas.into_iter().collect();
    let mut used: HashMap<String, usize> = HashMap::new();
    let mut picked_addresses = HashSet::new();
    let mut amms = Vec::with_capacity(target);
    let mut rows = Vec::new();

    let file = File::open(path)?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(row) = serde_json::from_str::<PoolIndexRow>(&line) {
            rows.push(row);
        }
    }

    // Pass 1: fill per-protocol quotas first.
    for row in &rows {
        if amms.len() >= target {
            break;
        }
        let Some(dex_type) = row.dex_type.as_deref() else {
            continue;
        };
        let Some(quota) = quota_map.get(dex_type) else {
            continue;
        };
        if used.get(dex_type).copied().unwrap_or(0) >= *quota {
            continue;
        }

        let Ok(addr) = row.address.parse::<Address>() else {
            continue;
        };
        if !picked_addresses.insert(addr) {
            continue;
        }
        if let Some(amm) = build_amm_from_row(dex_type, addr) {
            amms.push(amm);
            *used.entry(dex_type.to_string()).or_insert(0) += 1;
        }
    }

    // Pass 2: if still not enough, fill from all supported protocols.
    if amms.len() < target {
        for row in &rows {
            if amms.len() >= target {
                break;
            }
            let Some(dex_type) = row.dex_type.as_deref() else {
                continue;
            };
            let Ok(addr) = row.address.parse::<Address>() else {
                continue;
            };
            if !picked_addresses.insert(addr) {
                continue;
            }
            if let Some(amm) = build_amm_from_row(dex_type, addr) {
                amms.push(amm);
                *used.entry(dex_type.to_string()).or_insert(0) += 1;
            }
        }
    }

    println!("[pool-index] selected {} pools from {}", amms.len(), path);
    let mut used_kv: Vec<(String, usize)> = used.into_iter().collect();
    used_kv.sort_by(|a, b| a.0.cmp(&b.0));
    for (k, v) in used_kv {
        println!("[pool-index] {} => {}", k, v);
    }

    Ok(amms)
}

fn diff_summary(local: &AMM, remote: &AMM) -> Option<String> {
    match (local, remote) {
        (AMM::FluidDexPool(l), AMM::FluidDexPool(r)) => {
            if l.token0_real_reserves_1e12 != r.token0_real_reserves_1e12
                || l.token1_real_reserves_1e12 != r.token1_real_reserves_1e12
                || l.token0_imag_reserves_1e12 != r.token0_imag_reserves_1e12
                || l.token1_imag_reserves_1e12 != r.token1_imag_reserves_1e12
                || l.center_price_1e27 != r.center_price_1e27
                || l.fee_1e6 != r.fee_1e6
            {
                Some(format!(
                    "FluidDex mismatch: real0 {} vs {}, real1 {} vs {}, centerPrice {} vs {}",
                    l.token0_real_reserves_1e12,
                    r.token0_real_reserves_1e12,
                    l.token1_real_reserves_1e12,
                    r.token1_real_reserves_1e12,
                    l.center_price_1e27,
                    r.center_price_1e27
                ))
            } else {
                None
            }
        }
        (AMM::BalancerV2Pool(l), AMM::BalancerV2Pool(r)) => {
            if l.swap_fee != r.swap_fee || l.token_list.len() != r.token_list.len() {
                return Some(format!(
                    "BalancerV2 mismatch: swap_fee {} vs {}, token_len {} vs {}",
                    l.swap_fee,
                    r.swap_fee,
                    l.token_list.len(),
                    r.token_list.len()
                ));
            }

            for token in &l.token_list {
                let lb = l.tokens.get(token).map(|t| t.balance);
                let rb = r.tokens.get(token).map(|t| t.balance);
                if lb != rb {
                    return Some(format!(
                        "BalancerV2 token balance mismatch: token {:?}, {:?} vs {:?}",
                        token, lb, rb
                    ));
                }
            }
            None
        }
        (AMM::CurveLegacyPool(l), AMM::CurveLegacyPool(r)) => {
            if l.balances != r.balances
                || l.fee != r.fee
                || l.admin_fee != r.admin_fee
                || l.d != r.d
                || l.price_scale != r.price_scale
            {
                Some(format!(
                    "CurveLegacy mismatch: balances_len {} vs {}, fee {} vs {}",
                    l.balances.len(),
                    r.balances.len(),
                    l.fee,
                    r.fee
                ))
            } else {
                None
            }
        }
        (AMM::AerodromeV2Pool(l), AMM::AerodromeV2Pool(r)) => {
            if l.reserve_0 != r.reserve_0
                || l.reserve_1 != r.reserve_1
                || l.fee != r.fee
                || l.stable != r.stable
            {
                Some(format!(
                    "AerodromeV2 mismatch: reserve0 {} vs {}, reserve1 {} vs {}, fee {} vs {}",
                    l.reserve_0, r.reserve_0, l.reserve_1, r.reserve_1, l.fee, r.fee
                ))
            } else {
                None
            }
        }
        (AMM::AerodromeSlipstreamPool(l), AMM::AerodromeSlipstreamPool(r)) => {
            if l.sqrt_price != r.sqrt_price
                || l.tick != r.tick
                || l.liquidity != r.liquidity
                || l.fee != r.fee
            {
                Some(format!(
                    "AerodromeSlipstream mismatch: sqrt_price {} vs {}, tick {} vs {}, liq {} vs {}",
                    l.sqrt_price, r.sqrt_price, l.tick, r.tick, l.liquidity, r.liquidity
                ))
            } else {
                None
            }
        }
        (AMM::UniswapV3Pool(l), AMM::UniswapV3Pool(r)) => {
            if l.sqrt_price != r.sqrt_price
                || l.tick != r.tick
                || l.liquidity != r.liquidity
                || l.fee != r.fee
            {
                Some(format!(
                    "UniswapV3 mismatch: sqrt_price {} vs {}, tick {} vs {}, liq {} vs {}, fee {} vs {}",
                    l.sqrt_price, r.sqrt_price, l.tick, r.tick, l.liquidity, r.liquidity, l.fee, r.fee
                ))
            } else {
                None
            }
        }
        (AMM::PancakeV3Pool(l), AMM::PancakeV3Pool(r)) => {
            if l.sqrt_price != r.sqrt_price
                || l.tick != r.tick
                || l.liquidity != r.liquidity
                || l.fee != r.fee
            {
                Some(format!(
                    "PancakeV3 mismatch: sqrt_price {} vs {}, tick {} vs {}, liq {} vs {}, fee {} vs {}",
                    l.sqrt_price, r.sqrt_price, l.tick, r.tick, l.liquidity, r.liquidity, l.fee, r.fee
                ))
            } else {
                None
            }
        }
        (AMM::UniswapV2Pool(l), AMM::UniswapV2Pool(r)) => {
            if l.reserve_0 != r.reserve_0 || l.reserve_1 != r.reserve_1 || l.fee != r.fee {
                Some(format!(
                    "UniswapV2 mismatch: reserve0 {} vs {}, reserve1 {} vs {}, fee {} vs {}",
                    l.reserve_0, r.reserve_0, l.reserve_1, r.reserve_1, l.fee, r.fee
                ))
            } else {
                None
            }
        }
        (AMM::SushiV2Pool(l), AMM::SushiV2Pool(r)) => {
            if l.reserve_0 != r.reserve_0 || l.reserve_1 != r.reserve_1 || l.fee != r.fee {
                Some(format!(
                    "SushiV2 mismatch: reserve0 {} vs {}, reserve1 {} vs {}, fee {} vs {}",
                    l.reserve_0, r.reserve_0, l.reserve_1, r.reserve_1, l.fee, r.fee
                ))
            } else {
                None
            }
        }
        (AMM::ERC4626Vault(l), AMM::ERC4626Vault(r)) => {
            if l.asset_reserve != r.asset_reserve
                || l.vault_reserve != r.vault_reserve
                || l.deposit_fee != r.deposit_fee
                || l.withdraw_fee != r.withdraw_fee
            {
                Some(format!(
                    "ERC4626 mismatch: asset_reserve {} vs {}, vault_reserve {} vs {}",
                    l.asset_reserve, r.asset_reserve, l.vault_reserve, r.vault_reserve
                ))
            } else {
                None
            }
        }
        _ => None,
    }
}

async fn audit_once<N, P>(manager: &StateSpaceManager<N, P>, provider: P) -> eyre::Result<()>
where
    N: Network<BlockResponse = Block>,
    P: Provider<N> + Clone + 'static,
{
    let block = manager.realtime_head.load(Ordering::Relaxed);
    if block == 0 {
        println!("[audit] latest block is 0, skip");
        return Ok(());
    }

    let local_amms: Vec<AMM> = {
        let guard = manager.state.read().await;
        guard.state.values().cloned().collect()
    };

    let mut checked = 0usize;
    let mut mismatched = 0usize;

    for local in local_amms {
        let block_id = BlockId::Number(block.into());
        let remote = match local.clone().init::<N, _>(block_id, provider.clone()).await {
            Ok(v) => v,
            Err(e) => {
                println!(
                    "[audit][WARN] init failed pool={:?} block={} err={}",
                    local.address(),
                    block,
                    e
                );
                continue;
            }
        };

        checked += 1;
        if let Some(diff) = diff_summary(&local, &remote) {
            mismatched += 1;
            println!(
                "[audit][DIFF] block={} pool={:?} variant={:?} {}",
                block,
                local.address(),
                local.variant(),
                diff
            );
        }
    }

    println!(
        "[audit][SUMMARY] block={} checked={} mismatched={}",
        block, checked, mismatched
    );

    Ok(())
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();
    dotenv::dotenv().ok();

    let ws = std::env::var("WS_PROVIDER")
        .or_else(|_| std::env::var("ETHEREUM_WS"))
        .or_else(|_| std::env::var("BASE_WS"))
        .map_err(|_| eyre::eyre!("Please set WS_PROVIDER (or ETHEREUM_WS / BASE_WS)"))?;

    let provider = Arc::new(
        ProviderBuilder::new()
            .connect_ws(WsConnect::new(ws))
            .await?,
    );

    let chain_id = provider.get_chain_id().await?;
    let pool_index_path = std::env::var("POOL_INDEX_PATH").unwrap_or_else(|_| {
        "/Users/d-lyw/D-lyw/aave-liquidation/config/pool_index_1.json".to_string()
    });
    let amms = load_amms_from_pool_index(&pool_index_path, 20)?;
    if amms.is_empty() {
        return Err(eyre::eyre!(
            "No supported pools selected from {}",
            pool_index_path
        ));
    }

    println!(
        "[init] chain_id={} pools={} (with_amms + periodic same-block verification)",
        chain_id,
        amms.len()
    );

    let manager = StateSpaceBuilder::new(provider.clone())
        .with_amms(amms)
        .with_rate_sync_interval(Duration::from_secs(500))
        .with_curve_sync_interval(Duration::from_secs(240))
        .with_maintenance_interval(Duration::from_secs(600))
        .sync()
        .await?;

    let mut stream = manager.subscribe().await?;
    tokio::spawn(async move {
        while let Some(item) = stream.next().await {
            match item {
                Ok(addrs) if !addrs.is_empty() => {
                    println!("[subscribe] updated pools: {}", addrs.len());
                }
                Ok(_) => {}
                Err(e) => {
                    println!("[subscribe][ERR] {}", e);
                }
            }
        }
    });

    let mut ticker = tokio::time::interval(Duration::from_secs(120));
    loop {
        ticker.tick().await;
        if let Err(e) = audit_once(&manager, provider.clone()).await {
            println!("[audit][ERR] {}", e);
        }
    }
}

use alloy::{
    eips::BlockId,
    primitives::{address, Address, U256},
    providers::{Provider, ProviderBuilder},
    sol_types::SolValue,
};
use amms::{
    amms::{
        amm::AMM,
        curve_ng::{
            CurveNGPool, CurveNGPoolType, GetCurveNGStableSwapRuntimeDataBatchRequest,
            StableSwapRuntimeData,
        },
    },
    state_space::{RealtimeSyncSource, StateSpaceBuilder},
};
use eyre::{eyre, Result};
use futures::StreamExt;
use std::{
    collections::BTreeMap,
    env,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::time::sleep;

const XLAYER_CHAIN_ID: u64 = 196;
const DEFAULT_POOL: Address = address!("7EC81Ef12057008c0BB6B540127f88f917b4fC6c");

#[derive(Clone, Debug)]
struct StableSnapshot {
    balances: Vec<U256>,
    admin_balances: Vec<U256>,
    amp: U256,
    fee: U256,
    admin_fee: U256,
    offpeg_fee_multiplier: U256,
    rates: Vec<U256>,
}

fn provider_url() -> Result<String> {
    dotenv::dotenv().ok();
    env::var("XLAYER_PROVIDER")
        .or_else(|_| env::var("XLAYER_RPC_URL"))
        .or_else(|_| env::var("OKX_XLAYER_RPC_URL"))
        .map_err(|_| eyre!("set XLAYER_PROVIDER / XLAYER_RPC_URL / OKX_XLAYER_RPC_URL"))
}

fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
}

fn pool_address() -> Result<Address> {
    match env::var("POOL") {
        Ok(v) => Address::from_str(&v).map_err(|e| eyre!("invalid POOL={v}: {e}")),
        Err(_) => Ok(DEFAULT_POOL),
    }
}

fn local_snapshot(pool: &CurveNGPool) -> StableSnapshot {
    StableSnapshot {
        balances: pool.balances.clone(),
        admin_balances: pool.admin_balances.clone(),
        amp: pool.amp.unwrap_or_default(),
        fee: pool.fee,
        admin_fee: pool.admin_fee,
        offpeg_fee_multiplier: pool.offpeg_fee_multiplier,
        rates: pool.rates.clone(),
    }
}

async fn fetch_chain_snapshot<P: Provider + Clone>(
    provider: &P,
    pool: Address,
    n_coins: usize,
    block: u64,
) -> Result<StableSnapshot> {
    let deployer =
        GetCurveNGStableSwapRuntimeDataBatchRequest::deploy_builder(provider.clone(), vec![pool]);
    let res = deployer.call_raw().block(BlockId::from(block)).await?;
    let mut data = <Vec<StableSwapRuntimeData> as SolValue>::abi_decode(&res)?
        .into_iter()
        .filter(|d: &StableSwapRuntimeData| d.balances.len() == n_coins);
    let data = data
        .next()
        .ok_or_else(|| eyre!("runtime batch returned no data for {pool} at block {block}"))?;

    Ok(StableSnapshot {
        balances: data.balances,
        admin_balances: data.adminBalances,
        amp: data.amp,
        fee: data.fee,
        admin_fee: data.adminFee,
        offpeg_fee_multiplier: data.offpegFeeMultiplier,
        rates: data.rates,
    })
}

async fn fetch_chain_snapshot_retry<P: Provider + Clone>(
    provider: &P,
    pool: Address,
    n_coins: usize,
    block: u64,
) -> Result<StableSnapshot> {
    let mut last_err = None;
    for _ in 0..5 {
        match fetch_chain_snapshot(provider, pool, n_coins, block).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                last_err = Some(e);
                sleep(Duration::from_millis(250)).await;
            }
        }
    }
    Err(last_err.unwrap_or_else(|| eyre!("snapshot retry exhausted")))
}

fn first_mismatch(block: u64, local: &StableSnapshot, chain: &StableSnapshot) -> Option<String> {
    if local.balances != chain.balances {
        return Some(format!(
            "block={block} balances local={:?} chain={:?}",
            local.balances, chain.balances
        ));
    }
    if local.admin_balances != chain.admin_balances {
        return Some(format!(
            "block={block} admin_balances local={:?} chain={:?}",
            local.admin_balances, chain.admin_balances
        ));
    }
    if local.amp != chain.amp {
        return Some(format!(
            "block={block} amp local={} chain={}",
            local.amp, chain.amp
        ));
    }
    if local.fee != chain.fee {
        return Some(format!(
            "block={block} fee local={} chain={}",
            local.fee, chain.fee
        ));
    }
    if local.admin_fee != chain.admin_fee {
        return Some(format!(
            "block={block} admin_fee local={} chain={}",
            local.admin_fee, chain.admin_fee
        ));
    }
    if local.offpeg_fee_multiplier != chain.offpeg_fee_multiplier {
        return Some(format!(
            "block={block} offpeg_fee_multiplier local={} chain={}",
            local.offpeg_fee_multiplier, chain.offpeg_fee_multiplier
        ));
    }
    if local.rates != chain.rates {
        return Some(format!(
            "block={block} rates local={:?} chain={:?}",
            local.rates, chain.rates
        ));
    }
    None
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let rpc = provider_url()?;
    let pool = pool_address()?;
    let run_secs = env_u64("RUN_SECS", 300);
    let check_lag_blocks = env_u64("CHECK_LAG_BLOCKS", 2);
    let max_checks = env_usize("MAX_CHECKS", usize::MAX);
    let fail_fast = env::var("FAIL_FAST").map(|v| v != "0").unwrap_or(false);

    let provider = Arc::new(ProviderBuilder::new().connect_http(rpc.parse()?));
    let chain_id = provider.get_chain_id().await?;
    if chain_id != XLAYER_CHAIN_ID {
        return Err(eyre!(
            "expected XLayer chain_id {}, got {}",
            XLAYER_CHAIN_ID,
            chain_id
        ));
    }

    let init_block = provider.get_block_number().await?;
    println!(
        "[realtime-check] pool={pool} init_block={init_block} run_secs={run_secs} check_lag_blocks={check_lag_blocks}"
    );

    let manager = StateSpaceBuilder::new(provider.clone())
        .block(init_block)
        .with_amms(vec![AMM::CurveNGPool(CurveNGPool::new(
            pool,
            CurveNGPoolType::StableSwap,
        ))])
        .with_realtime_source(RealtimeSyncSource::XlayerFlashblocksRaw)
        .sync()
        .await?;

    let n_coins = {
        let guard = manager.state.read().await;
        match guard.get(&pool) {
            Some(AMM::CurveNGPool(p)) => p.n_coins as usize,
            other => return Err(eyre!("expected CurveNGPool in state, got {other:?}")),
        }
    };

    let mut stream = manager.subscribe_with_meta().await?;
    let started = Instant::now();
    let mut pending: BTreeMap<u64, StableSnapshot> = BTreeMap::new();
    let mut checks = 0usize;
    let mut mismatches = 0usize;
    let mut updates = 0usize;

    while started.elapsed() < Duration::from_secs(run_secs) && checks < max_checks {
        let item = match tokio::time::timeout(Duration::from_secs(30), stream.next()).await {
            Ok(Some(Ok(item))) => item,
            Ok(Some(Err(e))) => return Err(e.into()),
            Ok(None) => break,
            Err(_) => {
                println!("[realtime-check] no realtime update for 30s, continuing");
                continue;
            }
        };
        let (meta, affected) = item;
        updates += 1;

        if affected.contains(&pool) {
            let snapshot = {
                let guard = manager.state.read().await;
                match guard.get(&pool) {
                    Some(AMM::CurveNGPool(p)) => local_snapshot(p),
                    other => return Err(eyre!("expected CurveNGPool in state, got {other:?}")),
                }
            };
            pending.insert(meta.block_number, snapshot);
            println!(
                "[realtime-check][update] seq={} block={} source={:?} affected={} pending_blocks={}",
                meta.seq,
                meta.block_number,
                meta.source,
                affected.len(),
                pending.len()
            );
        }

        let ready_until = meta.block_number.saturating_sub(check_lag_blocks);
        let ready: Vec<u64> = pending
            .keys()
            .copied()
            .take_while(|block| *block <= ready_until)
            .collect();

        for block in ready {
            let Some(local) = pending.remove(&block) else {
                continue;
            };
            let chain = fetch_chain_snapshot_retry(&*provider, pool, n_coins, block).await?;
            checks += 1;

            if let Some(reason) = first_mismatch(block, &local, &chain) {
                mismatches += 1;
                eprintln!("[realtime-check][MISMATCH] {reason}");
                if fail_fast {
                    return Err(eyre!(reason));
                }
            } else {
                println!("[realtime-check][OK] block={block} checks={checks}");
            }

            if checks >= max_checks {
                break;
            }
        }
    }

    println!(
        "[realtime-check][summary] updates={updates} checks={checks} mismatches={mismatches} pending_blocks={}",
        pending.len()
    );

    Ok(())
}

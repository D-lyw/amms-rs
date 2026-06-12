use alloy::{
    eips::BlockId,
    primitives::{address, Address, B256, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::{Filter, Log},
    sol_types::SolValue,
};
use amms::amms::{
    amm::{AutomatedMarketMaker, SyncAction},
    curve_ng::{
        CurveNGFactory, CurveNGPool, CurveNGPoolType, GetCurveNGStableSwapRuntimeDataBatchRequest,
        StableSwapRuntimeData,
    },
};
use eyre::{eyre, Result};
use std::{env, sync::Arc};

use crate::common::amounts::sample_amounts;

use super::support::ICurveStablePoolNG;

const XLAYER_CHAIN_ID: u64 = 196;
const POOL: Address = address!("7EC81Ef12057008c0BB6B540127f88f917b4fC6c");

#[derive(Debug, Clone)]
struct StableSnapshot {
    balances: Vec<U256>,
    amp: U256,
    fee: U256,
    admin_fee: U256,
    offpeg_fee_multiplier: U256,
    rates: Vec<U256>,
}

fn xlayer_provider_url() -> Option<String> {
    dotenv::dotenv().ok();
    env::var("XLAYER_PROVIDER")
        .or_else(|_| env::var("XLAYER_RPC_URL"))
        .or_else(|_| env::var("OKX_XLAYER_RPC_URL"))
        .ok()
}

fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

fn sort_logs(logs: &mut [Log]) {
    logs.sort_by_key(|l| {
        (
            l.block_number.unwrap_or_default(),
            l.transaction_index.unwrap_or_default(),
            l.log_index.unwrap_or_default(),
        )
    });
}

fn abs_diff(a: U256, b: U256) -> U256 {
    if a >= b {
        a - b
    } else {
        b - a
    }
}

async fn fetch_logs_chunked<P: Provider + Clone>(
    provider: &P,
    pool: Address,
    event_sigs: Vec<B256>,
    from_block: u64,
    to_block: u64,
    chunk_size: u64,
) -> Result<Vec<Log>> {
    let mut out = Vec::new();
    let mut from = from_block;
    while from <= to_block {
        let to = (from + chunk_size.saturating_sub(1)).min(to_block);
        let mut chunk = provider
            .get_logs(
                &Filter::new()
                    .address(pool)
                    .event_signature(event_sigs.clone())
                    .from_block(from)
                    .to_block(to),
            )
            .await?;
        out.append(&mut chunk);
        if to == to_block {
            break;
        }
        from = to + 1;
    }
    sort_logs(&mut out);
    Ok(out)
}

async fn fetch_snapshot<P: Provider + Clone>(
    provider: &P,
    pool: Address,
    n_coins: usize,
    block: u64,
) -> Result<StableSnapshot> {
    let block_id = BlockId::from(block);
    let deployer = GetCurveNGStableSwapRuntimeDataBatchRequest::deploy_builder(
        provider.clone(),
        vec![pool],
    );
    let res = deployer.call_raw().block(block_id).await?;
    let mut pool_data_list =
        <Vec<StableSwapRuntimeData> as SolValue>::abi_decode(&res)?
            .into_iter()
            .filter(|d: &StableSwapRuntimeData| d.balances.len() == n_coins);

    let data = pool_data_list
        .next()
        .ok_or_else(|| eyre!("batch snapshot returned no data for pool {}", pool))?;

    Ok(StableSnapshot {
        balances: data.balances,
        amp: data.amp,
        fee: data.fee,
        admin_fee: data.adminFee,
        offpeg_fee_multiplier: data.offpegFeeMultiplier,
        rates: data.rates,
    })
}

fn assert_balance_alignment(
    block: u64,
    local: &CurveNGPool,
    chain: &StableSnapshot,
    balance_tol: U256,
) -> Result<()> {
    if local.balances.len() != chain.balances.len() {
        return Err(eyre!(
            "block {} balances len mismatch local={} chain={}",
            block,
            local.balances.len(),
            chain.balances.len()
        ));
    }

    for (idx, (local_balance, chain_balance)) in
        local.balances.iter().zip(chain.balances.iter()).enumerate()
    {
        let diff = abs_diff(*local_balance, *chain_balance);
        if diff > balance_tol {
            return Err(eyre!(
                "block {} balance[{}] drift too large: local={} chain={} diff={} tol={}",
                block,
                idx,
                local_balance,
                chain_balance,
                diff,
                balance_tol
            ));
        }
    }

    Ok(())
}

fn assert_full_alignment(block: u64, local: &CurveNGPool, chain: &StableSnapshot) -> Result<()> {
    assert_eq!(
        local.balances, chain.balances,
        "block {} balances mismatch after reinit",
        block
    );
    assert_eq!(
        local.amp.unwrap_or_default(),
        chain.amp,
        "block {} amp mismatch after reinit",
        block
    );
    assert_eq!(
        local.fee, chain.fee,
        "block {} fee mismatch after reinit",
        block
    );
    assert_eq!(
        local.admin_fee, chain.admin_fee,
        "block {} admin_fee mismatch after reinit",
        block
    );
    assert_eq!(
        local.offpeg_fee_multiplier, chain.offpeg_fee_multiplier,
        "block {} offpeg mismatch after reinit",
        block
    );
    assert_eq!(
        local.rates, chain.rates,
        "block {} stored_rates mismatch after reinit",
        block
    );
    Ok(())
}

#[tokio::test]
async fn test_xlayer_curve_ng_stableswap_swap_parity() -> Result<()> {
    let rpc_url = match xlayer_provider_url() {
        Some(url) => url,
        None => {
            println!("Skipping test: XLAYER_PROVIDER / XLAYER_RPC_URL not set");
            return Ok(());
        }
    };

    let provider = Arc::new(ProviderBuilder::new().connect_http(rpc_url.parse()?));
    let chain_id = provider.get_chain_id().await?;
    if chain_id != XLAYER_CHAIN_ID {
        println!(
            "Skipping test: expected XLayer chain_id {}, got {}",
            XLAYER_CHAIN_ID, chain_id
        );
        return Ok(());
    }

    let block = env::var("CURVE_NG_XLAYER_SWAP_BLOCK")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(provider.get_block_number().await?);
    let block_id = BlockId::from(block);

    let pool = CurveNGPool::new(POOL, CurveNGPoolType::StableSwap)
        .init(block_id, provider.clone())
        .await?;
    let contract = ICurveStablePoolNG::new(POOL, provider.clone());

    assert!(
        pool.n_coins >= 2,
        "expected at least 2 coins, got {}",
        pool.n_coins
    );

    for i in 0..pool.n_coins as usize {
        for j in 0..pool.n_coins as usize {
            if i == j {
                continue;
            }

            for amount_in in sample_amounts(pool.balances[i], pool.decimals[i]) {
                let local_out = pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;
                let chain_out = contract
                    .get_dy(i as i128, j as i128, amount_in)
                    .block(block_id)
                    .call()
                    .await?;

                assert_eq!(
                    local_out, chain_out,
                    "swap parity mismatch block={} pair {}->{} amount_in={} local={} chain={}",
                    block, i, j, amount_in, local_out, chain_out
                );
            }
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_xlayer_curve_ng_stableswap_event_replay_drift() -> Result<()> {
    let rpc_url = match xlayer_provider_url() {
        Some(url) => url,
        None => {
            println!("Skipping test: XLAYER_PROVIDER / XLAYER_RPC_URL not set");
            return Ok(());
        }
    };

    let provider = Arc::new(ProviderBuilder::new().connect_http(rpc_url.parse()?));
    let chain_id = provider.get_chain_id().await?;
    if chain_id != XLAYER_CHAIN_ID {
        println!(
            "Skipping test: expected XLayer chain_id {}, got {}",
            XLAYER_CHAIN_ID, chain_id
        );
        return Ok(());
    }

    let end_block = provider.get_block_number().await?;
    let block_range = env_u64("CURVE_NG_XLAYER_DRIFT_BLOCK_RANGE", 3_000);
    let start_block = end_block.saturating_sub(block_range);
    let chunk_size = env_u64("CURVE_NG_XLAYER_DRIFT_LOG_CHUNK", 100).clamp(1, 100);
    let checkpoint_interval = env_u64("CURVE_NG_XLAYER_DRIFT_CHECK_INTERVAL", 100).max(1);
    let reinit_interval = env_u64("CURVE_NG_XLAYER_DRIFT_REINIT_INTERVAL", 250).max(1);
    let balance_tol = U256::from(env_u64("CURVE_NG_XLAYER_DRIFT_BALANCE_TOL", 5));

    let mut pool = CurveNGPool::new(POOL, CurveNGPoolType::StableSwap)
        .init(BlockId::from(start_block), provider.clone())
        .await?;

    let init_chain = fetch_snapshot(&*provider, POOL, pool.n_coins as usize, start_block).await?;
    assert_full_alignment(start_block, &pool, &init_chain)?;

    let logs = fetch_logs_chunked(
        &*provider,
        POOL,
        pool.sync_events(),
        start_block + 1,
        end_block,
        chunk_size,
    )
    .await?;

    if logs.is_empty() {
        println!(
            "Skipping drift replay: no logs found for pool {} in window {}..{}",
            POOL, start_block, end_block
        );
        return Ok(());
    }

    let mut last_checkpoint = start_block;
    let mut last_reinit = start_block;
    let mut processed = 0u64;
    let mut async_updates = 0u64;
    let mut resyncs = 0u64;
    let mut update_calls = 0u64;
    let mut current_block_logs: Vec<Log> = Vec::new();

    for log in logs {
        let block = log
            .block_number
            .ok_or_else(|| eyre!("log missing block_number"))?;

        if current_block_logs.is_empty()
            || current_block_logs[0].block_number.unwrap_or_default() == block
        {
            current_block_logs.push(log);
            continue;
        }

        process_block_logs(
            &mut pool,
            provider.clone(),
            &current_block_logs,
            &mut processed,
            &mut async_updates,
            &mut resyncs,
            &mut update_calls,
            &mut last_checkpoint,
            &mut last_reinit,
            checkpoint_interval,
            reinit_interval,
            balance_tol,
        )
        .await?;

        current_block_logs.clear();
        current_block_logs.push(log);
    }

    if !current_block_logs.is_empty() {
        process_block_logs(
            &mut pool,
            provider.clone(),
            &current_block_logs,
            &mut processed,
            &mut async_updates,
            &mut resyncs,
            &mut update_calls,
            &mut last_checkpoint,
            &mut last_reinit,
            checkpoint_interval,
            reinit_interval,
            balance_tol,
        )
        .await?;
    }

    let final_block = current_block_logs
        .last()
        .and_then(|l| l.block_number)
        .unwrap_or(end_block);
    pool = CurveNGPool::new(POOL, CurveNGPoolType::StableSwap)
        .init(BlockId::from(final_block), provider.clone())
        .await?;
    let final_chain = fetch_snapshot(&*provider, POOL, pool.n_coins as usize, final_block).await?;
    assert_full_alignment(final_block, &pool, &final_chain)?;

    // 额外验证: 生产环境完整 update() 路径（含 get_block_number + refresh + update_spot_prices）
    pool.update(provider.clone()).await?;
    update_calls += 1;
    let head = provider.get_block_number().await?;
    let head_chain = fetch_snapshot(&*provider, POOL, pool.n_coins as usize, head).await?;
    assert_balance_alignment(head, &pool, &head_chain, balance_tol)?;

    println!(
        "[XLayer CurveNG Stable] pool={} window={}..{} processed={} async_updates={} resyncs={} update_verified={}",
        POOL, start_block, end_block, processed, async_updates, resyncs, update_calls
    );

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn process_block_logs<P: Provider + Clone + 'static>(
    pool: &mut CurveNGPool,
    provider: Arc<P>,
    logs: &[Log],
    processed: &mut u64,
    async_updates: &mut u64,
    resyncs: &mut u64,
    update_calls: &mut u64,
    last_checkpoint: &mut u64,
    last_reinit: &mut u64,
    checkpoint_interval: u64,
    reinit_interval: u64,
    balance_tol: U256,
) -> Result<()> {
    let block = logs[0]
        .block_number
        .ok_or_else(|| eyre!("log missing block_number"))?;
    let mut needs_resync = false;
    let mut saw_async_update = false;

    for log in logs {
        match pool.sync(log)? {
            SyncAction::None => {}
            SyncAction::AsyncUpdate => {
                *async_updates += 1;
                saw_async_update = true;
            }
            SyncAction::Resync => {
                *resyncs += 1;
                needs_resync = true;
            }
        }
        *processed += 1;
    }

    let chain_at_block = fetch_snapshot(&*provider, POOL, pool.n_coins as usize, block).await?;

    let due_checkpoint = block >= last_checkpoint.saturating_add(checkpoint_interval);
    let due_reinit = block >= last_reinit.saturating_add(reinit_interval);
    if needs_resync || due_checkpoint || due_reinit {
        // Resync / periodic reinit: local state may be intentionally out-of-sync;
        // re-init from chain and verify full alignment afterwards.
        *pool = CurveNGPool::new(POOL, CurveNGPoolType::StableSwap)
            .init(BlockId::from(block), provider.clone())
            .await?;
        assert_full_alignment(block, pool, &chain_at_block)?;
        *last_reinit = block;
    } else if saw_async_update {
        // 第一步: 验证 sync() 对余额的本地追踪没有偏差
        assert_balance_alignment(block, pool, &chain_at_block, balance_tol)?;

        // 第二步: 验证 refresh_runtime_data_batch (锁定到同一 event block)
        // 能正确从链上拉取全量运行时数据，精确恢复池状态
        CurveNGFactory::refresh_runtime_data_batch(
            std::slice::from_mut(pool),
            BlockId::from(block),
            provider.clone(),
        )
        .await?;
        *update_calls += 1;

        // 使用同一个 block 快照做精确对比（zero tolerance → 要求完全一致）
        assert_balance_alignment(block, pool, &chain_at_block, U256::ZERO)?;
        assert_eq!(
            pool.fee, chain_at_block.fee,
            "block {} fee mismatch after refresh", block
        );
        assert_eq!(
            pool.admin_fee, chain_at_block.admin_fee,
            "block {} admin_fee mismatch after refresh", block
        );
        assert_eq!(
            pool.offpeg_fee_multiplier, chain_at_block.offpeg_fee_multiplier,
            "block {} offpeg mismatch after refresh", block
        );
        assert_eq!(
            pool.rates, chain_at_block.rates,
            "block {} rates mismatch after refresh", block
        );
        assert_eq!(
            pool.amp.unwrap_or_default(),
            chain_at_block.amp,
            "block {} amp mismatch after refresh", block
        );

        println!(
            "[XLayer CurveNG Stable] block={} async update verified ({} calls)",
            block,
            *update_calls
        );
    } else {
        // SyncAction::None — verify local state has not silently drifted.
        assert_balance_alignment(block, pool, &chain_at_block, balance_tol)?;
    }

    if due_checkpoint {
        *last_checkpoint = block;
    }

    Ok(())
}

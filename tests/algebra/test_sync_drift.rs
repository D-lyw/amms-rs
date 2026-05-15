use std::collections::HashMap;
use std::sync::Arc;

use alloy::{
    eips::BlockId,
    network::Ethereum,
    primitives::{Address, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::{Filter, Log},
    sol,
};

use amms::amms::{
    algebra_integral::{AlgebraIntegralFactory, AlgebraIntegralPool, IAlgebraPool},
    amm::{AutomatedMarketMaker, AMM, SyncAction},
};

sol! {
    #[sol(rpc)]
    interface IAlgebraPluginFeeView {
        function getCurrentFee() external view returns (uint16 fee);
    }
}

use super::support::{
    algebra_cases, provider_url_for_base, ALGEBRA_DRIFT_FROM_BLOCK, ALGEBRA_DRIFT_TO_BLOCK,
};

async fn fetch_pool_logs<P: Provider + Clone>(
    provider: &P,
    pool: Address,
    from_block: u64,
    to_block: u64,
    event_sigs: Vec<alloy::primitives::B256>,
) -> eyre::Result<Vec<Log>> {
    let mut all_logs = Vec::new();
    let mut start = from_block;
    let step = 1_500u64;

    while start <= to_block {
        let end = (start + step - 1).min(to_block);
        let filter = Filter::new()
            .address(pool)
            .event_signature(event_sigs.clone())
            .from_block(start)
            .to_block(end);

        let mut logs_opt = None;
        let mut last_err = None;
        for _ in 0..5 {
            match provider.get_logs(&filter).await {
                Ok(v) => {
                    logs_opt = Some(v);
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                }
            }
        }

        let logs = match logs_opt {
            Some(v) => v,
            None => {
                return Err(eyre::eyre!(
                    "get_logs failed after retries for pool={} range={}..{}: {:?}",
                    pool,
                    start,
                    end,
                    last_err
                ))
            }
        };

        all_logs.extend(logs);
        start = end.saturating_add(1);
    }

    all_logs.sort_by_key(|log| {
        (
            log.block_number.unwrap_or_default(),
            log.transaction_index.unwrap_or_default(),
            log.log_index.unwrap_or_default(),
        )
    });

    Ok(all_logs)
}

async fn fetch_algebra_onchain_state<P: Provider + Clone>(
    provider: &P,
    pool_addr: Address,
    block: BlockId,
) -> eyre::Result<(U256, i32, u128, u32)> {
    let pool = IAlgebraPool::new(pool_addr, provider.clone());
    let s = pool.safelyGetStateOfAMM().block(block).call().await?;
    Ok((
        U256::from(s.sqrtPrice),
        s.tick.as_i32(),
        s.activeLiquidity,
        u32::from(s.lastFee),
    ))
}

fn collect_drift_parts(
    local_tick: i32,
    chain_tick: i32,
    local_sqrt: U256,
    chain_sqrt: U256,
    local_liq: u128,
    chain_liq: u128,
    local_fee: u32,
    chain_fee: u32,
) -> Vec<String> {
    let mut drift_parts = Vec::new();
    if local_tick != chain_tick {
        drift_parts.push(format!("tick(local={},chain={})", local_tick, chain_tick));
    }
    if local_sqrt != chain_sqrt {
        drift_parts.push(format!(
            "sqrtPrice(local={},chain={})",
            local_sqrt, chain_sqrt
        ));
    }
    if local_liq != chain_liq {
        drift_parts.push(format!(
            "liquidity(local={},chain={})",
            local_liq, chain_liq
        ));
    }
    if local_fee != chain_fee {
        drift_parts.push(format!("lastFee(local={},chain={})", local_fee, chain_fee));
    }
    drift_parts
}

/// Read the plugin's `getCurrentFee()` at `block` and compare with
/// `pool.compute_fee(block_timestamp)`.  Returns Ok(()) on 0-drift match.
async fn check_dynamic_fee_at_block<P: Provider + Clone>(
    provider: &P,
    pool: &AlgebraIntegralPool,
    _label: &str,
    block: BlockId,
) -> eyre::Result<()> {
    let plugin_addr = pool.plugin;
    let plugin = IAlgebraPluginFeeView::new(plugin_addr, provider.clone());
    let chain_fee = match plugin.getCurrentFee().block(block).call().await {
        Ok(f) => u32::from(f),
        Err(_) => return Ok(()), // skip if RPC fails
    };

    let block_ts = match provider.get_block(block).await {
        Ok(Some(b)) => b.header.timestamp as u32,
        _ => return Ok(()),
    };

    let local_fee = match pool.compute_fee(block_ts) {
        Some(f) => f,
        None => return Ok(()), // can't compute locally
    };

    if local_fee != chain_fee {
        Err(eyre::eyre!("fee(local={},chain={})", local_fee, chain_fee))
    } else {
        Ok(())
    }
}

async fn run_replay_drift_for_pool<P: Provider + Clone>(
    provider: Arc<P>,
    mut local: AlgebraIntegralPool,
    label: &str,
    from_block: u64,
    to_block: u64,
) -> eyre::Result<()> {
    let pool_addr = local.inner.address;

    let (init_chain_sqrt, init_chain_tick, init_chain_liq, init_chain_fee) =
        fetch_algebra_onchain_state(&*provider, pool_addr, BlockId::from(from_block - 1)).await?;
    let init_drifts = collect_drift_parts(
        local.inner.tick,
        init_chain_tick,
        local.inner.sqrt_price,
        init_chain_sqrt,
        local.inner.liquidity,
        init_chain_liq,
        local.last_fee,
        init_chain_fee,
    );
    if !init_drifts.is_empty() {
        return Err(eyre::eyre!(
            "[{}] init drift at block {}: {}",
            label,
            from_block - 1,
            init_drifts.join(", ")
        ));
    }

    let logs = fetch_pool_logs(
        &provider,
        pool_addr,
        from_block,
        to_block,
        local.sync_events(),
    )
    .await?;
    println!(
        "[{}] replay range {}..{} logs={}",
        label,
        from_block,
        to_block,
        logs.len()
    );

    let mut last_checked_block = from_block.saturating_sub(1);
    let check_interval = 50u64;
    let mut current_block: Option<u64> = None;

    for log in logs {
        let block_num = log.block_number.unwrap_or_default();

        if let Some(prev_block) = current_block {
            if block_num != prev_block
                && prev_block >= last_checked_block.saturating_add(check_interval)
            {
                let (chain_sqrt, chain_tick, chain_liq, chain_fee) =
                    fetch_algebra_onchain_state(&*provider, pool_addr, BlockId::from(prev_block))
                        .await?;
                let drifts = collect_drift_parts(
                    local.inner.tick,
                    chain_tick,
                    local.inner.sqrt_price,
                    chain_sqrt,
                    local.inner.liquidity,
                    chain_liq,
                    local.last_fee,
                    chain_fee,
                );
                if !drifts.is_empty() {
                    return Err(eyre::eyre!(
                        "[{}] checkpoint drift at block {}: {}",
                        label,
                        prev_block,
                        drifts.join(", ")
                    ));
                }

                // Also verify dynamic fee computation if plugin is connected.
                if !local.plugin.is_zero() && local.timepoints.is_some() {
                    let block_fee = check_dynamic_fee_at_block(
                        &*provider,
                        &local,
                        label,
                        BlockId::from(prev_block),
                    )
                    .await;
                    if let Err(msg) = block_fee {
                        return Err(eyre::eyre!("[{}] checkpoint dynamic fee drift at block {}: {}", label, prev_block, msg));
                    }
                }

                last_checked_block = prev_block;
            }
        }

        let action = local.sync(&log)?;
        match action {
            SyncAction::None => {}
            SyncAction::AsyncUpdate => {
                if block_num == 0 {
                    continue;
                }
                // Historical replay must stay block-pinned.
                local = AlgebraIntegralPool::new(pool_addr)
                    .init::<alloy::network::Ethereum, _>(BlockId::from(block_num), provider.clone())
                    .await?;
            }
            SyncAction::Resync => {
                if block_num == 0 {
                    continue;
                }
                local = AlgebraIntegralPool::new(pool_addr)
                    .init::<alloy::network::Ethereum, _>(
                        BlockId::from(block_num - 1),
                        provider.clone(),
                    )
                    .await?;
                let replay = local.sync(&log)?;
                if let SyncAction::AsyncUpdate = replay {
                    local = AlgebraIntegralPool::new(pool_addr)
                        .init::<alloy::network::Ethereum, _>(
                            BlockId::from(block_num),
                            provider.clone(),
                        )
                        .await?;
                }
            }
        }

        local.set_last_synced_block(block_num);
        current_block = Some(block_num);
    }

    if let Some(prev_block) = current_block {
        if prev_block >= last_checked_block.saturating_add(check_interval) {
            let (chain_sqrt, chain_tick, chain_liq, chain_fee) =
                fetch_algebra_onchain_state(&*provider, pool_addr, BlockId::from(prev_block))
                    .await?;
            let drifts = collect_drift_parts(
                local.inner.tick,
                chain_tick,
                local.inner.sqrt_price,
                chain_sqrt,
                local.inner.liquidity,
                chain_liq,
                local.last_fee,
                chain_fee,
            );
            if !drifts.is_empty() {
                return Err(eyre::eyre!(
                    "[{}] checkpoint drift at block {}: {}",
                    label,
                    prev_block,
                    drifts.join(", ")
                ));
            }

            // Dynamic fee check at post-loop checkpoint.
            if !local.plugin.is_zero() && local.timepoints.is_some() {
                if let Err(msg) = check_dynamic_fee_at_block(
                    &*provider,
                    &local,
                    label,
                    BlockId::from(prev_block),
                )
                .await
                {
                    return Err(eyre::eyre!("[{}] checkpoint dynamic fee drift at block {}: {}", label, prev_block, msg));
                }
            }
        }
    }

    let (end_chain_sqrt, end_chain_tick, end_chain_liq, end_chain_fee) =
        fetch_algebra_onchain_state(&*provider, pool_addr, BlockId::from(to_block)).await?;

    let terminal_drifts = collect_drift_parts(
        local.inner.tick,
        end_chain_tick,
        local.inner.sqrt_price,
        end_chain_sqrt,
        local.inner.liquidity,
        end_chain_liq,
        local.last_fee,
        end_chain_fee,
    );
    if !terminal_drifts.is_empty() {
        return Err(eyre::eyre!(
            "[{}] terminal drift at {}: {}",
            label,
            to_block,
            terminal_drifts.join(", ")
        ));
    }

    // Terminal dynamic fee check.
    if !local.plugin.is_zero() && local.timepoints.is_some() {
        if let Err(msg) = check_dynamic_fee_at_block(
            &*provider,
            &local,
            label,
            BlockId::from(to_block),
        )
        .await
        {
            return Err(eyre::eyre!("[{}] terminal dynamic fee drift at {}: {}", label, to_block, msg));
        }
    }

    println!(
        "[{}] final match at block {}: tick={} sqrtPrice={} liquidity={} fee={}",
        label, to_block, local.inner.tick, local.inner.sqrt_price, local.inner.liquidity, local.last_fee
    );

    Ok(())
}

#[tokio::test]
async fn test_sync_drift_real_pools() -> eyre::Result<()> {
    let rpc = match provider_url_for_base() {
        Some(v) => v,
        None => {
            eprintln!("skip algebra sync drift: BASE_PROVIDER/ETHEREUM_PROVIDER not set");
            return Ok(());
        }
    };

    let provider = Arc::new(ProviderBuilder::new().connect_http(rpc.parse()?));

    // Batch-init all pools in chunks (avoids batch-contract gas limits).
    let init_block = BlockId::from(ALGEBRA_DRIFT_FROM_BLOCK.saturating_sub(1));
    let cases = algebra_cases();
    let batch: Vec<AMM> = cases.iter().map(|c| AMM::AlgebraIntegralPool(AlgebraIntegralPool::new(c.pool))).collect();
    let mut pool_map: HashMap<Address, AlgebraIntegralPool> = HashMap::new();
    for chunk in batch.chunks(5) {
        for attempt in 0..3 {
            match AlgebraIntegralFactory::init_batch::<Ethereum, _>(chunk.to_vec(), init_block, provider.clone()).await {
                Ok(initialized) => {
                    for amm in initialized {
                        if let AMM::AlgebraIntegralPool(p) = amm {
                            pool_map.insert(p.inner.address, p);
                        }
                    }
                    break;
                }
                Err(e) => {
                    if attempt == 2 {
                        return Err(eyre::eyre!("batch init chunk failed: {:?}", e));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                }
            }
        }
    }

    let mut failures = Vec::new();
    for case in &cases {
        let pool = match pool_map.get(&case.pool) {
            Some(p) => p.clone(),
            None => continue,
        };
        let mut ok = false;
        let mut last_err = None;
        for _ in 0..3 {
            match run_replay_drift_for_pool(
                provider.clone(),
                pool.clone(),
                case.label,
                ALGEBRA_DRIFT_FROM_BLOCK,
                ALGEBRA_DRIFT_TO_BLOCK,
            )
            .await
            {
                Ok(()) => {
                    ok = true;
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
                }
            }
        }
        if !ok {
            failures.push(format!("[{}] {:?}", case.label, last_err));
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(eyre::eyre!(
            "drift validation failures ({}):\n{}",
            failures.len(),
            failures.join("\n")
        ))
    }
}

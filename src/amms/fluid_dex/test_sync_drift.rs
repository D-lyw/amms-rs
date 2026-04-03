use super::{
    decode_liquidity_utilization, decode_price_from_dex_variables, mask, DexReservesResolver,
    FluidDexPool, FluidDexT1, FluidLiquidity, LogOperate, FLUID_DEX_RESOLVER,
    FLUID_LIQUIDITY_LAYER,
};
use crate::amms::amm::AutomatedMarketMaker;
use alloy::{
    eips::BlockId,
    network::Ethereum,
    primitives::{Address, B256, U256},
    providers::Provider,
    rpc::types::{Filter, Log},
    sol_types::SolEvent,
};
use eyre::Result;
use std::{str::FromStr, sync::Arc};

const WSTETH_ETH_POOL: &str = "0x0B1a513ee24972DAEf112bC777a5610d4325C9e7";
const USDC_USDT_POOL: &str = "0x667701e51B4D1Ca244F17C78F7aB8744B4C99F9B";

async fn fetch_pool_events<P: Provider + Clone>(
    provider: &P,
    pool_address: Address,
    from_block: u64,
    to_block: u64,
) -> Result<Vec<Log>> {
    let event_sigs = vec![FluidDexT1::Swap::SIGNATURE_HASH, LogOperate::SIGNATURE_HASH];

    let filter = Filter::new()
        .address(vec![pool_address, FLUID_LIQUIDITY_LAYER])
        .event_signature(event_sigs)
        .from_block(from_block)
        .to_block(to_block);

    let logs = provider.get_logs(&filter).await?;
    Ok(logs)
}

async fn fetch_onchain_state<P: Provider + Clone>(
    provider: &P,
    pool_address: Address,
    resolver_address: Address,
    block: BlockId,
) -> Result<(U256, U256, U256, U256)> {
    let resolver = DexReservesResolver::new(resolver_address, provider.clone());
    let res = resolver
        .getPoolReservesAdjusted(pool_address)
        .block(block)
        .call()
        .await?;

    let real_0 = res.collateralReserves.token0RealReserves + res.debtReserves.token0RealReserves;
    let real_1 = res.collateralReserves.token1RealReserves + res.debtReserves.token1RealReserves;
    let imag_0 =
        res.collateralReserves.token0ImaginaryReserves + res.debtReserves.token0ImaginaryReserves;
    let imag_1 =
        res.collateralReserves.token1ImaginaryReserves + res.debtReserves.token1ImaginaryReserves;

    Ok((real_0, real_1, imag_0, imag_1))
}

async fn find_first_active_block<P: Provider + Clone>(
    provider: &P,
    pool_address: Address,
    resolver_address: Address,
    start_block: u64,
    end_block: u64,
    step: u64,
) -> Result<Option<u64>> {
    if start_block >= end_block {
        return Ok(Some(end_block));
    }
    let resolver = DexReservesResolver::new(resolver_address, provider.clone());
    let mut block = start_block;
    while block <= end_block {
        let res = resolver
            .getPoolReservesAdjusted(pool_address)
            .block(BlockId::from(block))
            .call()
            .await;
        if res.is_ok() {
            return Ok(Some(block));
        }
        block = block.saturating_add(step.max(1));
    }
    let res = resolver
        .getPoolReservesAdjusted(pool_address)
        .block(BlockId::from(end_block))
        .call()
        .await;
    if res.is_ok() {
        return Ok(Some(end_block));
    }
    Ok(None)
}

async fn fetch_block_timestamp<P: Provider + Clone>(provider: &P, block: BlockId) -> u64 {
    if let Ok(Some(b)) = provider.get_block(block).await {
        return b.header.timestamp;
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn refresh_pool_from_resolver<P: Provider + Clone>(
    pool: &mut FluidDexPool,
    provider: &P,
    resolver_address: Address,
    block: BlockId,
) -> Result<()> {
    let resolver = DexReservesResolver::new(resolver_address, provider.clone());
    let pr = resolver
        .getPoolReservesAdjusted(pool.address)
        .block(block)
        .call()
        .await?;

    pool.center_price_1e27 = pr.centerPrice;

    pool.token0_real_reserves_1e12 =
        pr.collateralReserves.token0RealReserves + pr.debtReserves.token0RealReserves;
    pool.token1_real_reserves_1e12 =
        pr.collateralReserves.token1RealReserves + pr.debtReserves.token1RealReserves;
    pool.token0_imag_reserves_1e12 =
        pr.collateralReserves.token0ImaginaryReserves + pr.debtReserves.token0ImaginaryReserves;
    pool.token1_imag_reserves_1e12 =
        pr.collateralReserves.token1ImaginaryReserves + pr.debtReserves.token1ImaginaryReserves;

    pool.col_token0_real_1e12 = pr.collateralReserves.token0RealReserves;
    pool.col_token1_real_1e12 = pr.collateralReserves.token1RealReserves;
    pool.col_token0_imag_1e12 = pr.collateralReserves.token0ImaginaryReserves;
    pool.col_token1_imag_1e12 = pr.collateralReserves.token1ImaginaryReserves;

    pool.debt_token0_real_1e12 = pr.debtReserves.token0RealReserves;
    pool.debt_token1_real_1e12 = pr.debtReserves.token1RealReserves;
    pool.debt_token0_imag_1e12 = pr.debtReserves.token0ImaginaryReserves;
    pool.debt_token1_imag_1e12 = pr.debtReserves.token1ImaginaryReserves;

    pool.withdrawable_token0 = super::TokenLimitData {
        available: pr.limits.withdrawableToken0.available,
        expands_to: pr.limits.withdrawableToken0.expandsTo,
        expand_duration: pr.limits.withdrawableToken0.expandDuration.to::<u64>(),
    };
    pool.withdrawable_token1 = super::TokenLimitData {
        available: pr.limits.withdrawableToken1.available,
        expands_to: pr.limits.withdrawableToken1.expandsTo,
        expand_duration: pr.limits.withdrawableToken1.expandDuration.to::<u64>(),
    };
    pool.borrowable_token0 = super::TokenLimitData {
        available: pr.limits.borrowableToken0.available,
        expands_to: pr.limits.borrowableToken0.expandsTo,
        expand_duration: pr.limits.borrowableToken0.expandDuration.to::<u64>(),
    };
    pool.borrowable_token1 = super::TokenLimitData {
        available: pr.limits.borrowableToken1.available,
        expands_to: pr.limits.borrowableToken1.expandsTo,
        expand_duration: pr.limits.borrowableToken1.expandDuration.to::<u64>(),
    };

    let dex = FluidDexT1::new(pool.address, provider.clone());
    let dex_variables = dex
        .readFromStorage(B256::from(U256::from(0u64)))
        .block(block)
        .call()
        .await
        .unwrap_or(U256::ZERO);
    let dex_variables2 = dex
        .readFromStorage(B256::from(U256::from(1u64)))
        .block(block)
        .call()
        .await
        .unwrap_or(U256::ZERO);
    pool.range_shift = dex
        .readFromStorage(B256::from(U256::from(7u64)))
        .block(block)
        .call()
        .await
        .unwrap_or(U256::ZERO);
    pool.threshold_shift = dex
        .readFromStorage(B256::from(U256::from(8u64)))
        .block(block)
        .call()
        .await
        .unwrap_or(U256::ZERO);
    pool.center_price_shift = dex
        .readFromStorage(B256::from(U256::from(9u64)))
        .block(block)
        .call()
        .await
        .unwrap_or(U256::ZERO);

    let fee_1e4 = u32::try_from((dex_variables2 >> 2u32) & mask(17)).unwrap_or(0);
    let revenue_cut_percent: U256 = (dex_variables2 >> 19u32) & mask(7);
    let revenue_cut = U256::from(100_000_000u64)
        .saturating_sub(revenue_cut_percent.saturating_mul(U256::from(fee_1e4)));
    pool.fee_1e6 = fee_1e4;
    pool.revenue_cut_1e8 = if revenue_cut.is_zero() {
        U256::from(100_000_000u64)
    } else {
        revenue_cut
    };
    pool.is_swap_paused = ((dex_variables2 >> 255) & U256::ONE) == U256::ONE;
    pool.is_smart_collateral_enabled = (dex_variables2 & U256::ONE) == U256::ONE;
    pool.is_smart_debt_enabled = ((dex_variables2 >> 1) & U256::ONE) == U256::ONE;
    pool.utilization_limit_token0 = (dex_variables2 >> 228u32) & mask(10);
    pool.utilization_limit_token1 = (dex_variables2 >> 238u32) & mask(10);
    pool.older_price_1e27 = decode_price_from_dex_variables(dex_variables, 1);
    pool.last_stored_price_1e27 = decode_price_from_dex_variables(dex_variables, 41);
    pool.last_center_price_1e27 = decode_price_from_dex_variables(dex_variables, 81);
    pool.last_swap_timestamp = ((dex_variables >> 121u32) & mask(33)).to::<u64>();
    pool.last_synced_block_timestamp = fetch_block_timestamp(provider, block).await;
    let _ = pool
        .update_center_price_from_chain::<Ethereum, _>(
            dex_variables,
            dex_variables2,
            provider.clone(),
            block,
            pool.last_synced_block_timestamp,
        )
        .await;
    pool.compute_ranges_from_dex(
        dex_variables,
        dex_variables2,
        pool.last_synced_block_timestamp,
    );

    if !pool.liquidity_address.is_zero() {
        let liquidity = FluidLiquidity::new(pool.liquidity_address, provider.clone());
        let exchange_price_token0 = liquidity
            .readFromStorage(pool.exchange_price_token0_slot)
            .block(block)
            .call()
            .await
            .unwrap_or(U256::ZERO);
        let exchange_price_token1 = liquidity
            .readFromStorage(pool.exchange_price_token1_slot)
            .block(block)
            .call()
            .await
            .unwrap_or(U256::ZERO);
        pool.token0_utilization = decode_liquidity_utilization(exchange_price_token0);
        pool.token1_utilization = decode_liquidity_utilization(exchange_price_token1);
    }

    pool.limits_sync_time = pool.last_synced_block_timestamp;
    pool.refresh_prices();

    Ok(())
}

async fn run_sync_drift_test(pool_address: Address, label: &str) -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    dotenv::dotenv().ok();
    let rpc_endpoint = match std::env::var("ETHEREUM_PROVIDER") {
        Ok(u) => u,
        Err(_) => {
            println!("Skipping test: ETHEREUM_PROVIDER not set");
            return Ok(());
        }
    };

    let resolver_address = FLUID_DEX_RESOLVER;
    let provider = Arc::new(
        alloy::providers::ProviderBuilder::new().connect_http(rpc_endpoint.parse().unwrap()),
    );

    let current_block = provider.get_block_number().await?;
    println!("[{label}] Current block: {current_block}");

    let test_block_range = 10000u64;
    let start_block = current_block.saturating_sub(test_block_range);
    let start_block = match find_first_active_block(
        &*provider,
        pool_address,
        resolver_address,
        start_block,
        current_block,
        200,
    )
    .await?
    {
        Some(block) => block,
        None => {
            println!("[{label}] No active block found in range, skipping drift test");
            return Ok(());
        }
    };
    if start_block != current_block.saturating_sub(test_block_range) {
        println!("[{label}] Adjusted start block to {}", start_block);
    }

    let mut pool = FluidDexPool::new(pool_address, resolver_address);
    pool = pool
        .init::<Ethereum, _>(BlockId::from(start_block), provider.clone())
        .await?;
    pool.set_last_synced_block(start_block);

    let (onchain_real_0, onchain_real_1, onchain_imag_0, onchain_imag_1) = fetch_onchain_state(
        &*provider,
        pool_address,
        resolver_address,
        BlockId::from(start_block),
    )
    .await?;

    assert_eq!(
        pool.token0_real_reserves_1e12, onchain_real_0,
        "[{label}] Initial token0_real_reserves mismatch!"
    );
    assert_eq!(
        pool.token1_real_reserves_1e12, onchain_real_1,
        "[{label}] Initial token1_real_reserves mismatch!"
    );
    assert_eq!(
        pool.token0_imag_reserves_1e12, onchain_imag_0,
        "[{label}] Initial token0_imag_reserves mismatch!"
    );
    assert_eq!(
        pool.token1_imag_reserves_1e12, onchain_imag_1,
        "[{label}] Initial token1_imag_reserves mismatch!"
    );

    println!(
        "[{label}] ✅ Initial state verified at block {}",
        start_block
    );

    let mut events =
        fetch_pool_events(&*provider, pool_address, start_block + 1, current_block).await?;

    events.sort_by(|a, b| {
        let a_block = a.block_number.unwrap_or(0);
        let b_block = b.block_number.unwrap_or(0);
        if a_block != b_block {
            a_block.cmp(&b_block)
        } else {
            let a_tx_idx = a.transaction_index.unwrap_or(0);
            let b_tx_idx = b.transaction_index.unwrap_or(0);
            if a_tx_idx != b_tx_idx {
                a_tx_idx.cmp(&b_tx_idx)
            } else {
                let a_log_idx = a.log_index.unwrap_or(0);
                let b_log_idx = b.log_index.unwrap_or(0);
                a_log_idx.cmp(&b_log_idx)
            }
        }
    });

    if events.is_empty() {
        println!("[{label}] No events in range, skipping drift test");
        return Ok(());
    }

    let check_interval = 50;
    let mut last_checked_block = start_block;
    let mut events_processed = 0u64;
    let mut total_checks = 0u64;
    let mut max_drift_pct = 0.0f64;

    for log in &events {
        let block_num = log.block_number.unwrap_or(0);

        match pool.sync(log) {
            Ok(_) => {}
            Err(e) => {
                println!("[{label}] ⚠️ sync error at block {}: {:?}", block_num, e);
                continue;
            }
        }
        pool.set_last_synced_block(block_num);
        events_processed += 1;

        if block_num >= last_checked_block + check_interval {
            let check_block = BlockId::from(block_num);
            let _ =
                refresh_pool_from_resolver(&mut pool, &*provider, resolver_address, check_block)
                    .await;
            match fetch_onchain_state(&*provider, pool_address, resolver_address, check_block).await
            {
                Ok((oc_real_0, oc_real_1, oc_imag_0, oc_imag_1)) => {
                    total_checks += 1;

                    let real_0_matches = pool.token0_real_reserves_1e12 == oc_real_0;
                    let real_1_matches = pool.token1_real_reserves_1e12 == oc_real_1;
                    let imag_0_matches = pool.token0_imag_reserves_1e12 == oc_imag_0;
                    let imag_1_matches = pool.token1_imag_reserves_1e12 == oc_imag_1;

                    let real_0_drift = if !oc_real_0.is_zero() {
                        let local_f = pool
                            .token0_real_reserves_1e12
                            .to_string()
                            .parse::<f64>()
                            .unwrap_or(0.0);
                        let remote_f = oc_real_0.to_string().parse::<f64>().unwrap_or(1.0);
                        ((local_f - remote_f) / remote_f * 100.0).abs()
                    } else {
                        0.0
                    };

                    if real_0_drift > max_drift_pct {
                        max_drift_pct = real_0_drift;
                    }

                    if !real_0_matches || !real_1_matches || !imag_0_matches || !imag_1_matches {
                        println!(
                            "[{label}] ❌ Checkpoint at block {block_num} (after {events_processed} events):"
                        );
                        if !real_0_matches {
                            println!(
                                "  token0_real: local={} vs on-chain={} (drift={:.6}%)",
                                pool.token0_real_reserves_1e12, oc_real_0, real_0_drift
                            );
                        }
                        if !real_1_matches {
                            println!(
                                "  token1_real: local={} vs on-chain={}",
                                pool.token1_real_reserves_1e12, oc_real_1
                            );
                        }
                        if !imag_0_matches {
                            println!(
                                "  token0_imag: local={} vs on-chain={}",
                                pool.token0_imag_reserves_1e12, oc_imag_0
                            );
                        }
                        if !imag_1_matches {
                            println!(
                                "  token1_imag: local={} vs on-chain={}",
                                pool.token1_imag_reserves_1e12, oc_imag_1
                            );
                        }
                    } else {
                        println!(
                            "[{label}] ✅ Checkpoint at block {block_num}: all match (after {events_processed} events)"
                        );
                    }

                    last_checked_block = block_num;
                }
                Err(e) => {
                    println!(
                        "[{label}] ⚠️ Could not fetch on-chain state at block {}: {:?}",
                        block_num, e
                    );
                }
            }
        }
    }

    if let Some(last_log) = events.last() {
        let final_block = last_log.block_number.unwrap_or(current_block);
        let check_block = BlockId::from(final_block);
        let _ =
            refresh_pool_from_resolver(&mut pool, &*provider, resolver_address, check_block).await;
        match fetch_onchain_state(&*provider, pool_address, resolver_address, check_block).await {
            Ok((oc_real_0, oc_real_1, oc_imag_0, oc_imag_1)) => {
                total_checks += 1;

                println!("\n[{label}] === FINAL STATE COMPARISON (block {final_block}) ===");
                println!(
                    "  token0_real: local={} vs on-chain={}  {}",
                    pool.token0_real_reserves_1e12,
                    oc_real_0,
                    if pool.token0_real_reserves_1e12 == oc_real_0 {
                        "✅"
                    } else {
                        "❌"
                    }
                );
                println!(
                    "  token1_real: local={} vs on-chain={}  {}",
                    pool.token1_real_reserves_1e12,
                    oc_real_1,
                    if pool.token1_real_reserves_1e12 == oc_real_1 {
                        "✅"
                    } else {
                        "❌"
                    }
                );
                println!(
                    "  token0_imag: local={} vs on-chain={}  {}",
                    pool.token0_imag_reserves_1e12,
                    oc_imag_0,
                    if pool.token0_imag_reserves_1e12 == oc_imag_0 {
                        "✅"
                    } else {
                        "❌"
                    }
                );
                println!(
                    "  token1_imag: local={} vs on-chain={}  {}",
                    pool.token1_imag_reserves_1e12,
                    oc_imag_1,
                    if pool.token1_imag_reserves_1e12 == oc_imag_1 {
                        "✅"
                    } else {
                        "❌"
                    }
                );

                assert_eq!(
                    pool.token0_real_reserves_1e12, oc_real_0,
                    "[{label}] Final token0_real_reserves mismatch at block {final_block}!"
                );
                assert_eq!(
                    pool.token1_real_reserves_1e12, oc_real_1,
                    "[{label}] Final token1_real_reserves mismatch at block {final_block}!"
                );
                assert_eq!(
                    pool.token0_imag_reserves_1e12, oc_imag_0,
                    "[{label}] Final token0_imag_reserves mismatch at block {final_block}!"
                );
                assert_eq!(
                    pool.token1_imag_reserves_1e12, oc_imag_1,
                    "[{label}] Final token1_imag_reserves mismatch at block {final_block}!"
                );
            }
            Err(e) => {
                println!("[{label}] ⚠️ Final on-chain fetch failed: {:?}", e);
            }
        }
    }

    println!("\n[{label}] === SYNC DRIFT TEST SUMMARY ===");
    println!("  Block range: {} -> {}", start_block, current_block);
    println!("  Events processed: {}", events_processed);
    println!("  Checkpoints verified: {}", total_checks);
    println!("  Max token0_real drift: {:.8}%", max_drift_pct);
    println!("  Test PASSED ✅");

    Ok(())
}

#[tokio::test]
async fn test_fluid_dex_wsteth_eth_sync_drift() -> Result<()> {
    let pool_address = Address::from_str(WSTETH_ETH_POOL)?;
    run_sync_drift_test(pool_address, "wstETH/ETH").await
}

#[tokio::test]
async fn test_fluid_dex_usdc_usdt_sync_drift() -> Result<()> {
    let pool_address = Address::from_str(USDC_USDT_POOL)?;
    run_sync_drift_test(pool_address, "USDC/USDT").await
}

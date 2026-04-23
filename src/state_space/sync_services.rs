use alloy::network::Network;
use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::sol_types::SolValue;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::sleep;
use tracing::{error, info};

use crate::amms::amm::{AutomatedMarketMaker, AMM};
use crate::amms::balancer_v2::BalancerV2Pool;
use crate::amms::balancer_v3::BalancerV3Pool;
use crate::amms::curve_ng::{ICurveNGPool, ICurveNGStableSwap};
use crate::amms::fluid_dex::{
    DexReservesResolver, FluidDexT1, FluidLiquidity, TokenLimitData, FLUID_DEX_RESOLVER,
};
use crate::amms::uniswap_v3::GetUniswapV3PoolStaticMetaBatchRequest;
use crate::state_space::StateSpace;

const STARTUP_JITTER_PCT: u128 = 15;

fn startup_delay_with_jitter(interval: Duration, task_key: &str) -> Duration {
    if interval.is_zero() {
        return Duration::ZERO;
    }

    let base_ms = interval.as_millis();
    if base_ms == 0 {
        return interval;
    }

    // Stable per-task deterministic offset to spread wake-up edges.
    let mut hash = 14695981039346656037u64; // FNV-1a 64-bit offset basis
    for b in task_key.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(1099511628211);
    }

    let jitter_budget_ms = base_ms
        .saturating_mul(STARTUP_JITTER_PCT)
        .checked_div(100)
        .unwrap_or(0);
    if jitter_budget_ms == 0 {
        return interval;
    }

    let jitter_ms = (hash as u128) % (jitter_budget_ms + 1);
    let total_ms = base_ms.saturating_add(jitter_ms).min(u64::MAX as u128) as u64;
    Duration::from_millis(total_ms)
}

fn mask(bits: u32) -> U256 {
    if bits == 256 {
        return U256::MAX;
    }
    (U256::ONE << bits) - U256::ONE
}

fn from_big_number(value: U256) -> U256 {
    let exponent_mask = U256::from(0xFFu64);
    let exponent = (value & exponent_mask).to::<u64>();
    let coefficient = value >> 8;
    coefficient << exponent
}

fn decode_price_from_dex_variables(dex_variables: U256, shift: u32) -> U256 {
    let x40 = U256::MAX >> 216;
    let raw = (dex_variables >> shift) & x40;
    from_big_number(raw)
}

fn decode_liquidity_utilization(exchange_price_word: U256) -> U256 {
    (exchange_price_word >> 30u32) & mask(14)
}

/// Periodically updates rates for Balancer V2 pools that have rate providers.
/// This is necessary because rate changes are not emitted as events.
pub async fn start_balancer_v2_rate_sync_task<N, P>(
    state: Arc<RwLock<StateSpace>>,
    provider: P,
    interval: Duration,
) where
    N: Network,
    P: Provider<N> + Clone + 'static,
{
    let mut next_sleep = startup_delay_with_jitter(interval, "balancer_v2_rate");
    loop {
        sleep(next_sleep).await;
        next_sleep = interval;

        // 1. Collect pools that need update
        // We clone them to release the read lock quickly and to perform async RPC calls without holding the lock
        let mut target_pools: Vec<BalancerV2Pool> = {
            let read_guard = state.read().await;
            read_guard
                .state
                .values()
                .filter_map(|amm| {
                    if let AMM::BalancerV2Pool(pool) = amm.as_ref() {
                        // Check if pool has any token with rate provider
                        if pool.tokens.values().any(|t| t.rate_provider.is_some()) {
                            return Some(pool.clone());
                        }
                    }
                    None
                })
                .collect()
        };

        if target_pools.is_empty() {
            continue;
        }

        info!(
            "Updating rates for {} Balancer V2 pools",
            target_pools.len()
        );

        // 2. Batch update rates (this does RPC calls)
        if let Err(e) =
            BalancerV2Pool::batch_update_rates::<N, P>(&mut target_pools, provider.clone()).await
        {
            error!("Failed to update Balancer V2 rates: {:?}", e);
            continue;
        }

        // 3. Update state
        {
            let mut write_guard = state.write().await;
            for pool in target_pools {
                // Update only if it still exists
                if let Some(existing_amm) = write_guard.get_mut_cow(&pool.address) {
                    if let AMM::BalancerV2Pool(existing_pool) = existing_amm {
                        for (token_addr, token_state) in pool.tokens {
                            if let Some(existing_token_state) =
                                existing_pool.tokens.get_mut(&token_addr)
                            {
                                if let Some(new_rate) = token_state.rate {
                                    existing_token_state.rate = Some(new_rate);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Periodically updates rates for Balancer V3 pools that have rate providers.
pub async fn start_balancer_v3_rate_sync_task<N, P>(
    state: Arc<RwLock<StateSpace>>,
    provider: P,
    interval: Duration,
) where
    N: Network,
    P: Provider<N> + Clone + 'static,
{
    let mut next_sleep = startup_delay_with_jitter(interval, "balancer_v3_rate");
    loop {
        sleep(next_sleep).await;
        next_sleep = interval;

        // 1. Collect pools that need update
        let mut target_pools: Vec<BalancerV3Pool> = {
            let read_guard = state.read().await;
            read_guard
                .state
                .values()
                .filter_map(|amm| {
                    if let AMM::BalancerV3Pool(pool) = amm.as_ref() {
                        // Check if pool has any token with rate provider
                        if pool.tokens.values().any(|t| t.rate_provider.is_some()) {
                            return Some(pool.clone());
                        }
                    }
                    None
                })
                .collect()
        };

        if target_pools.is_empty() {
            continue;
        }

        info!(
            "Updating rates for {} Balancer V3 pools",
            target_pools.len()
        );

        // 2. Batch update rates (this does RPC calls)
        if let Err(e) =
            BalancerV3Pool::batch_update_rates::<N, P>(&mut target_pools, provider.clone()).await
        {
            error!("Failed to update Balancer V3 rates: {:?}", e);
            continue;
        }

        // 3. Update state
        {
            let mut write_guard = state.write().await;
            for pool in target_pools {
                // Update only if it still exists
                if let Some(existing_amm) = write_guard.get_mut_cow(&pool.address) {
                    if let AMM::BalancerV3Pool(existing_pool) = existing_amm {
                        for (token_addr, token_state) in pool.tokens {
                            if let Some(existing_token_state) =
                                existing_pool.tokens.get_mut(&token_addr)
                            {
                                existing_token_state.rate = token_state.rate;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Periodically updates swap fees for all Balancer V3 pools.
/// This is necessary because:
/// 1. swap_fee might fail to fetch during initial pool discovery
/// 2. swap_fee can be updated by governance without emitting events
/// 3. Some pools use dynamic fees that change over time
pub async fn start_balancer_v3_fee_sync_task<N, P>(
    state: Arc<RwLock<StateSpace>>,
    provider: P,
    interval: Duration,
) where
    N: Network,
    P: Provider<N> + Clone + 'static,
{
    let mut next_sleep = startup_delay_with_jitter(interval, "balancer_v3_fee");
    loop {
        sleep(next_sleep).await;
        next_sleep = interval;

        // 1. Collect ALL Balancer V3 pools (not just those with rate providers)
        let mut target_pools: Vec<BalancerV3Pool> = {
            let read_guard = state.read().await;
            read_guard
                .state
                .values()
                .filter_map(|amm| {
                    if let AMM::BalancerV3Pool(pool) = amm.as_ref() {
                        Some(pool.clone())
                    } else {
                        None
                    }
                })
                .collect()
        };

        if target_pools.is_empty() {
            continue;
        }

        info!(
            "Syncing swap_fee for {} Balancer V3 pools",
            target_pools.len()
        );

        // 2. Batch update swap fees
        if let Err(e) =
            BalancerV3Pool::batch_update_swap_fees::<N, P>(&mut target_pools, provider.clone())
                .await
        {
            error!("Failed to update Balancer V3 swap fees: {:?}", e);
            continue;
        }

        // 3. Update state
        {
            let mut write_guard = state.write().await;
            let mut updated_count = 0;
            for pool in target_pools {
                if let Some(existing_amm) = write_guard.get_mut_cow(&pool.address) {
                    if let AMM::BalancerV3Pool(existing_pool) = existing_amm {
                        // Only update if swap_fee actually changed or was zero
                        if existing_pool.swap_fee.is_zero()
                            || existing_pool.swap_fee != pool.swap_fee
                        {
                            if !pool.swap_fee.is_zero() {
                                existing_pool.swap_fee = pool.swap_fee;
                                updated_count += 1;
                            }
                        }
                    }
                }
            }
            if updated_count > 0 {
                info!(
                    "Applied swap_fee updates to {} Balancer V3 pools in state",
                    updated_count
                );
            }
        }
    }
}

/// Periodically refreshes Slipstream pool `fee` from chain.
///
/// We do not rely on `CustomFeeSet` events alone because dynamic fee updates can
/// drift without this event being observed in our pipeline.
pub async fn start_slipstream_fee_sync_task<N, P>(
    state: Arc<RwLock<StateSpace>>,
    provider: P,
    interval: Duration,
) where
    N: Network,
    P: Provider<N> + Clone + 'static,
{
    const BATCH_SIZE: usize = 150;

    let mut next_sleep = startup_delay_with_jitter(interval, "slipstream_fee");
    loop {
        sleep(next_sleep).await;
        next_sleep = interval;

        let target_addresses: Vec<Address> = {
            let read_guard = state.read().await;
            read_guard
                .state
                .values()
                .filter_map(|amm| match amm.as_ref() {
                    AMM::AerodromeSlipstreamPool(pool) => Some(pool.address),
                    _ => None,
                })
                .collect()
        };

        if target_addresses.is_empty() {
            continue;
        }

        let mut fee_map: HashMap<Address, u32> = HashMap::new();
        for chunk in target_addresses.chunks(BATCH_SIZE) {
            let chunk_addrs = chunk.to_vec();
            let return_data = match GetUniswapV3PoolStaticMetaBatchRequest::deploy_builder(
                provider.clone(),
                chunk_addrs.clone(),
            )
            .call_raw()
            .await
            {
                Ok(data) => data,
                Err(e) => {
                    error!("Slipstream fee sync batch RPC failed: {:?}", e);
                    continue;
                }
            };

            let static_data =
                match <Vec<(Address, Address, i32, u32)> as SolValue>::abi_decode(&return_data) {
                    Ok(data) => data,
                    Err(e) => {
                        error!(
                            "Slipstream fee sync decode failed: {:?}, return_data_len={}",
                            e,
                            return_data.len()
                        );
                        continue;
                    }
                };

            for ((_, _, _, fee), addr) in static_data.into_iter().zip(chunk_addrs.into_iter()) {
                fee_map.insert(addr, fee);
            }
        }

        if fee_map.is_empty() {
            continue;
        }

        let mut updated_count = 0usize;
        {
            let mut write_guard = state.write().await;
            for (address, new_fee) in fee_map {
                if let Some(existing_amm) = write_guard.get_mut_cow(&address) {
                    if let AMM::AerodromeSlipstreamPool(pool) = existing_amm {
                        if pool.fee != new_fee {
                            pool.fee = new_fee;
                            updated_count += 1;
                        }
                    }
                }
            }
        }

        if updated_count > 0 {
            info!(
                "Applied fee updates to {} Slipstream pools in state",
                updated_count
            );
        }
    }
}

/// Periodically updates stored_rates, D, and price_scale for Curve pools (NG and Legacy).
///
/// This task handles two main categories of updates:
/// 1. `stored_rates` for StableSwap pools:
///    - Curve NG StableSwap pools (always).
///    - Curve Legacy StableSwap pools (if they involve lending/rebasing tokens).
/// 2. `price_scale` for CryptoSwap pools:
///    - Curve NG and Legacy CryptoSwap pools (TwoCrypto, TriCrypto).
///
/// This is necessary because these values change dynamically (e.g., via interest accumulation or internal oracle updates)
/// without emitting standard Swap events.
pub async fn start_curve_rate_sync_task<N, P>(
    state: Arc<RwLock<StateSpace>>,
    provider: P,
    interval: Duration,
) where
    N: Network,
    P: Provider<N> + Clone + 'static,
{
    let mut next_sleep = startup_delay_with_jitter(interval, "curve_rate");
    loop {
        sleep(next_sleep).await;
        next_sleep = interval;

        // =========================================================================
        // Task A: Sync `stored_rates` for StableSwap Pools
        // =========================================================================

        // A.1 Collect target pools (StableSwap only)
        // We target both NG and Legacy pools because both can have `stored_rates`.
        let stable_targets: Vec<Address> = {
            let read_guard = state.read().await;
            read_guard
                .state
                .values()
                .filter_map(|amm| match amm.as_ref() {
                    AMM::CurveNGPool(pool) => {
                        if pool.pool_type.is_stable() && pool.n_coins > 0 {
                            Some(pool.address)
                        } else {
                            None
                        }
                    }
                    AMM::CurveLegacyPool(pool) => {
                        // Legacy StableSwap pools might use stored_rates (e.g., sBTC, cTokens)
                        if pool.pool_type
                            == crate::amms::curve_legacy::CurveLegacyPoolType::StableSwap
                            && pool.n_coins > 0
                        {
                            Some(pool.address)
                        } else {
                            None
                        }
                    }
                    _ => None,
                })
                .collect()
        };

        if !stable_targets.is_empty() {
            info!(
                "Syncing stored_rates for {} Curve StableSwap pools (NG & Legacy)",
                stable_targets.len()
            );

            let mut updated_count = 0;

            // A.2 Fetch and Update
            for addr in &stable_targets {
                // We use ICurveNGStableSwap interface here because it includes `stored_rates()`.
                // This method signature is compatible with Legacy pools that implement it.
                // We also need ICurveNG for offpeg_fee_multiplier
                let stable_pool = ICurveNGStableSwap::new(*addr, provider.clone());
                let ng_pool = ICurveNGPool::new(*addr, provider.clone());

                // Best-effort fetch. Many Legacy pools (like 3pool) will revert here, which is expected.
                // For NG pools, we try to fetch both stored_rates and offpeg_fee_multiplier
                let rates_res = stable_pool.stored_rates().call().await;
                // Try fetching multiplier (only exists on NG)
                let multiplier_res = ng_pool.offpeg_fee_multiplier().call().await;

                if let Ok(rates) = rates_res {
                    let mut write_guard = state.write().await;
                    if let Some(amm) = write_guard.get_mut_cow(addr) {
                        let success = match amm {
                            AMM::CurveNGPool(pool) => {
                                let mut updated = false;
                                if rates.len() == pool.n_coins as usize {
                                    pool.rates = rates;
                                    updated = true;
                                }

                                // Update multiplier if fetched successfully
                                if let Ok(m) = multiplier_res {
                                    pool.offpeg_fee_multiplier = m;
                                    updated = true;
                                }
                                updated
                            }
                            AMM::CurveLegacyPool(pool) => {
                                if rates.len() == pool.n_coins as usize {
                                    pool.rates = rates;
                                    true
                                } else {
                                    false
                                }
                            }
                            _ => false,
                        };

                        if success {
                            updated_count += 1;
                        }
                    }
                }
            }

            if updated_count > 0 {
                info!(
                    "Updated stored_rates/multiplier for {} Curve pools",
                    updated_count
                );
            }
        }

        // =========================================================================
        // Task B: Sync `price_scale` for CryptoSwap Pools
        // =========================================================================

        // B.1 Collect target pools (CryptoSwap only)
        let crypto_targets: Vec<Address> = {
            let read_guard = state.read().await;
            read_guard
                .state
                .values()
                .filter_map(|amm| match amm.as_ref() {
                    AMM::CurveNGPool(pool) => {
                        if pool.pool_type.is_crypto() && pool.n_coins > 0 {
                            Some(pool.address)
                        } else {
                            None
                        }
                    }
                    AMM::CurveLegacyPool(pool) => {
                        if pool.pool_type
                            == crate::amms::curve_legacy::CurveLegacyPoolType::CryptoSwap
                            && pool.n_coins > 0
                        {
                            Some(pool.address)
                        } else {
                            None
                        }
                    }
                    _ => None,
                })
                .collect()
        };

        if !crypto_targets.is_empty() {
            let mut ps_updated_count = 0;

            // B.2 Fetch and Update
            // Reuse per-pool update(multicall) to preserve variant-specific logic paths.
            for addr in crypto_targets {
                let Some(mut updated_amm) = ({
                    let read_guard = state.read().await;
                    read_guard.get(&addr).cloned()
                }) else {
                    continue;
                };

                if let Err(e) = updated_amm.update::<N, _>(provider.clone()).await {
                    error!(
                        "Curve crypto update(multicall) failed: addr={:?}, err={:?}",
                        addr, e
                    );
                    continue;
                }

                let mut write_guard = state.write().await;
                match (write_guard.get_mut_cow(&addr), updated_amm) {
                    (Some(AMM::CurveNGPool(pool)), AMM::CurveNGPool(mut updated))
                        if pool.pool_type.is_crypto() =>
                    {
                        if let Some(new_price_scale) = updated.price_scale.take() {
                            pool.price_scale = Some(new_price_scale);
                            ps_updated_count += 1;
                        }
                        if let Some(d) = updated.d {
                            pool.d = Some(d);
                        }
                        if updated.balances.len() == pool.n_coins as usize {
                            pool.balances = updated.balances;
                        }
                        // TwoCrypto-specific runtime params refreshed in update(multicall).
                        pool.twocrypto_future_a_gamma_time = updated.twocrypto_future_a_gamma_time;
                        pool.twocrypto_last_timestamp = updated.twocrypto_last_timestamp;
                        pool.spot_prices = updated.spot_prices;
                    }
                    (Some(AMM::CurveLegacyPool(pool)), AMM::CurveLegacyPool(mut updated))
                        if pool.pool_type
                            == crate::amms::curve_legacy::CurveLegacyPoolType::CryptoSwap =>
                    {
                        if let Some(new_price_scale) = updated.price_scale.take() {
                            pool.price_scale = Some(new_price_scale);
                            ps_updated_count += 1;
                        }
                        if let Some(d) = updated.d {
                            pool.d = Some(d);
                        }
                        if updated.balances.len() == pool.n_coins as usize {
                            pool.balances = updated.balances;
                        }
                        pool.spot_prices = updated.spot_prices;
                    }
                    _ => {}
                }
            }

            if ps_updated_count > 0 {
                info!(
                    "Updated price_scale for {} CryptoSwap pools (NG & Legacy)",
                    ps_updated_count
                );
            }
        }
    }
}

/// Periodically updates limits and centerPrice for Fluid DEX pools.
/// This is necessary because:
/// 1. Limits (borrowable/withdrawable) expand over time and are affected by borrow/withdraw operations
/// 2. CenterPrice drifts over time even without events
/// 3. Fluid docs recommend refreshing every 5-10 minutes if no event happened
pub async fn start_fluid_dex_limits_sync_task<N, P>(
    state: Arc<RwLock<StateSpace>>,
    provider: P,
    interval: Duration,
) where
    N: Network,
    P: Provider<N> + Clone + 'static,
{
    let resolver_address = FLUID_DEX_RESOLVER;
    let resolver = DexReservesResolver::new(resolver_address, provider.clone());

    let mut next_sleep = startup_delay_with_jitter(interval, "fluid_limits");
    loop {
        sleep(next_sleep).await;
        next_sleep = interval;

        // 1. Collect Fluid DEX pool addresses that need update
        let target_pools: Vec<Address> = {
            let read_guard = state.read().await;
            read_guard
                .state
                .values()
                .filter_map(|amm| {
                    if let AMM::FluidDexPool(pool) = amm.as_ref() {
                        Some(pool.address)
                    } else {
                        None
                    }
                })
                .collect()
        };

        if target_pools.is_empty() {
            continue;
        }

        info!(
            "Updating limits and centerPrice for {} Fluid DEX pools",
            target_pools.len()
        );

        // 2. Batch fetch updated reserves and limits from resolver
        let pools_reserves = match resolver
            .getPoolsReservesAdjusted(target_pools.clone())
            .call()
            .await
        {
            Ok(res) => res,
            Err(e) => {
                error!("Failed to fetch Fluid DEX reserves: {:?}", e);
                continue;
            }
        };

        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let block_timestamp = current_time;

        // 3. Update state
        let mut updated_count = 0;
        {
            let mut write_guard = state.write().await;
            for pr in pools_reserves {
                if let Some(amm) = write_guard.get_mut_cow(&pr.pool) {
                    if let AMM::FluidDexPool(pool) = amm {
                        // Update center price
                        pool.center_price_1e27 = pr.centerPrice;

                        // Update combined reserves
                        pool.token0_real_reserves_1e12 = pr.collateralReserves.token0RealReserves
                            + pr.debtReserves.token0RealReserves;
                        pool.token1_real_reserves_1e12 = pr.collateralReserves.token1RealReserves
                            + pr.debtReserves.token1RealReserves;
                        pool.token0_imag_reserves_1e12 =
                            pr.collateralReserves.token0ImaginaryReserves
                                + pr.debtReserves.token0ImaginaryReserves;
                        pool.token1_imag_reserves_1e12 =
                            pr.collateralReserves.token1ImaginaryReserves
                                + pr.debtReserves.token1ImaginaryReserves;

                        // Update collateral pool reserves
                        pool.col_token0_real_1e12 = pr.collateralReserves.token0RealReserves;
                        pool.col_token1_real_1e12 = pr.collateralReserves.token1RealReserves;
                        pool.col_token0_imag_1e12 = pr.collateralReserves.token0ImaginaryReserves;
                        pool.col_token1_imag_1e12 = pr.collateralReserves.token1ImaginaryReserves;

                        // Update debt pool reserves
                        pool.debt_token0_real_1e12 = pr.debtReserves.token0RealReserves;
                        pool.debt_token1_real_1e12 = pr.debtReserves.token1RealReserves;
                        pool.debt_token0_imag_1e12 = pr.debtReserves.token0ImaginaryReserves;
                        pool.debt_token1_imag_1e12 = pr.debtReserves.token1ImaginaryReserves;

                        // Update limits
                        pool.withdrawable_token0 = TokenLimitData {
                            available: pr.limits.withdrawableToken0.available,
                            expands_to: pr.limits.withdrawableToken0.expandsTo,
                            expand_duration: pr
                                .limits
                                .withdrawableToken0
                                .expandDuration
                                .to::<u64>(),
                        };
                        pool.withdrawable_token1 = TokenLimitData {
                            available: pr.limits.withdrawableToken1.available,
                            expands_to: pr.limits.withdrawableToken1.expandsTo,
                            expand_duration: pr
                                .limits
                                .withdrawableToken1
                                .expandDuration
                                .to::<u64>(),
                        };
                        pool.borrowable_token0 = TokenLimitData {
                            available: pr.limits.borrowableToken0.available,
                            expands_to: pr.limits.borrowableToken0.expandsTo,
                            expand_duration: pr.limits.borrowableToken0.expandDuration.to::<u64>(),
                        };
                        pool.borrowable_token1 = TokenLimitData {
                            available: pr.limits.borrowableToken1.available,
                            expands_to: pr.limits.borrowableToken1.expandsTo,
                            expand_duration: pr.limits.borrowableToken1.expandDuration.to::<u64>(),
                        };

                        let dex = FluidDexT1::new(pool.address, provider.clone());
                        let dex_variables = dex
                            .readFromStorage(B256::from(U256::from(0u64)))
                            .call()
                            .await
                            .unwrap_or(U256::ZERO);
                        let dex_variables2 = dex
                            .readFromStorage(B256::from(U256::from(1u64)))
                            .call()
                            .await
                            .unwrap_or(U256::ZERO);
                        pool.range_shift = dex
                            .readFromStorage(B256::from(U256::from(7u64)))
                            .call()
                            .await
                            .unwrap_or(U256::ZERO);
                        pool.threshold_shift = dex
                            .readFromStorage(B256::from(U256::from(8u64)))
                            .call()
                            .await
                            .unwrap_or(U256::ZERO);
                        pool.center_price_shift = dex
                            .readFromStorage(B256::from(U256::from(9u64)))
                            .call()
                            .await
                            .unwrap_or(U256::ZERO);
                        let fee_1e4 =
                            u32::try_from((dex_variables2 >> 2u32) & mask(17)).unwrap_or(0);
                        let revenue_cut_percent: U256 = (dex_variables2 >> 19u32) & mask(7);
                        let revenue_cut = U256::from(100_000_000u64).saturating_sub(
                            revenue_cut_percent.saturating_mul(U256::from(fee_1e4)),
                        );
                        pool.fee_1e6 = fee_1e4;
                        pool.revenue_cut_1e8 = if revenue_cut.is_zero() {
                            U256::from(100_000_000u64)
                        } else {
                            revenue_cut
                        };
                        pool.is_swap_paused = ((dex_variables2 >> 255) & U256::ONE) == U256::ONE;
                        pool.is_smart_collateral_enabled =
                            (dex_variables2 & U256::ONE) == U256::ONE;
                        pool.is_smart_debt_enabled =
                            ((dex_variables2 >> 1) & U256::ONE) == U256::ONE;
                        pool.utilization_limit_token0 = (dex_variables2 >> 228u32) & mask(10);
                        pool.utilization_limit_token1 = (dex_variables2 >> 238u32) & mask(10);
                        pool.older_price_1e27 = decode_price_from_dex_variables(dex_variables, 1);
                        pool.last_stored_price_1e27 =
                            decode_price_from_dex_variables(dex_variables, 41);
                        pool.last_center_price_1e27 =
                            decode_price_from_dex_variables(dex_variables, 81);
                        pool.last_swap_timestamp =
                            ((dex_variables >> 121u32) & mask(33)).to::<u64>();
                        pool.last_synced_block_timestamp = block_timestamp;
                        let _ = pool
                            .update_center_price_from_chain::<N, _>(
                                dex_variables,
                                dex_variables2,
                                provider.clone(),
                                alloy::eips::BlockId::latest(),
                                block_timestamp,
                            )
                            .await;
                        pool.compute_ranges_from_dex(
                            dex_variables,
                            dex_variables2,
                            block_timestamp,
                        );

                        if !pool.liquidity_address.is_zero() {
                            let liquidity =
                                FluidLiquidity::new(pool.liquidity_address, provider.clone());
                            let exchange_price_token0 = liquidity
                                .readFromStorage(pool.exchange_price_token0_slot)
                                .call()
                                .await
                                .unwrap_or(U256::ZERO);
                            let exchange_price_token1 = liquidity
                                .readFromStorage(pool.exchange_price_token1_slot)
                                .call()
                                .await
                                .unwrap_or(U256::ZERO);
                            pool.token0_utilization =
                                decode_liquidity_utilization(exchange_price_token0);
                            pool.token1_utilization =
                                decode_liquidity_utilization(exchange_price_token1);
                        }

                        pool.limits_sync_time = current_time;
                        updated_count += 1;
                    }
                }
            }
        }

        if updated_count > 0 {
            info!(
                "Updated limits and centerPrice for {} Fluid DEX pools",
                updated_count
            );
        }
    }
}

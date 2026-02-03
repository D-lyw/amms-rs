use alloy::eips::BlockId;
use alloy::network::Network;
use alloy::primitives::Address;
use alloy::providers::Provider;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::sleep;
use tracing::{error, info};

use crate::amms::amm::{AutomatedMarketMaker, Variant, AMM};
use crate::amms::balancer_v2::BalancerV2Pool;
use crate::amms::balancer_v3::BalancerV3Pool;
use crate::amms::curve_ng::{ICurveNGPool, ICurveNGStableSwap, ICurveTriCrypto, ICurveTwoCrypto};
use crate::amms::factory::Factory;
use crate::amms::fluid_dex::{DexReservesResolver, TokenLimitData};
use crate::state_space::StateSpace;

const FLUID_DEX_RESOLVER: &str = "0xC93876C0EEd99645DD53937b25433e311881A27C";

/// Periodically syncs a subset of AMMs (round-robin) to ensure they are up-to-date with the latest block.
pub async fn start_state_maintenance_task<N, P>(
    state: Arc<RwLock<StateSpace>>,
    _factories: Vec<Factory>,
    provider: P,
    interval: Duration,
) where
    N: Network,
    P: Provider<N> + Clone + 'static,
{
    let batch_size = 50;
    let mut current_index = 0;

    loop {
        sleep(interval).await;

        // 1. Get target block
        let target_block = { state.read().await.latest_block.load(Ordering::Relaxed) };

        if target_block == 0 {
            continue;
        }

        // 2. Select a batch of pools to update
        // We sort addresses to ensure deterministic iteration order across updates
        let mut all_addresses: Vec<Address> =
            { state.read().await.state.keys().cloned().collect() };

        if all_addresses.is_empty() {
            continue;
        }

        all_addresses.sort();

        // Calculate batch range
        if current_index >= all_addresses.len() {
            current_index = 0;
        }
        let end = (current_index + batch_size).min(all_addresses.len());
        let batch_addresses = &all_addresses[current_index..end];

        info!(
            "Starting state maintenance for block {} (Batch: {}/{} pools)",
            target_block,
            batch_addresses.len(),
            all_addresses.len()
        );

        // Group pools by variant to batch requests
        let mut pools_by_variant: HashMap<Variant, Vec<AMM>> = HashMap::new();
        {
            let read_guard = state.read().await;
            for addr in batch_addresses {
                if let Some(amm) = read_guard.state.get(addr) {
                    pools_by_variant
                        .entry(amm.variant())
                        .or_default()
                        .push(amm.clone());
                }
            }
        }

        // 3. Fetch selected pools at target block (Async, No Lock)
        let mut synced_pools = Vec::new();
        let chain_tip = BlockId::from(target_block);

        for (variant, amms) in pools_by_variant {
            let provider = provider.clone();
            let res = variant
                .sync_all_pools::<N, _>(amms, chain_tip, provider)
                .await;

            match res {
                Ok(pools) => synced_pools.extend(pools),
                Err(e) => {
                    error!(
                        "State maintenance failed for variant {:?}: {:?}",
                        variant, e
                    );
                }
            }
        }

        // 4. Commit (Per-pool monotonic)
        {
            let mut write_guard = state.write().await;
            let mut updated = 0usize;
            let mut skipped_newer = 0usize;

            for mut pool in synced_pools {
                let address = pool.address();
                if let Some(existing) = write_guard.state.get(&address) {
                    if existing.last_synced_block() > target_block {
                        skipped_newer += 1;
                        continue;
                    }
                }

                pool.set_last_synced_block(target_block);
                write_guard.state.insert(address, pool);
                updated += 1;
            }

            info!(
                "State maintenance committed at block {}. Updated {}, skipped {} newer pools.",
                target_block, updated, skipped_newer
            );

            current_index = end;
        }
    }
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
    loop {
        // Sleep first to avoid immediate update on start (or sleep at end)
        sleep(interval).await;

        // 1. Collect pools that need update
        // We clone them to release the read lock quickly and to perform async RPC calls without holding the lock
        let mut target_pools: Vec<BalancerV2Pool> = {
            let read_guard = state.read().await;
            read_guard
                .state
                .values()
                .filter_map(|amm| {
                    if let AMM::BalancerV2Pool(pool) = amm {
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
                if let Some(AMM::BalancerV2Pool(existing_pool)) =
                    write_guard.state.get_mut(&pool.address)
                {
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

/// Periodically updates rates for Balancer V3 pools that have rate providers.
pub async fn start_balancer_v3_rate_sync_task<N, P>(
    state: Arc<RwLock<StateSpace>>,
    provider: P,
    interval: Duration,
) where
    N: Network,
    P: Provider<N> + Clone + 'static,
{
    loop {
        // Sleep first to avoid immediate update on start (or sleep at end)
        sleep(interval).await;

        // 1. Collect pools that need update
        let mut target_pools: Vec<BalancerV3Pool> = {
            let read_guard = state.read().await;
            read_guard
                .state
                .values()
                .filter_map(|amm| {
                    if let AMM::BalancerV3Pool(pool) = amm {
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
                if let Some(AMM::BalancerV3Pool(existing_pool)) =
                    write_guard.state.get_mut(&pool.address)
                {
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
    loop {
        sleep(interval).await;

        // 1. Collect ALL Balancer V3 pools (not just those with rate providers)
        let mut target_pools: Vec<BalancerV3Pool> = {
            let read_guard = state.read().await;
            read_guard
                .state
                .values()
                .filter_map(|amm| {
                    if let AMM::BalancerV3Pool(pool) = amm {
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
                if let Some(AMM::BalancerV3Pool(existing_pool)) =
                    write_guard.state.get_mut(&pool.address)
                {
                    // Only update if swap_fee actually changed or was zero
                    if existing_pool.swap_fee.is_zero() || existing_pool.swap_fee != pool.swap_fee {
                        if !pool.swap_fee.is_zero() {
                            existing_pool.swap_fee = pool.swap_fee;
                            updated_count += 1;
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
    loop {
        sleep(interval).await;

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
                .filter_map(|amm| match amm {
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
                    if let Some(amm) = write_guard.state.get_mut(addr) {
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
        // Tuple: (Address, n_coins, is_ng)
        let crypto_targets: Vec<(Address, u8, bool)> = {
            let read_guard = state.read().await;
            read_guard
                .state
                .values()
                .filter_map(|amm| match amm {
                    AMM::CurveNGPool(pool) => {
                        if pool.pool_type.is_crypto() && pool.n_coins > 0 {
                            Some((pool.address, pool.n_coins, true))
                        } else {
                            None
                        }
                    }
                    AMM::CurveLegacyPool(pool) => {
                        if pool.pool_type
                            == crate::amms::curve_legacy::CurveLegacyPoolType::CryptoSwap
                            && pool.n_coins > 0
                        {
                            Some((pool.address, pool.n_coins, false))
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
            for (addr, n_coins, is_ng) in crypto_targets {
                // Fetch logic depends on pool variant and coin count
                let price_scale_result = if is_ng {
                    // --- Curve NG Strategy ---
                    if n_coins == 2 {
                        // TwoCrypto: single uint256
                        let contract = ICurveTwoCrypto::new(addr, provider.clone());
                        contract.price_scale().call().await.ok().map(|ps| vec![ps])
                    } else {
                        // TriCrypto: array of uint256s, accessed by index
                        let contract = ICurveTriCrypto::new(addr, provider.clone());
                        let mut scales = Vec::new();
                        let mut success = true;
                        for i in 0..(n_coins - 1) {
                            match contract
                                .price_scale(alloy::primitives::U256::from(i))
                                .call()
                                .await
                            {
                                Ok(ps) => scales.push(ps),
                                Err(_) => {
                                    success = false;
                                    break;
                                }
                            }
                        }
                        if success && !scales.is_empty() {
                            Some(scales)
                        } else {
                            None
                        }
                    }
                } else {
                    // --- Curve Legacy Strategy ---
                    // Legacy CryptoSwap usually follows TriCrypto logic (index-based)
                    let contract = ICurveTriCrypto::new(addr, provider.clone());
                    let mut scales = Vec::new();
                    let mut success = true;
                    // n_coins=3 implies 2 price scales (0 and 1)
                    for i in 0..(n_coins - 1) {
                        match contract
                            .price_scale(alloy::primitives::U256::from(i))
                            .call()
                            .await
                        {
                            Ok(ps) => scales.push(ps),
                            Err(_) => {
                                success = false;
                                break;
                            }
                        }
                    }
                    if success && !scales.is_empty() {
                        Some(scales)
                    } else {
                        None
                    }
                };

                // For CryptoSwap pools (both NG and Legacy), fetch D value from chain
                // D is critical for accurate get_dy calculations and must be in sync with on-chain state
                // Our local newton_d implementation may differ slightly from chain's newton_D,
                // causing significant swap output errors (up to 18% observed)
                let d_result = {
                    let pool_contract = ICurveNGPool::new(addr, provider.clone());
                    pool_contract.D().call().await.ok()
                };

                // Fetch balances for CryptoSwap pools to fix admin fee drift
                let mut new_balances = Vec::new();
                let pool_contract = ICurveNGPool::new(addr, provider.clone());
                let mut balances_ok = true;
                for i in 0..n_coins {
                    // Use uint256 index for CryptoSwap
                    match pool_contract
                        .balances(alloy::primitives::U256::from(i))
                        .call()
                        .await
                    {
                        Ok(b) => new_balances.push(b),
                        Err(_) => {
                            balances_ok = false;
                            break;
                        }
                    }
                }

                // Apply update if fetch succeeded
                if let Some(new_price_scale) = price_scale_result {
                    let mut write_guard = state.write().await;
                    match write_guard.state.get_mut(&addr) {
                        Some(AMM::CurveNGPool(pool)) => {
                            pool.price_scale = Some(new_price_scale);
                            // Use D value fetched from chain instead of local recalculation
                            // Our newton_d has precision differences from chain's newton_D,
                            // causing significant swap simulation errors
                            if let Some(d) = d_result {
                                pool.d = Some(d);
                            }

                            if balances_ok && new_balances.len() == pool.n_coins as usize {
                                pool.balances = new_balances;
                            }

                            ps_updated_count += 1;
                        }

                        Some(AMM::CurveLegacyPool(pool)) => {
                            pool.price_scale = Some(new_price_scale.clone());
                            // Also update D value for Legacy CryptoSwap to ensure consistency
                            // D must be in sync with price_scale for accurate get_dy calculations
                            if let Some(d) = d_result {
                                pool.d = Some(d);
                            }

                            if balances_ok && new_balances.len() == pool.n_coins as usize {
                                pool.balances = new_balances;
                            }

                            ps_updated_count += 1;
                        }
                        _ => {}
                    }
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
    let resolver_address: Address = FLUID_DEX_RESOLVER.parse().unwrap_or(Address::ZERO);
    let resolver = DexReservesResolver::new(resolver_address, provider.clone());

    loop {
        sleep(interval).await;

        // 1. Collect Fluid DEX pool addresses that need update
        let target_pools: Vec<Address> = {
            let read_guard = state.read().await;
            read_guard
                .state
                .values()
                .filter_map(|amm| {
                    if let AMM::FluidDexPool(pool) = amm {
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

        // 3. Update state
        let mut updated_count = 0;
        {
            let mut write_guard = state.write().await;
            for pr in pools_reserves {
                if let Some(AMM::FluidDexPool(pool)) = write_guard.state.get_mut(&pr.pool) {
                    // Update center price
                    pool.center_price_1e27 = pr.centerPrice;

                    // Update combined reserves
                    pool.token0_real_reserves_1e12 = pr.collateralReserves.token0RealReserves
                        + pr.debtReserves.token0RealReserves;
                    pool.token1_real_reserves_1e12 = pr.collateralReserves.token1RealReserves
                        + pr.debtReserves.token1RealReserves;
                    pool.token0_imag_reserves_1e12 = pr.collateralReserves.token0ImaginaryReserves
                        + pr.debtReserves.token0ImaginaryReserves;
                    pool.token1_imag_reserves_1e12 = pr.collateralReserves.token1ImaginaryReserves
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
                        expand_duration: pr.limits.withdrawableToken0.expandDuration.to::<u64>(),
                    };
                    pool.withdrawable_token1 = TokenLimitData {
                        available: pr.limits.withdrawableToken1.available,
                        expands_to: pr.limits.withdrawableToken1.expandsTo,
                        expand_duration: pr.limits.withdrawableToken1.expandDuration.to::<u64>(),
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

                    pool.limits_sync_time = current_time;
                    updated_count += 1;
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

use alloy::eips::BlockId;
use alloy::network::Network;
use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolValue;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::amms::aerodrome_slipstream::pool::FEE_MODULE_GLOBALS;
use crate::amms::aerodrome_slipstream::pool::{
    DynamicFeeConfig, FeeModuleGlobals, GetAerodromeSlipstreamFeeConfigBatchRequest,
};
use crate::amms::amm::{AutomatedMarketMaker, AMM};
use crate::amms::balancer_v2::BalancerV2Pool;
use crate::amms::balancer_v3::BalancerV3Pool;
use crate::amms::binaryfi_prop::BinaryFiPropPool;
use crate::amms::caliber_prop::CaliberPropPool;
use crate::amms::curve_ng::{CurveNGFactory, ICurveNGStableSwap};
use crate::amms::fluid_dex::{
    DexReservesResolver, FluidDexT1, FluidLiquidity, TokenLimitData, FLUID_DEX_RESOLVER,
};
use crate::amms::pendle::PendlePool;
use crate::amms::rocketpool::RocketPoolConverter;
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

// ── FeeModule reader interface ──
sol! {
    #[sol(rpc)]
    contract IDynamicFeeModuleReader {
        function dynamicFeeConfig(address pool) external view returns (uint24 baseFee, uint24 feeCap, uint64 scalingFactor, bool initialFeeEnabled, uint24 initialFee);
        function defaultScalingFactor() external view returns (uint256);
        function defaultFeeCap() external view returns (uint256);
        function secondsAgo() external view returns (uint32);
    }
}

/// Periodically refreshes DynamicFeeConfig and global FeeModule parameters
/// for all Slipstream pools.
///
/// These parameters (scalingFactor, feeCap, initialFee, etc.) are changed by
/// governance operations which emit events we don't fully subscribe to.
/// This low-frequency task acts as a safety net to keep fee config in sync.
///
/// Note: `pool.fee` and `observations` are maintained in real-time via event-driven
/// sync and do NOT require periodic refresh.
pub async fn start_slipstream_fee_config_sync_task<N, P>(
    state: Arc<RwLock<StateSpace>>,
    provider: P,
    interval: Duration,
    fee_module_addresses: Vec<Address>,
) where
    N: Network,
    P: Provider<N> + Clone + 'static,
{
    const BATCH_SIZE: usize = 40;

    let mut next_sleep = startup_delay_with_jitter(interval, "slipstream_fee_config");
    loop {
        sleep(next_sleep).await;
        next_sleep = interval;

        // 1. Collect all Slipstream pool addresses
        let pool_list: Vec<Address> = {
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

        if pool_list.is_empty() {
            continue;
        }

        // 2. Refresh global FeeModule parameters (from first available FeeModule)
        if let Some(&fm_addr) = fee_module_addresses.first() {
            let fee_module = IDynamicFeeModuleReader::new(fm_addr, provider.clone());
            let globals = FeeModuleGlobals {
                default_scaling_factor: match fee_module.defaultScalingFactor().call().await {
                    Ok(v) => v.to::<u64>(),
                    Err(_) => 0,
                },
                default_fee_cap: match fee_module.defaultFeeCap().call().await {
                    Ok(v) => v.to::<u32>(),
                    Err(_) => 50_000,
                },
                seconds_ago: match fee_module.secondsAgo().call().await {
                    Ok(v) => v,
                    Err(_) => 600,
                },
            };
            if let Ok(mut g) = FEE_MODULE_GLOBALS.lock() {
                *g = globals;
            }
        }

        // 3. Batch-process pools: refresh dynamicFeeConfig via batch contract
        for chunk in pool_list.chunks(BATCH_SIZE) {
            let batch_result = GetAerodromeSlipstreamFeeConfigBatchRequest::deploy_builder(
                provider.clone(),
                chunk.to_vec(),
            )
            .call_raw()
            .await;

            match batch_result {
                Ok(data) => {
                    use alloy::sol_types::SolType;
                    type FeeConfigDataArray = sol!((address, address, bool, bool, bool, bool, bool, int24, uint24, uint24, uint24, uint64, bool, uint24)[]);
                    if let Ok(decoded) = FeeConfigDataArray::abi_decode(&data) {
                        let mut write_guard = state.write().await;
                        for (i, fcd) in decoded.iter().enumerate() {
                            if i >= chunk.len() {
                                break;
                            }
                            let pool_addr = chunk[i];
                            if let Some(amm) = write_guard.get_mut_cow(&pool_addr) {
                                if let AMM::AerodromeSlipstreamPool(pool) = amm {
                                    // Periodic task is best-effort:
                                    // update only fields explicitly marked successful by batch contract.
                                    // fcd layout:
                                    // (factory, feeModule, factoryOk, tickSpacingOk, tickSpacingFeeOk, feeModuleOk, dynamicFeeConfigOk, tickSpacing, tickSpacingFee, baseFee, feeCap, scalingFactor, initialFeeEnabled, initialFee)
                                    if !fcd.2
                                        || !fcd.3
                                        || fcd.0 == Address::ZERO
                                        || fcd.7.as_i32() == 0
                                    {
                                        tracing::warn!(
                                            target: "state_space::slipstream_fee_config_sync",
                                            pool = ?pool_addr,
                                            factory = ?fcd.0,
                                            tick_spacing = fcd.7.as_i32(),
                                            "partial fee-context fetch detected; skipping this pool update and preserving cached values"
                                        );
                                        continue;
                                    }

                                    pool.tick_spacing = fcd.7.as_i32();
                                    if fcd.4 && fcd.8.to::<u32>() != 0 {
                                        pool.factory_tick_spacing_fee = fcd.8.to::<u32>();
                                    } else {
                                        tracing::warn!(
                                            target: "state_space::slipstream_fee_config_sync",
                                            pool = ?pool_addr,
                                            tick_spacing = pool.tick_spacing,
                                            "tickSpacingToFee missing in this sync round; preserving previous cached mapping"
                                        );
                                    }
                                    if fcd.5 && fcd.6 && fcd.1 != Address::ZERO {
                                        // feeModule != Address::ZERO
                                        pool.dynamic_fee_config = DynamicFeeConfig {
                                            base_fee: fcd.9.to::<u32>(),
                                            fee_cap: fcd.10.to::<u32>(),
                                            scaling_factor: fcd.11,
                                            initial_fee_enabled: fcd.12,
                                            initial_fee: fcd.13.to::<u32>(),
                                        };
                                    }
                                }
                            }
                        }
                    } else {
                        error!("Slipstream fee config sync: batch decode failed");
                    }
                }
                Err(e) => {
                    error!("Slipstream fee config sync: batch call failed: {:?}", e);
                }
            }
        }

        info!(
            "Slipstream fee config sync: refreshed {} pools",
            pool_list.len()
        );
    }
}

/// Periodically updates non-event runtime data for Curve pools (NG and Legacy).
///
/// This task handles two main categories of updates:
/// 1. StableSwap runtime data:
///    - Curve NG StableSwap pools: balances/amp/fee/admin_fee/stored_rates/offpeg multiplier.
///    - Curve Legacy StableSwap pools: stored_rates for lending/rebasing variants.
/// 2. `price_scale` and related runtime data for CryptoSwap pools:
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
        // Task A: Sync StableSwap runtime data
        // =========================================================================

        // A.1 Collect target pools (StableSwap only)
        // We target both NG and Legacy pools because both can drift outside events.
        let (mut ng_stable_pools, legacy_stable_targets): (Vec<_>, Vec<_>) = {
            let read_guard = state.read().await;
            let mut ng = Vec::new();
            let mut legacy = Vec::new();

            for amm in read_guard.state.values() {
                match amm.as_ref() {
                    AMM::CurveNGPool(pool) if pool.pool_type.is_stable() && pool.n_coins > 0 => {
                        ng.push(pool.clone());
                    }
                    AMM::CurveLegacyPool(pool)
                        if pool.pool_type
                            == crate::amms::curve_legacy::CurveLegacyPoolType::StableSwap
                            && pool.n_coins > 0 =>
                    {
                        legacy.push(pool.address);
                    }
                    _ => {}
                }
            }

            (ng, legacy)
        };

        let stable_target_count = ng_stable_pools.len() + legacy_stable_targets.len();
        if stable_target_count > 0 {
            info!(
                "Syncing StableSwap runtime data for {} Curve pools (NG & Legacy)",
                stable_target_count
            );

            let mut updated_count = 0;

            if !ng_stable_pools.is_empty() {
                match provider.get_block_number().await {
                    Ok(fetch_block) => {
                        let block_id = BlockId::from(fetch_block);
                        if let Err(e) = CurveNGFactory::refresh_runtime_data_batch::<N, _>(
                            &mut ng_stable_pools,
                            block_id,
                            provider.clone(),
                        )
                        .await
                        {
                            error!(
                                "Curve NG stable runtime refresh(batch-contract) failed: block={}, err={:?}",
                                fetch_block, e
                            );
                        } else {
                            let mut write_guard = state.write().await;
                            for mut pool in ng_stable_pools.drain(..) {
                                if let Some(existing) = write_guard.get(&pool.address) {
                                    if existing.last_synced_block() > fetch_block {
                                        continue;
                                    }
                                }
                                pool.set_last_synced_block(fetch_block);
                                write_guard.insert_amm(AMM::CurveNGPool(pool));
                                updated_count += 1;
                            }
                        }
                    }
                    Err(e) => {
                        error!(
                            "Failed to fetch block number for Curve NG stable refresh: {:?}",
                            e
                        );
                    }
                }
            }

            for addr in legacy_stable_targets {
                let stable_pool = ICurveNGStableSwap::new(addr, provider.clone());
                let rates_res = stable_pool.stored_rates().call().await.ok();
                let mut write_guard = state.write().await;
                if let Some(AMM::CurveLegacyPool(pool)) = write_guard.get_mut_cow(&addr) {
                    if let Some(rates) = rates_res {
                        if rates.len() == pool.n_coins as usize {
                            pool.rates = rates;
                            updated_count += 1;
                        }
                    }
                }
            }

            if updated_count > 0 {
                state.write().await.rebuild_curve_legacy_meta_views();
            }

            if updated_count > 0 {
                info!(
                    "Updated StableSwap runtime data for {} Curve pools",
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
                state.write().await.rebuild_curve_legacy_meta_views();
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

/// Periodically refreshes Rocket Pool redemption state.
///
/// Rocket Pool redemption capacity depends on protocol accounting plus deposit
/// pool excess balance, so relying on pool-local logs is not sufficient.
pub async fn start_rocketpool_sync_task<N, P>(
    state: Arc<RwLock<StateSpace>>,
    provider: P,
    interval: Duration,
) where
    N: Network,
    P: Provider<N> + Clone + 'static,
{
    let mut next_sleep = startup_delay_with_jitter(interval, "rocketpool");
    loop {
        sleep(next_sleep).await;
        next_sleep = interval;

        let mut target_pools: Vec<RocketPoolConverter> = {
            let read_guard = state.read().await;
            read_guard
                .state
                .values()
                .filter_map(|amm| match amm.as_ref() {
                    AMM::RocketPoolConverter(pool) => Some(pool.clone()),
                    _ => None,
                })
                .collect()
        };

        if target_pools.is_empty() {
            continue;
        }

        info!(
            "Updating redemption state for {} Rocket Pool converters",
            target_pools.len()
        );

        for pool in &mut target_pools {
            if let Err(e) = pool.update::<N, P>(provider.clone()).await {
                error!(
                    address = ?pool.address,
                    error = ?e,
                    "Failed to update Rocket Pool converter"
                );
            }
        }

        let mut write_guard = state.write().await;
        for pool in target_pools {
            if let Some(existing_amm) = write_guard.get_mut_cow(&pool.address) {
                if let AMM::RocketPoolConverter(existing_pool) = existing_amm {
                    *existing_pool = pool;
                }
            }
        }
    }
}

/// 定期刷新 PendlePool 的 sy_exchange_rate（底层计息资产收益率）。
/// sy_exchange_rate 变化缓慢且无事件通知，需周期性拉取。推荐 5-15 分钟间隔。
pub async fn start_pendle_sync_task<N, P>(
    state: Arc<RwLock<StateSpace>>,
    provider: P,
    interval: Duration,
) where
    N: Network,
    P: Provider<N> + Clone + 'static,
{
    let mut next_sleep = startup_delay_with_jitter(interval, "pendle");
    loop {
        sleep(next_sleep).await;
        next_sleep = interval;

        let mut target_pools: Vec<PendlePool> = {
            let read_guard = state.read().await;
            read_guard
                .state
                .values()
                .filter_map(|amm| match amm.as_ref() {
                    AMM::PendlePool(pool) => Some(pool.clone()),
                    _ => None,
                })
                .collect()
        };

        if target_pools.is_empty() {
            continue;
        }

        info!(
            "Updating {} Pendle pools (sy_exchange_rate)",
            target_pools.len()
        );

        for pool in &mut target_pools {
            if let Err(e) = pool.update::<N, P>(provider.clone()).await {
                error!(
                    address = ?pool.address,
                    error = ?e,
                    "Failed to update Pendle pool"
                );
            }
        }

        let mut write_guard = state.write().await;
        for pool in target_pools {
            if let Some(existing_amm) = write_guard.get_mut_cow(&pool.address) {
                if let AMM::PendlePool(existing) = existing_amm {
                    *existing = pool;
                }
            }
        }
    }
}

/// Periodically reconciles Caliber propAMM pool ladder snapshots.
///
/// 实时模式下（默认）报价更新由 Xlayer flashblocks 原始交易流
/// （`batchUpdateParameters` calldata）驱动，本任务降频为**对账/兜底**：
/// - 冷启动 / flashblocks 断流回填；
/// - 储备、pos（`cfg+4/5`、`cfg+7`）等不随更新交易变化的低频变动；
/// - 漏更新 / calldata 解码失败的纠正。
///
/// 对账间隔即断流期间报价滞后的上界（默认 45s，`with_caliber_reconcile_interval`
/// 可配置）；关闭实时开关（`with_caliber_realtime_sync(false)`）时退回纯周期拉取
/// （`caliber_ladder_sync_interval`）。
///
/// 批量刷新：`caliber_prop::batch_refresh_snapshots` 把所有 pool 的
/// `eth_getStorageAt`（固定槽位 + ladder 槽位）折叠进 JSON-RPC batch，
/// 每 512 槽一次 HTTP 请求，RPC 往返从每 pool ~10+n 次降到几乎常数。
pub async fn start_caliber_prop_ladder_sync_task<N, P>(
    state: Arc<RwLock<StateSpace>>,
    provider: P,
    interval: Duration,
) where
    N: Network,
    P: Provider<N> + Clone + 'static,
{
    // ⚠️ 临时兜底（2026-08-10）：生产 RPC 限流下 caliber 对账轮询禁止低于
    // 60s（调用方即使传入 25s/45s 也会被钳制到 60s）。caliber 价格新鲜度由
    // flashblocks 实时交易流保证，对账仅为冷启动/断流/储备/漏更新的低频兜底，
    // 无需高频轮询。后续应改为配置化独立 HTTP RPC 端点后移除。
    const MIN_RECONCILE_INTERVAL: Duration = Duration::from_secs(60);
    let interval = interval.max(MIN_RECONCILE_INTERVAL);
    // 对账失败退避上限：RPC 限流/网关过载时避免持续硬撞（间隔翻倍，
    // 成功恢复 `interval`；配合 storage_at_batch 限流时不逐槽回退）。
    const MAX_RECONCILE_BACKOFF: Duration = Duration::from_secs(300);
    let mut next_sleep = startup_delay_with_jitter(interval, "caliber_prop");
    loop {
        sleep(next_sleep).await;

        let mut target_pools: Vec<CaliberPropPool> = {
            let read_guard = state.read().await;
            read_guard
                .state
                .values()
                .filter_map(|amm| match amm.as_ref() {
                    AMM::CaliberPropPool(pool) => Some(pool.clone()),
                    _ => None,
                })
                .collect()
        };

        if target_pools.is_empty() {
            next_sleep = interval;
            continue;
        }

        let mut reconcile_failed = false;
        debug!(
            "Reconciling ladder snapshots for {} Caliber propAMM pools",
            target_pools.len()
        );

        match crate::amms::caliber_prop::batch_refresh_snapshots::<N, P>(
            &provider,
            &mut target_pools,
            alloy::eips::BlockId::latest(),
        )
        .await
        {
            Ok(flags) => {
                let failed = flags.iter().filter(|f| !**f).count();
                if failed > 0 {
                    reconcile_failed = true;
                    warn!("Caliber reconcile: {}/{} pools failed", failed, flags.len());
                }
            }
            Err(e) => {
                reconcile_failed = true;
                error!(error = ?e, "Caliber reconcile batch snapshot failed");
            }
        }

        let mut write_guard = state.write().await;
        for pool in target_pools {
            if let Some(existing_amm) = write_guard.get_mut_cow(&pool.address()) {
                if let AMM::CaliberPropPool(existing) = existing_amm {
                    *existing = pool;
                }
            }
        }

        // 失败退避：成功恢复基础间隔；失败翻倍（上限 300s），限流恢复后
        // 不会多实例/多轮集体打爆网关。
        next_sleep = if reconcile_failed {
            next_sleep.saturating_mul(2).min(MAX_RECONCILE_BACKOFF)
        } else {
            interval
        };
    }
}

/// BinaryFi propAMM 周期全量校正任务。
///
/// 与 Caliber 不同，BinaryFi 的价格/费率完全由事件驱动（L2 raw-tx 注入，
/// 逐位精确），本任务只负责重新锚定**事件不可观测**的引擎状态：
/// `maxIn/maxOut`（会被 swap 消耗、重置时机引擎内部决定）、买入禁用/冻结态、
/// 金库外部转账引起的余额漂移。
///
/// 更新方式与 caliber 周期任务一致：直接复用池子的 `update()`（批量快照路径）。
/// BinaryFi 的 `update()` 是 stale 驱动（AsyncUpdate 事件触发时标记局部 stale），
/// 周期任务先把全部 pair 标记 stale，再调 `update()` 即等价于全量快照校正，
/// 不新增任何 fetch 逻辑（一次批量静态调用覆盖全部 132 条费率 + 大额 cap + 余额）。
pub async fn start_binaryfi_prop_sync_task<N, P>(
    state: Arc<RwLock<StateSpace>>,
    provider: P,
    interval: Duration,
) where
    N: Network,
    P: Provider<N> + Clone + 'static,
{
    let mut next_sleep = startup_delay_with_jitter(interval, "binaryfi_prop");
    loop {
        sleep(next_sleep).await;
        next_sleep = interval;

        // 虚拟子池化后按部署分组：每组取 seed 做一次全量快照刷新，再把刷新结果
        // 整份克隆到同部署其余实例（恢复各自虚拟身份），避免 65 个实例各自触发
        // 一次链上批量读取。
        let mut groups: HashMap<(Address, Address, Address, Address), Vec<BinaryFiPropPool>> =
            HashMap::new();
        {
            let read_guard = state.read().await;
            for amm in read_guard.state.values() {
                if let AMM::BinaryFiPropPool(pool) = amm.as_ref() {
                    groups
                        .entry(pool.deployment_key())
                        .or_default()
                        .push(pool.clone());
                }
            }
        }

        if groups.is_empty() {
            continue;
        }

        debug!(
            "Re-anchoring {} BinaryFi propAMM deployment(s) via full snapshot",
            groups.len()
        );

        let mut write_guard = state.write().await;
        for (_, group) in groups {
            let Some(mut seed) = group.first().cloned() else {
                continue;
            };
            // 一次链上批量读取全量快照；分发时各实例自行 apply_snapshot，
            // 利用实例自身的 price_updated_block 保鲜判断（日志价格 >= snap_block
            // 不覆盖），防止快照回退更新日志价格。
            let (snap, snap_block) = match seed.fetch_full_snapshot::<N, P>(provider.clone()).await
            {
                Ok(v) => v,
                Err(e) => {
                    error!(
                        address = ?seed.pool_address,
                        error = ?e,
                        "Failed to refresh BinaryFi propAMM full snapshot"
                    );
                    continue;
                }
            };
            let refreshed_pairs: Vec<usize> =
                snap.quotePairs.iter().map(|p| p.to::<usize>()).collect();
            for pool in group {
                let addr = pool.address();
                // 保留实例虚拟身份 + 实例自身 price_updated_block（日志保鲜）
                let mut refreshed = pool.clone();
                refreshed.apply_snapshot(&snap, snap_block);
                refreshed.set_last_synced_block(snap_block);
                refreshed.clear_stale_pairs(&refreshed_pairs);
                if let Some(existing_amm) = write_guard.get_mut_cow(&addr) {
                    if let AMM::BinaryFiPropPool(existing) = existing_amm {
                        *existing = refreshed;
                    }
                }
            }
        }
    }
}


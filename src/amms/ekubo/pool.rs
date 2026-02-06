//! Ekubo V2 Pool Implementation
//!
//! This module contains the EkuboPool struct and its AMM trait implementation,
//! including multi-tick swap iteration for accurate simulation.

use super::math;
use super::types::{parse_position_updated_log0, parse_swap_event_log0, EkuboPoolKey, TickInfo};
use crate::amms::{
    amm::{AutomatedMarketMaker, SyncAction},
    consts::MPFR_T_PRECISION,
    error::AMMError,
    Token,
};
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{address, Address, B256, U256},
    providers::Provider,
    rpc::types::Log,
    sol,
};
use rug::ops::Pow;
use rug::Float;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::info;

// ========== Sol Interfaces ==========

sol! {
    // CoreDataFetcher 合约 - 查询池状态
    // 地址: 0x208BB00c6b142351e4a431f6Dd323691ebb7C285
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    interface CoreDataFetcher {
        struct PoolKey {
            address token0;
            address token1;
            bytes32 config;  // PoolConfig (bytes32)
        }

        /// 查询池状态 - 获取流动性
        function poolState(PoolKey memory poolKey)
            external
            view
            returns (
                uint160 sqrtRatio,  // 注意: 这个值可能不是标准的 sqrtPriceX96, 我们主要使用 poolPrice 获取价格
                int32 tick,
                uint128 liquidity
            );

        /// 查询池价格 - 获取精确价格 (Q64.128)
        function poolPrice(PoolKey memory poolKey)
            external
            view
            returns (
                uint256 sqrtRatioFixed,
                int32 tick
            );
    }
}

// ========== Constants ==========

const TICK_BASE: f64 = 1.000001;
// Ekubo tick spacing is 1.000001, which is 100x more precise than Uniswap V3 (1.0001)
// So the tick range is ~100x larger: log(2^128) / log(1.000001) ≈ 88,722,839
const MIN_TICK: i32 = -88722839;
const MAX_TICK: i32 = 88722839;

// ========== EkuboPool Struct ==========

/// Ekubo Pool 实现（参考 UniswapV4）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EkuboPool {
    // Singleton 地址
    pub address: Address,
    pub pool_key: EkuboPoolKey,
    pub pool_id: B256,

    #[serde(default)]
    pub last_synced_block: u64,

    pub token_a: Token,
    pub token_b: Token,

    // 核心状态
    pub liquidity: u128,
    pub sqrt_price: U256,
    pub fee: u128,
    pub tick: i32,
    pub tick_spacing: i32,

    // Tick 数据（与 UniswapV3 相同）
    pub tick_bitmap: HashMap<i32, U256>,
    pub ticks: HashMap<i32, TickInfo>,

    // 缓存价格
    #[serde(default)]
    pub token_a_price: f64,
    #[serde(default)]
    pub token_b_price: f64,
}

// ========== AMM Trait Implementation ==========

impl AutomatedMarketMaker for EkuboPool {
    fn address(&self) -> Address {
        // 使用 pool_id 的前 20 字节作为虚拟地址
        // 避免在 StateSpace 中冲突(StateSpace 使用 HashMap<Address, AMM>)
        // 与 UniswapV4 的实现完全一致
        if self.pool_id == B256::ZERO {
            Address::ZERO
        } else {
            Address::from_slice(&self.pool_id.as_slice()[0..20])
        }
    }

    fn supported_chains(&self) -> Option<Vec<u64>> {
        Some(vec![1]) // Ekubo V2 deployment with hardcoded address is on Mainnet
    }

    fn last_synced_block(&self) -> u64 {
        self.last_synced_block
    }

    fn has_sufficient_liquidity(&self) -> bool {
        // Validate tick is within valid range
        // Ekubo uses 1.000001 tick base, range is +/- 88,722,839
        if self.tick < MIN_TICK || self.tick > MAX_TICK {
            tracing::warn!(
                target = "amms::ekubo::has_sufficient_liquidity",
                tick = self.tick,
                pool_id = ?self.pool_id,
                "Invalid tick value - pool filtered out"
            );
            return false;
        }

        // CRITICAL: Validate tick data exists
        // If tick_bitmap or ticks are empty, find_next_initialized_tick will return None,
        // causing simulate_swap to assume infinite liquidity depth and produce
        // astronomically wrong outputs (false arbitrage opportunities).
        if self.tick_bitmap.is_empty() || self.ticks.is_empty() {
            tracing::warn!(
                target = "amms::ekubo::has_sufficient_liquidity",
                pool_id = ?self.pool_id,
                tick_bitmap_count = self.tick_bitmap.len(),
                ticks_count = self.ticks.len(),
                "Missing tick data - pool filtered out to prevent false arbitrage"
            );
            return false;
        }

        // Dynamic liquidity threshold based on token decimals
        // L ~ sqrt(x * y)
        // We estimate required L based on required token amounts (x_thresh, y_thresh)
        // L_thresh = sqrt(x_thresh * y_thresh)

        let d_a = self.token_a.decimals;
        let d_b = self.token_b.decimals;

        let t_a_u128 = if d_a >= 18 {
            10u128.pow(d_a as u32 - 4)
        }
        // 0.0001
        else if d_a >= 6 {
            100u128.saturating_mul(10u128.pow(d_a as u32))
        }
        // 100
        else {
            100_000
        };

        let t_b_u128 = if d_b >= 18 {
            10u128.pow(d_b as u32 - 4)
        }
        // 0.0001
        else if d_b >= 6 {
            100u128.saturating_mul(10u128.pow(d_b as u32))
        }
        // 100
        else {
            100_000
        };

        // Calculate geometric mean of thresholds
        let l_thresh = if let Some(prod) = t_a_u128.checked_mul(t_b_u128) {
            prod.isqrt()
        } else {
            // Fallback if product overflows u128 (rare, requires decimals > 30)
            u128::MAX.isqrt()
        };

        self.liquidity >= l_thresh
    }

    fn decimals(&self, token: Address) -> u8 {
        if token == self.token_a.address {
            self.token_a.decimals
        } else if token == self.token_b.address {
            self.token_b.decimals
        } else {
            0
        }
    }

    fn set_last_synced_block(&mut self, block_number: u64) {
        self.last_synced_block = self.last_synced_block.max(block_number);
    }

    fn sync_events(&self) -> Vec<B256> {
        // Ekubo V2 使用 Log0 匿名事件 (无 topic signature)
        // 返回空 vec - Log0 事件需要通过地址过滤而非 topic 过滤
        // StateSpace 需要特殊处理 Ekubo 池子的事件订阅
        vec![]
    }

    fn spot_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
        // 必须验证 BOTH base_token AND quote_token 都存在于池子中
        let base_is_a = base_token == self.token_a.address;
        let base_is_b = base_token == self.token_b.address;
        let quote_is_a = quote_token == self.token_a.address;
        let quote_is_b = quote_token == self.token_b.address;

        if !base_is_a && !base_is_b {
            return Err(AMMError::TokenNotFound(base_token));
        }
        if !quote_is_a && !quote_is_b {
            return Err(AMMError::TokenNotFound(quote_token));
        }
        if base_token == quote_token {
            return Err(AMMError::Msg(
                "base and quote tokens are the same".to_string(),
            ));
        }

        let price = if base_is_a {
            self.token_a_price
        } else {
            self.token_b_price
        };

        // 价格有效性校验：0 或非有限值表示价格未初始化或计算失败
        if price <= 0.0 || !price.is_finite() {
            return Err(AMMError::Msg("Invalid cached spot price".to_string()));
        }

        Ok(price)
    }

    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError> {
        // Ekubo V2 使用 Log0 匿名事件 (无 topic)
        // 检查日志来源是否是 Core 合约 (0xe0e0e08a6a4b9dc7bd67bcb7aade5cf48157d444)
        // self.address 是虚拟地址，不能用于校验日志来源
        if log.address() != address!("e0e0e08a6a4b9dc7bd67bcb7aade5cf48157d444") {
            return Ok(SyncAction::None);
        }

        // Log0 事件没有 topics,直接解析 data
        let data = log.data().data.as_ref();

        // SwapEvent: 116 字节
        if data.len() == 116 {
            match parse_swap_event_log0(data) {
                Ok(swap_event) => {
                    // 验证 pool_id 匹配
                    if swap_event.pool_id != self.pool_id {
                        return Ok(SyncAction::None);
                    }

                    if let Ok(sqrt_price) = self.get_sqrt_ratio_at_tick(swap_event.tick_after) {
                        self.sqrt_price = sqrt_price;
                        self.tick = swap_event.tick_after;
                        self.liquidity = swap_event.liquidity_after;

                        if let Ok(p) =
                            self.calculate_price(self.token_a.address, self.token_b.address)
                        {
                            self.token_a_price = p;
                            if p != 0.0 {
                                self.token_b_price = 1.0 / p;
                            } else {
                                self.token_b_price = 0.0;
                            }
                        }
                    } else {
                        tracing::debug!(
                            target = "amms::ekubo::sync",
                            pool_id = ?self.pool_id,
                            tick = swap_event.tick_after,
                            "Failed to compute sqrt ratio from tick"
                        );
                    }

                    info!(
                        target = "amms::ekubo::sync",
                        pool_id = ?self.pool_id,
                        sqrt_price = ?self.sqrt_price,
                        liquidity = ?self.liquidity,
                        tick = ?self.tick,
                        "Swap synced (Log0)"
                    );
                }
                Err(e) => {
                    tracing::debug!(
                        target = "amms::ekubo::sync",
                        error = ?e,
                        "Failed to parse Log0 SwapEvent"
                    );
                }
            }
        }
        // PositionUpdated: 140 字节
        else if data.len() == 140 {
            match parse_position_updated_log0(data) {
                Ok(position_event) => {
                    // 验证 pool_id 匹配
                    if position_event.pool_id != self.pool_id {
                        return Ok(SyncAction::None);
                    }

                    let tick_lower = position_event.tick_lower;
                    let tick_upper = position_event.tick_upper;
                    let liquidity_delta = position_event.liquidity_delta;

                    // 更新 tick 数据 (使用已有的 modify_position 逻辑)
                    if liquidity_delta != 0 {
                        // 更新 lower tick
                        self.update_tick_from_event(tick_lower, liquidity_delta, false);
                        // 更新 upper tick
                        self.update_tick_from_event(tick_upper, liquidity_delta, true);

                        // 如果当前价格在范围内，更新活跃流动性
                        if self.tick >= tick_lower && self.tick < tick_upper {
                            self.liquidity = if liquidity_delta < 0 {
                                self.liquidity.saturating_sub((-liquidity_delta) as u128)
                            } else {
                                self.liquidity.saturating_add(liquidity_delta as u128)
                            };
                        }
                    }

                    info!(
                        target = "amms::ekubo::sync",
                        pool_id = ?self.pool_id,
                        tick_lower = tick_lower,
                        tick_upper = tick_upper,
                        liquidity_delta = liquidity_delta,
                        "PositionUpdated synced (Log0)"
                    );
                }
                Err(e) => {
                    tracing::debug!(
                        target = "amms::ekubo::sync",
                        error = ?e,
                        "Failed to parse Log0 PositionUpdated"
                    );
                }
            }
        }

        Ok(SyncAction::None)
    }

    fn simulate_swap(
        &self,
        base_token: Address,
        _quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        // Multi-tick swap simulation
        // This implements the full tick iteration logic matching on-chain behavior

        if amount_in.is_zero() {
            return Ok(U256::ZERO);
        }

        // 防御性检查
        if self.sqrt_price.is_zero() {
            return Err(AMMError::Msg("sqrt_price is zero".into()));
        }
        if self.liquidity == 0 {
            return Err(AMMError::Msg("liquidity is zero".into()));
        }
        // Validate tick is within valid range
        if self.tick < MIN_TICK || self.tick > MAX_TICK {
            return Err(AMMError::Msg(format!(
                "tick {} out of valid range [{}, {}]",
                self.tick, MIN_TICK, MAX_TICK
            )));
        }

        let zero_for_one = base_token == self.token_a.address;

        // Initialize state for multi-tick iteration
        let mut current_sqrt_ratio = self.sqrt_price;
        let mut current_liquidity = self.liquidity;
        let mut current_tick = self.tick;

        // Price limits
        let sqrt_price_limit = if zero_for_one {
            math::MIN_SQRT_RATIO
        } else {
            math::MAX_SQRT_RATIO
        };

        // 将输入金额转换为 i128
        let mut amount_remaining: i128 = amount_in
            .try_into()
            .map_err(|_| AMMError::Msg("amount_in overflow i128".into()))?;

        let mut amount_out: u128 = 0;

        // Ekubo V2 费率是 uint64 格式 (2^64 分母)
        let fee_u64 = self.fee as u64;

        // is_token1 indicates the input token is token1
        // zero_for_one = true means token0 -> token1, so is_token1 = false
        let is_token1 = !zero_for_one;

        // Multi-tick iteration loop
        // For arbitrage, we typically don't cross many ticks, but we implement full logic
        const MAX_ITERATIONS: u32 = 100; // Safety limit
        let mut iterations = 0;

        while amount_remaining > 0 && current_sqrt_ratio != sqrt_price_limit {
            iterations += 1;
            if iterations > MAX_ITERATIONS {
                tracing::warn!(
                    target = "amms::ekubo::simulate_swap",
                    "Max iterations reached in multi-tick swap"
                );
                break;
            }

            // Find next initialized tick
            let sqrt_ratio_target =
                self.get_next_sqrt_ratio_target(current_tick, zero_for_one, sqrt_price_limit);

            // CRITICAL SAFETY CHECK: If first iteration and target equals limit,
            // it means no initialized ticks were found.
            // If we have liquidity, this is valid (wide position).
            // If liquidity is 0, then we can't swap anyway (caught by earlier check).
            if iterations == 1 && sqrt_ratio_target == sqrt_price_limit {
                // No op - just proceed to consume liquidity
                tracing::trace!(
                    target = "amms::ekubo::simulate_swap",
                    "No initialized ticks in range, assuming constant liquidity"
                );
            }

            // Compute swap step
            let step_result = math::compute_swap_step(
                current_sqrt_ratio,
                current_liquidity,
                sqrt_ratio_target,
                amount_remaining,
                is_token1,
                fee_u64,
            )
            .map_err(|e| AMMError::Msg(format!("Ekubo swap math error: {}", e)))?;

            // Update remaining amount
            amount_remaining = amount_remaining
                .checked_sub(step_result.consumed_amount.abs())
                .unwrap_or(0);

            // Accumulate output
            amount_out = amount_out
                .checked_add(step_result.calculated_amount)
                .ok_or_else(|| AMMError::Msg("Amount out overflow".into()))?;

            // Update current sqrt ratio
            current_sqrt_ratio = step_result.sqrt_ratio_next;

            // Check if we hit the tick boundary and need to cross
            if step_result.sqrt_ratio_next == sqrt_ratio_target
                && sqrt_ratio_target != sqrt_price_limit
            {
                // Cross tick - update liquidity
                let next_tick = self.get_tick_at_sqrt_ratio(sqrt_ratio_target)?;

                // Get liquidity delta from tick data
                if let Some(tick_info) = self.ticks.get(&next_tick) {
                    if zero_for_one {
                        // Moving down, subtract liquidity_net
                        current_liquidity = if tick_info.liquidity_net < 0 {
                            current_liquidity
                                .checked_add((-tick_info.liquidity_net) as u128)
                                .unwrap_or(current_liquidity)
                        } else {
                            current_liquidity
                                .checked_sub(tick_info.liquidity_net as u128)
                                .unwrap_or(0)
                        };
                    } else {
                        // Moving up, add liquidity_net
                        current_liquidity = if tick_info.liquidity_net >= 0 {
                            current_liquidity
                                .checked_add(tick_info.liquidity_net as u128)
                                .unwrap_or(current_liquidity)
                        } else {
                            current_liquidity
                                .checked_sub((-tick_info.liquidity_net) as u128)
                                .unwrap_or(0)
                        };
                    }
                }

                current_tick = if zero_for_one {
                    next_tick - 1
                } else {
                    next_tick
                };

                // If liquidity is now zero, we can't continue
                if current_liquidity == 0 {
                    break;
                }
            }
        }

        tracing::trace!(
            target = "amms::ekubo::simulate_swap",
            amount_in = ?amount_in,
            amount_out = ?amount_out,
            iterations = iterations,
            ?zero_for_one,
            sqrt_price = ?self.sqrt_price,
            liquidity = ?self.liquidity,
        );

        Ok(U256::from(amount_out))
    }

    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let amount_out = self.simulate_swap(base_token, quote_token, amount_in)?;

        if let Ok(p) = self.calculate_price(self.token_a.address, self.token_b.address) {
            self.token_a_price = p;
            if p != 0.0 {
                self.token_b_price = 1.0 / p;
            } else {
                self.token_b_price = 0.0;
            }
        }

        Ok(amount_out)
    }

    fn tokens(&self) -> Vec<Address> {
        vec![self.token_a.address, self.token_b.address]
    }

    fn calculate_price(&self, base_token: Address, _quote_token: Address) -> Result<f64, AMMError> {
        if self.sqrt_price.is_zero() {
            return Err(AMMError::Msg("sqrt_price is zero".into()));
        }
        let sqrt_price_x96 = self.sqrt_price;

        let sqrt_price_str = sqrt_price_x96.to_string();
        let sqrt_price_val = Float::parse_radix(&sqrt_price_str, 10)
            .map_err(|e| AMMError::Msg(format!("Float parse error: {}", e)))?;
        let sqrt_price_float = Float::with_val(MPFR_T_PRECISION, sqrt_price_val);

        // Ekubo uses 64.128 fixed-point, so divide by 2^128, not 2^96
        let mut denom = Float::with_val(MPFR_T_PRECISION, 1);
        denom <<= 128u32;

        let p_raw = (sqrt_price_float / denom).pow(2);

        let shift = self.token_a.decimals as i32 - self.token_b.decimals as i32;
        let scale_factor = Float::with_val(MPFR_T_PRECISION, 10).pow(shift);

        let price_a: Float = p_raw * scale_factor;
        let price_a_f64 = price_a.to_f64();

        if base_token == self.token_a.address {
            Ok(price_a_f64)
        } else {
            if price_a_f64 == 0.0 {
                Ok(0.0)
            } else {
                Ok(1.0 / price_a_f64)
            }
        }
    }

    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        Self: Sized,
        N: Network,
        P: Provider<N> + Clone,
    {
        // 1. Fetch core state (sqrt_price, tick, liquidity)
        self = self
            .fetch_core_state(block_number, provider.clone())
            .await?;

        // 2. Fetch full tick data (batched for single pool)
        // Wrap self in AMM wrapper for the factory sync methods
        {
            use super::factory::EkuboFactory;
            use crate::amms::amm::AMM;

            let mut pools = vec![AMM::EkuboPool(self.clone())];

            // Sync tick bitmaps
            EkuboFactory::sync_tick_bitmaps::<N, _>(&mut pools, block_number, provider.clone())
                .await?;

            // Sync tick data
            EkuboFactory::sync_tick_data::<N, _>(&mut pools, block_number, provider.clone())
                .await?;

            // Update self with synced data
            if let AMM::EkuboPool(synced_pool) = pools.remove(0) {
                self.tick_bitmap = synced_pool.tick_bitmap;
                self.ticks = synced_pool.ticks;
            }
        }

        info!(
            target: "amms::ekubo::init",
            pool_id = ?self.pool_id,
            tick_bitmap_words = ?self.tick_bitmap.len(),
            ticks_count = ?self.ticks.len(),
            "Pool fully initialized with all tick data"
        );

        Ok(self)
    }

    async fn update<N, P>(&mut self, _provider: P) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        Ok(())
    }
}

// ========== EkuboPool Implementation ==========

impl EkuboPool {
    pub fn new(address: Address, pool_key: EkuboPoolKey) -> Self {
        let pool_id = pool_key.pool_id();
        EkuboPool {
            address,
            pool_key,
            pool_id,
            token_a_price: 0.0,
            token_b_price: 0.0,
            ..Default::default()
        }
    }

    pub fn pool_id(&self) -> B256 {
        self.pool_key.pool_id()
    }

    /// Internal helper to fetch core state (sqrt_price, tick, liquidity) from chain
    /// Does NOT fetch tick data - used by init() and init_batch()
    pub(super) async fn fetch_core_state<N, P>(
        mut self,
        block_number: BlockId,
        provider: P,
    ) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        // CoreDataFetcher 地址 (V2 版本)
        let data_fetcher_addr = address!("208bb00c6b142351e4a431f6dd323691ebb7c285");
        let data_fetcher = CoreDataFetcher::new(data_fetcher_addr, provider.clone());

        // 解析 PoolConfig
        let pool_config = self.pool_key.parse_config();
        self.fee = pool_config.fee as u128;
        self.tick_spacing = pool_config.tick_spacing;

        // 使用 CoreDataFetcher.poolState() 查询池状态
        let config_bytes: alloy::primitives::FixedBytes<32> =
            alloy::primitives::FixedBytes::from_slice(&self.pool_key.config.to_be_bytes::<32>());
        let pool_key = CoreDataFetcher::PoolKey {
            token0: self.pool_key.token0,
            token1: self.pool_key.token1,
            config: config_bytes,
        };

        // 1. 获取 liquidity (从 poolState)
        let pool_state = data_fetcher
            .poolState(pool_key.clone())
            .block(block_number)
            .call()
            .await
            .map_err(|e| AMMError::Msg(format!("CoreDataFetcher.poolState failed: {}", e)))?;

        // 2. 获取精确价格 (从 poolPrice)
        // 注意: poolState 返回的 sqrtRatio 可能是内部格式或有误，使用 poolPrice 的 Q64.128 结果更可靠
        // 更进一步: 为了保证与 sync 逻辑的一致性，且避免链上 sqrtRatio 格式（uint96/float/Q64.96）的歧义，
        // 我们统一使用 tick 来计算 sqrt_price。Tick 是 Ekubo 中最可靠的状态源。
        let pool_price = data_fetcher
            .poolPrice(pool_key)
            .block(block_number)
            .call()
            .await
            .map_err(|e| AMMError::Msg(format!("CoreDataFetcher.poolPrice failed: {}", e)))?;

        self.tick = pool_price.tick;
        // 强制使用 tick 计算 sqrt_price，确保与 sync 逻辑一致，且避免格式解析错误
        self.sqrt_price = self.get_sqrt_ratio_at_tick(self.tick)?;
        self.liquidity = pool_state.liquidity;

        // 获取 token 信息
        self.token_a = if self.pool_key.token0 == Address::ZERO {
            Token {
                address: Address::ZERO,
                decimals: 18,
                symbol: "ETH".to_string(),
                chain_id: 1,
            }
        } else {
            Token::new(self.pool_key.token0, provider.clone()).await?
        };

        self.token_b = if self.pool_key.token1 == Address::ZERO {
            Token {
                address: Address::ZERO,
                decimals: 18,
                symbol: "ETH".to_string(),
                chain_id: 1,
            }
        } else {
            Token::new(self.pool_key.token1, provider.clone()).await?
        };

        // 计算价格缓存
        if let Ok(p) = self.calculate_price(self.token_a.address, self.token_b.address) {
            self.token_a_price = p;
            if p != 0.0 {
                self.token_b_price = 1.0 / p;
            } else {
                self.token_b_price = 0.0;
            }
        }

        Ok(self)
    }

    /// Update tick data from PositionUpdated event
    /// This handles both liquidity_gross and liquidity_net updates, plus tick_bitmap flipping
    fn update_tick_from_event(&mut self, tick: i32, liquidity_delta: i128, is_upper: bool) {
        let info = self.ticks.entry(tick).or_default();

        let liquidity_gross_before = info.liquidity_gross;

        // Update liquidity_gross
        let liquidity_gross_after = if liquidity_delta < 0 {
            liquidity_gross_before.saturating_sub((-liquidity_delta) as u128)
        } else {
            liquidity_gross_before.saturating_add(liquidity_delta as u128)
        };

        // Check if initialization status flipped
        let flipped = (liquidity_gross_after == 0) != (liquidity_gross_before == 0);

        // Update initialized flag
        if liquidity_gross_before == 0 && liquidity_gross_after > 0 {
            info.initialized = true;
        } else if liquidity_gross_after == 0 {
            info.initialized = false;
        }

        info.liquidity_gross = liquidity_gross_after;

        // Update liquidity_net
        info.liquidity_net = if is_upper {
            info.liquidity_net.saturating_sub(liquidity_delta)
        } else {
            info.liquidity_net.saturating_add(liquidity_delta)
        };

        // Update tick_bitmap if initialization status changed
        if flipped {
            self.flip_tick(tick, liquidity_gross_after > 0);
        }

        // Remove tick entry if it's now empty
        if liquidity_gross_after == 0 {
            self.ticks.remove(&tick);
        }
    }

    /// Get the next sqrt ratio target based on tick bitmap
    fn get_next_sqrt_ratio_target(
        &self,
        current_tick: i32,
        zero_for_one: bool,
        sqrt_price_limit: U256,
    ) -> U256 {
        // Try to find next initialized tick from bitmap
        if let Some(next_tick) = self.find_next_initialized_tick(current_tick, zero_for_one) {
            // Convert tick to sqrt ratio
            if let Ok(sqrt_ratio) = self.get_sqrt_ratio_at_tick(next_tick) {
                // Clamp to limit
                if zero_for_one {
                    if sqrt_ratio > sqrt_price_limit {
                        return sqrt_ratio;
                    }
                } else {
                    if sqrt_ratio < sqrt_price_limit {
                        return sqrt_ratio;
                    }
                }
            }
        }

        // Fall back to price limit
        sqrt_price_limit
    }

    /// Find next initialized tick from the tick bitmap
    fn find_next_initialized_tick(&self, current_tick: i32, zero_for_one: bool) -> Option<i32> {
        let compressed = current_tick.div_euclid(self.tick_spacing);

        if zero_for_one {
            // Search downwards
            for offset in 0..256 {
                let search_tick = (compressed - offset) * self.tick_spacing;
                if search_tick < MIN_TICK {
                    return None;
                }
                if let Some(tick_info) = self.ticks.get(&search_tick) {
                    if tick_info.initialized {
                        return Some(search_tick);
                    }
                }
            }
        } else {
            // Search upwards
            for offset in 1..256 {
                let search_tick = (compressed + offset) * self.tick_spacing;
                if search_tick > MAX_TICK {
                    return None;
                }
                if let Some(tick_info) = self.ticks.get(&search_tick) {
                    if tick_info.initialized {
                        return Some(search_tick);
                    }
                }
            }
        }

        None
    }

    pub fn sqrt_ratio_from_tick(tick: i32) -> Result<U256, AMMError> {
        let abs_tick = if tick < 0 {
            -(tick as i64)
        } else {
            tick as i64
        };

        let base = Float::with_val(MPFR_T_PRECISION, TICK_BASE);
        let mut ratio = base.pow(abs_tick);
        if tick < 0 {
            let one = Float::with_val(MPFR_T_PRECISION, 1);
            ratio = one / ratio;
        }
        let sqrt_ratio = ratio.sqrt();

        let q128: rug::Integer = rug::Integer::from(1) << 128;
        let result = sqrt_ratio * Float::with_val(MPFR_T_PRECISION, q128);

        let result_int = result
            .to_integer()
            .ok_or_else(|| AMMError::Msg("Failed to convert sqrt ratio to integer".to_string()))?;
        let result_str = result_int.to_string();
        U256::from_str_radix(&result_str, 10)
            .map_err(|e| AMMError::Msg(format!("Failed to parse U256: {}", e)))
    }

    pub fn tick_from_sqrt_ratio(sqrt_price: U256) -> Result<i32, AMMError> {
        let sqrt_price_str = sqrt_price.to_string();
        let sqrt_price_val = Float::parse_radix(&sqrt_price_str, 10)
            .map_err(|e| AMMError::Msg(format!("Failed to parse sqrt price: {}", e)))?;
        let sqrt_price_float = Float::with_val(MPFR_T_PRECISION, sqrt_price_val);

        let q128: Float = Float::with_val(MPFR_T_PRECISION, 2_f64).pow(128);
        let price: Float = (sqrt_price_float / q128).pow(2);

        let tick_base_float = Float::with_val(MPFR_T_PRECISION, TICK_BASE);
        let log_price = price.ln() / tick_base_float.ln();
        Ok(log_price.to_f64().floor() as i32)
    }

    fn get_sqrt_ratio_at_tick(&self, tick: i32) -> Result<U256, AMMError> {
        Self::sqrt_ratio_from_tick(tick)
    }

    /// Get tick at a given sqrt ratio
    fn get_tick_at_sqrt_ratio(&self, sqrt_price: U256) -> Result<i32, AMMError> {
        Self::tick_from_sqrt_ratio(sqrt_price)
    }

    /// 修改流动性位置
    pub fn modify_position(
        &mut self,
        tick_lower: i32,
        tick_upper: i32,
        liquidity_delta: i128,
    ) -> Result<(), AMMError> {
        self.update_position(tick_lower, tick_upper, liquidity_delta)?;

        if liquidity_delta != 0 {
            if self.tick >= tick_lower && self.tick < tick_upper {
                self.liquidity = if liquidity_delta < 0 {
                    self.liquidity
                        .checked_sub((-liquidity_delta) as u128)
                        .ok_or_else(|| AMMError::Msg("Liquidity underflow".into()))?
                } else {
                    self.liquidity
                        .checked_add(liquidity_delta as u128)
                        .ok_or_else(|| AMMError::Msg("Liquidity overflow".into()))?
                }
            }
        }

        Ok(())
    }

    /// 更新位置
    pub fn update_position(
        &mut self,
        tick_lower: i32,
        tick_upper: i32,
        liquidity_delta: i128,
    ) -> Result<(), AMMError> {
        let mut flipped_lower = false;
        let mut flipped_upper = false;

        if liquidity_delta != 0 {
            flipped_lower = self.update_tick(tick_lower, liquidity_delta, false)?;
            flipped_upper = self.update_tick(tick_upper, liquidity_delta, true)?;
            if flipped_lower {
                self.flip_tick(tick_lower, liquidity_delta > 0);
            }
            if flipped_upper {
                self.flip_tick(tick_upper, liquidity_delta > 0);
            }
        }

        if liquidity_delta < 0 {
            if flipped_lower {
                self.ticks.remove(&tick_lower);
            }
            if flipped_upper {
                self.ticks.remove(&tick_upper);
            }
        }

        Ok(())
    }

    /// 更新 tick
    pub fn update_tick(
        &mut self,
        tick: i32,
        liquidity_delta: i128,
        upper: bool,
    ) -> Result<bool, AMMError> {
        let info = self.ticks.entry(tick).or_default();

        let liquidity_gross_before = info.liquidity_gross;

        let liquidity_gross_after = if liquidity_delta < 0 {
            liquidity_gross_before
                .checked_sub((-liquidity_delta) as u128)
                .ok_or_else(|| AMMError::Msg("Liquidity gross underflow".into()))?
        } else {
            liquidity_gross_before
                .checked_add(liquidity_delta as u128)
                .ok_or_else(|| AMMError::Msg("Liquidity gross overflow".into()))?
        };

        let flipped = (liquidity_gross_after == 0) != (liquidity_gross_before == 0);

        if liquidity_gross_before == 0 {
            info.initialized = true;
        }

        info.liquidity_gross = liquidity_gross_after;

        info.liquidity_net = if upper {
            info.liquidity_net
                .checked_sub(liquidity_delta)
                .ok_or_else(|| {
                    AMMError::Msg(format!(
                        "Liquidity net underflow at tick {}: {} - {}",
                        tick, info.liquidity_net, liquidity_delta
                    ))
                })?
        } else {
            info.liquidity_net
                .checked_add(liquidity_delta)
                .ok_or_else(|| {
                    AMMError::Msg(format!(
                        "Liquidity net overflow at tick {}: {} + {}",
                        tick, info.liquidity_net, liquidity_delta
                    ))
                })?
        };

        Ok(flipped)
    }

    /// Flip tick in bitmap
    pub fn flip_tick(&mut self, tick: i32, initialized: bool) {
        // Ekubo V2 uses an offset of 89421695
        // constant 89421695
        const BITMAP_OFFSET: i64 = 89421695;

        // Use i64 for calculation to avoid overflow
        let compressed = (tick as i64).div_euclid(self.tick_spacing as i64);
        let raw_index = compressed + BITMAP_OFFSET;

        let word_pos = (raw_index / 256) as i32;
        let bit_pos = (raw_index % 256) as u8;
        let mask = U256::from(1) << bit_pos;

        if let Some(word) = self.tick_bitmap.get_mut(&word_pos) {
            if initialized {
                *word |= mask;
            } else {
                *word &= !mask;
            }
        } else if initialized {
            self.tick_bitmap.insert(word_pos, mask);
        }
    }
}

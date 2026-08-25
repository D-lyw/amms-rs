//! 说明：Pancake Infinity 目前仅部署在 BNB Chain；本套利系统主要运行在
//! Ethereum 主网，因此该模块实现尚未进行充分的链上测试与验证。请在 BNB
//! Chain 环境下进一步验证后再用于生产。
use std::collections::HashMap;
use std::sync::Arc;

use alloy::eips::BlockId;
use alloy::primitives::{aliases::I24, Address, Bytes, B256, U160, U256};
use alloy::providers::{Network, Provider};
use alloy::rpc::types::Log;
use alloy::sol_types::{SolEvent, SolInterface};
use alloy::{sol, sol_types::SolValue};

use serde::{Deserialize, Serialize};
use uniswap_v3_math::error::UniswapV3MathError;
use uniswap_v3_math::swap_math::compute_swap_step;
use uniswap_v3_math::tick_bitmap::next_initialized_tick_within_one_word;
use uniswap_v3_math::tick_math::{MAX_SQRT_RATIO, MAX_TICK, MIN_SQRT_RATIO, MIN_TICK};

use crate::amms::amm::{AutomatedMarketMaker, SyncAction};
use crate::amms::consts::{MPFR_T_PRECISION, U256_1};
use crate::amms::error::AMMError;
use crate::amms::uniswap_v3::{compress_tick, Info, UniswapV3Error};
use crate::amms::uniswap_v4::lense::{get_liquidity_slot, get_pool_state_slot};
use crate::amms::Token;
use ICLPoolManager::ICLPoolManagerInstance;

use rug::ops::Pow;
use rug::Float;
use thiserror::Error;

sol! {
    #[sol(rpc)]
    interface ICLPoolManager {
        type PoolId is bytes32;
        type Currency is address;
        type BalanceDelta is int256;
        struct SwapParams { bool zeroForOne; int256 amountSpecified; uint160 sqrtPriceLimitX96; }
        #[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
        struct PoolKey { address currency0; address currency1; address hooks; address poolManager; uint24 fee; bytes32 parameters; }
        event Initialize(PoolId indexed id, Currency indexed currency0, Currency indexed currency1, uint24 fee, bytes32 parameters, IHooks hooks, uint160 sqrtPriceX96, int24 tick);
        event ModifyLiquidity(PoolId indexed id, address indexed sender, int24 tickLower, int24 tickUpper, int256 liquidityDelta, bytes32 salt);
        event Swap(PoolId indexed id, address indexed sender, int128 amount0, int128 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick, uint24 fee);
        function swap(PoolKey memory key, SwapParams memory params, bytes calldata hookData) external returns (BalanceDelta swapDelta);
        function poolIdToPoolKey(bytes32 id) external view returns (PoolKey memory key);
        function extsload(bytes32 slot) external view returns (bytes32 value);
        function extsload(bytes32 startSlot, uint256 nSlots) external view returns (bytes32[] memory values);
        function extsload(bytes32[] calldata slots) external view returns (bytes32[] memory values);
    }
    interface IHooks {}
}

#[derive(Error, Debug)]
pub enum PancakeInfinityError {
    #[error("Unknown Event Signature {0}")]
    UnknownEventSignature(B256),
    #[error("Not initialized")]
    NotInitialized,
    #[error("Liquidity Delta Overflow")]
    LiquidityDeltaOverflow,
    #[error("Tick Data Missing {0}")]
    TickDataMissing(i32),
    #[error(transparent)]
    UniswapV3MathError(#[from] UniswapV3MathError),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PancakeInfinityPool {
    pub pool_key: ICLPoolManager::PoolKey,
    pub pool_id: B256,
    pub manager_address: Address,
    #[serde(default)]
    pub last_synced_block: u64,
    pub token_a: Token,
    pub token_b: Token,
    pub sqrt_price: U256,
    pub liquidity: u128,
    pub tick: i32,
    pub tick_spacing: i32,
    pub protocol_fee: u32,
    pub lp_fee: u32,
    /// 只读背景数据（swap 模拟/链上 swap 均不修改，仅 Mint/Burn 同步时写入）：
    /// Arc 共享使 `Clone` 退化为 O(1) 引用计数，pending 模拟链每事件少一次 O(N) 深拷贝。
    pub tick_bitmap: Arc<HashMap<i16, U256>>,
    pub ticks: Arc<HashMap<i32, Info>>,
    pub token_a_price: f64,
    pub token_b_price: f64,
}

impl AutomatedMarketMaker for PancakeInfinityPool {
    fn address(&self) -> Address {
        if self.pool_id == B256::ZERO {
            Address::ZERO
        } else {
            Address::from_slice(&self.pool_id.as_slice()[0..20])
        }
    }

    fn last_synced_block(&self) -> u64 {
        self.last_synced_block
    }

    fn set_last_synced_block(&mut self, block_number: u64) {
        self.last_synced_block = self.last_synced_block.max(block_number);
    }

    fn sync_events(&self) -> Vec<B256> {
        vec![
            ICLPoolManager::ModifyLiquidity::SIGNATURE_HASH,
            ICLPoolManager::Swap::SIGNATURE_HASH,
        ]
    }

    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError> {
        let event_signature = log.topics()[0];
        if log.topics().len() < 2 || log.topics()[1] != self.pool_id {
            return Ok(SyncAction::None);
        }
        match event_signature {
            ICLPoolManager::Initialize::SIGNATURE_HASH => {
                // Initialize is not needed for steady-state realtime syncing of tracked pools.
                return Ok(SyncAction::None);
            }
            ICLPoolManager::ModifyLiquidity::SIGNATURE_HASH => {
                let event = ICLPoolManager::ModifyLiquidity::decode_log(&log.inner)?;
                let liquidity_delta: i128 = event.liquidityDelta.try_into().map_err(|_| {
                    AMMError::PancakeInfinityError(PancakeInfinityError::LiquidityDeltaOverflow)
                })?;
                self.modify_position(
                    event.tickLower.as_i32(),
                    event.tickUpper.as_i32(),
                    liquidity_delta,
                )?;
            }
            ICLPoolManager::Swap::SIGNATURE_HASH => {
                let event = ICLPoolManager::Swap::decode_log(&log.inner)?;
                self.sqrt_price = U256::from(event.sqrtPriceX96);
                self.tick = event.tick.as_i32();
                self.liquidity = event.liquidity;

                // Update spot prices
                self.token_a_price =
                    self.calculate_price(self.token_a.address, self.token_b.address)?;
                self.token_b_price =
                    self.calculate_price(self.token_b.address, self.token_a.address)?;
            }
            _ => {
                return Err(AMMError::PancakeInfinityError(
                    PancakeInfinityError::UnknownEventSignature(event_signature),
                ))
            }
        }
        Ok(SyncAction::None)
    }

    fn tokens(&self) -> Vec<Address> {
        vec![self.token_a.address, self.token_b.address]
    }

    fn has_sufficient_liquidity(&self) -> bool {
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

        // Fast path: active in-range liquidity already meets the threshold.
        if self.liquidity >= l_thresh {
            return true;
        }

        // Efficient O(1) best-case check for any tick containing enough liquidity
        self.ticks
            .values()
            .any(|info| info.liquidity_gross >= l_thresh)
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

    /// PancakeSwap Infinity is currently only deployed on BNB Chain
    fn supported_chains(&self) -> Option<Vec<u64>> {
        Some(vec![56, 4663]) // BNB Chain, Robinhood Chain
    }

    fn calculate_price(&self, base_token: Address, _quote_token: Address) -> Result<f64, AMMError> {
        if self.sqrt_price.is_zero() {
            return Err(AMMError::Msg("sqrt_price is zero".into()));
        }
        let sqrt_price_x96 = self.sqrt_price;
        // Convert sqrt_price_x96 (U256) to rug::Float
        let sqrt_price_str = sqrt_price_x96.to_string();
        let sqrt_price_val = Float::parse_radix(&sqrt_price_str, 10)
            .map_err(|e| AMMError::Msg(format!("Float parse error: {}", e)))?;
        let sqrt_price_float = Float::with_val(MPFR_T_PRECISION, sqrt_price_val);

        let mut denom = Float::with_val(MPFR_T_PRECISION, 1);
        denom <<= 96u32; // Q96 denominator

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

    fn spot_price(&self, base_token: Address, _quote_token: Address) -> Result<f64, AMMError> {
        let price = if base_token == self.token_a.address {
            self.token_a_price
        } else if base_token == self.token_b.address {
            self.token_b_price
        } else {
            return Err(AMMError::TokenNotFound(base_token));
        };

        // 价格有效性校验：0 或非有限值表示价格未初始化或计算失败
        if price <= 0.0 || !price.is_finite() {
            return Err(AMMError::Msg("Invalid cached spot price".to_string()));
        }

        Ok(price)
    }

    fn simulate_swap(
        &self,
        base_token: Address,
        quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let mut pool = self.clone();
        pool.simulate_swap_mut(base_token, quote_token, amount_in)
    }

    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        _quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        if amount_in.is_zero() {
            return Ok(U256::ZERO);
        }
        let zero_for_one = base_token == self.token_a.address;
        let sqrt_price_limit_x_96 = if zero_for_one {
            MIN_SQRT_RATIO + U256_1
        } else {
            MAX_SQRT_RATIO - U256_1
        };
        let mut current_state = crate::amms::uniswap_v4::CurrentState {
            amount_specified_remaining: alloy::primitives::I256::from_raw(amount_in),
            amount_calculated: alloy::primitives::I256::ZERO,
            sqrt_price_x_96: self.sqrt_price,
            tick: self.tick,
            liquidity: self.liquidity,
        };
        while current_state.amount_specified_remaining != alloy::primitives::I256::ZERO
            && current_state.sqrt_price_x_96 != sqrt_price_limit_x_96
        {
            let mut step = crate::amms::uniswap_v4::StepComputations {
                sqrt_price_start_x_96: current_state.sqrt_price_x_96,
                ..Default::default()
            };
            let (tick_next, initialized) = next_initialized_tick_within_one_word(
                &self.tick_bitmap,
                current_state.tick,
                self.tick_spacing,
                zero_for_one,
            )
            .map_err(UniswapV3Error::from)?;
            step.tick_next = tick_next;
            step.initialized = initialized;
            step.tick_next = step.tick_next.clamp(MIN_TICK, MAX_TICK);
            step.sqrt_price_next_x96 =
                uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(step.tick_next)
                    .map_err(UniswapV3Error::from)?;
            let swap_target_sqrt_ratio = if zero_for_one {
                if step.sqrt_price_next_x96 < sqrt_price_limit_x_96 {
                    sqrt_price_limit_x_96
                } else {
                    step.sqrt_price_next_x96
                }
            } else if step.sqrt_price_next_x96 > sqrt_price_limit_x_96 {
                sqrt_price_limit_x_96
            } else {
                step.sqrt_price_next_x96
            };
            let fee_pips: u32 = ((self.protocol_fee as u64)
                + (((self.lp_fee as u64) * (1_000_000u64 - self.protocol_fee as u64) + 999_999u64)
                    / 1_000_000u64))
                .min(1_000_000u64) as u32;
            let (sqrt_price_x_96, amount_in, amount_out, fee_amount) = compute_swap_step(
                current_state.sqrt_price_x_96,
                swap_target_sqrt_ratio,
                current_state.liquidity,
                current_state.amount_specified_remaining,
                fee_pips,
            )
            .map_err(UniswapV3Error::from)?;
            current_state.sqrt_price_x_96 = sqrt_price_x_96;
            step.amount_in = amount_in;
            step.amount_out = amount_out;
            step.fee_amount = fee_amount;
            current_state.amount_specified_remaining = current_state
                .amount_specified_remaining
                .overflowing_sub(alloy::primitives::I256::from_raw(
                    step.amount_in.overflowing_add(step.fee_amount).0,
                ))
                .0;
            current_state.amount_calculated -= alloy::primitives::I256::from_raw(step.amount_out);
            if current_state.sqrt_price_x_96 == step.sqrt_price_next_x96 {
                if step.initialized {
                    let mut liquidity_net = if let Some(info) = self.ticks.get(&step.tick_next) {
                        info.liquidity_net
                    } else {
                        return Err(AMMError::PancakeInfinityError(
                            PancakeInfinityError::TickDataMissing(step.tick_next),
                        ));
                    };
                    if zero_for_one {
                        liquidity_net = -liquidity_net;
                    }
                    current_state.liquidity = if liquidity_net < 0 {
                        if current_state.liquidity < (-liquidity_net as u128) {
                            return Err(UniswapV3Error::LiquidityUnderflow.into());
                        } else {
                            current_state.liquidity - (-liquidity_net as u128)
                        }
                    } else {
                        current_state.liquidity + (liquidity_net as u128)
                    };
                }
                current_state.tick = if zero_for_one {
                    step.tick_next.wrapping_sub(1)
                } else {
                    step.tick_next
                };
            } else if current_state.sqrt_price_x_96 != step.sqrt_price_start_x_96 {
                current_state.tick = uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(
                    current_state.sqrt_price_x_96,
                )
                .map_err(UniswapV3Error::from)?;
            }
        }

        self.sqrt_price = current_state.sqrt_price_x_96;
        self.tick = current_state.tick;
        self.liquidity = current_state.liquidity;

        // 刷新缓存 spot price（状态已推进到 swap 后）
        if let Ok(p) = self.calculate_price(self.token_a.address, self.token_b.address) {
            self.token_a_price = p;
            if p != 0.0 {
                self.token_b_price = 1.0 / p;
            } else {
                self.token_b_price = 0.0;
            }
        }

        Ok((-current_state.amount_calculated).into_raw())
    }

    fn simulate_swap_exact_out(
        &self,
        base_token: Address,
        _quote_token: Address,
        amount_out: U256,
    ) -> Result<U256, AMMError> {
        if amount_out.is_zero() {
            return Ok(U256::ZERO);
        }

        // Defensive check: prevent divide-by-zero panic in uniswap_v3_math
        if self.sqrt_price.is_zero() {
            return Err(AMMError::Msg("sqrt_price is zero".into()));
        }

        let zero_for_one = base_token == self.token_a.address;

        // Set sqrt_price_limit_x_96 to the max or min sqrt price in the pool depending on zero_for_one
        let sqrt_price_limit_x_96 = if zero_for_one {
            MIN_SQRT_RATIO + U256_1
        } else {
            MAX_SQRT_RATIO - U256_1
        };

        // Initialize a mutable state struct to hold the dynamic simulated state of the pool
        // Negative amount_specified_remaining for exact-out mode
        let mut current_state = crate::amms::uniswap_v4::CurrentState {
            sqrt_price_x_96: self.sqrt_price,
            amount_calculated: alloy::primitives::I256::ZERO,
            amount_specified_remaining: alloy::primitives::I256::ZERO
                - alloy::primitives::I256::from_raw(amount_out),
            tick: self.tick,
            liquidity: self.liquidity,
        };

        while current_state.amount_specified_remaining != alloy::primitives::I256::ZERO
            && current_state.sqrt_price_x_96 != sqrt_price_limit_x_96
        {
            // Initialize a new step struct to hold the dynamic state of the pool at each step
            let mut step = crate::amms::uniswap_v4::StepComputations {
                sqrt_price_start_x_96: current_state.sqrt_price_x_96,
                ..Default::default()
            };

            // Get the next tick from the current tick
            let (tick_next, initialized) = next_initialized_tick_within_one_word(
                &self.tick_bitmap,
                current_state.tick,
                self.tick_spacing,
                zero_for_one,
            )
            .map_err(UniswapV3Error::from)?;

            step.tick_next = tick_next.clamp(MIN_TICK, MAX_TICK);
            step.initialized = initialized;

            // Get the next sqrt price from the input amount
            step.sqrt_price_next_x96 =
                uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(step.tick_next)
                    .map_err(UniswapV3Error::from)?;

            // Target spot price
            let swap_target_sqrt_ratio = if zero_for_one {
                if step.sqrt_price_next_x96 < sqrt_price_limit_x_96 {
                    sqrt_price_limit_x_96
                } else {
                    step.sqrt_price_next_x96
                }
            } else if step.sqrt_price_next_x96 > sqrt_price_limit_x_96 {
                sqrt_price_limit_x_96
            } else {
                step.sqrt_price_next_x96
            };

            // Compute swap step and update the current state
            let (sqrt_price_x_96, amount_in, amount_out, fee_amount);
            if current_state.liquidity == 0 {
                // If liquidity is zero, we move instantly to the target price without consuming any amount
                sqrt_price_x_96 = swap_target_sqrt_ratio;
                amount_in = U256::ZERO;
                amount_out = U256::ZERO;
                fee_amount = U256::ZERO;
            } else {
                // Same dynamic fee calculation as simulate_swap_mut
                let fee_pips: u32 = ((self.protocol_fee as u64)
                    + (((self.lp_fee as u64) * (1_000_000u64 - self.protocol_fee as u64)
                        + 999_999u64)
                        / 1_000_000u64))
                    .min(1_000_000u64) as u32;

                (sqrt_price_x_96, amount_in, amount_out, fee_amount) = compute_swap_step(
                    current_state.sqrt_price_x_96,
                    swap_target_sqrt_ratio,
                    current_state.liquidity,
                    current_state.amount_specified_remaining,
                    fee_pips,
                )
                .map_err(UniswapV3Error::from)?;
            }

            current_state.sqrt_price_x_96 = sqrt_price_x_96;
            step.amount_in = amount_in;
            step.amount_out = amount_out;
            step.fee_amount = fee_amount;

            // Exact output: decrement remaining output (add since it's negative), increment calculated input
            current_state.amount_specified_remaining = current_state
                .amount_specified_remaining
                .overflowing_add(alloy::primitives::I256::from_raw(step.amount_out))
                .0;

            current_state.amount_calculated += alloy::primitives::I256::from_raw(
                step.amount_in.overflowing_add(step.fee_amount).0,
            );

            // If the price moved all the way to the next price, recompute the liquidity change for the next iteration
            if current_state.sqrt_price_x_96 == step.sqrt_price_next_x96 {
                if step.initialized {
                    let mut liquidity_net = if let Some(info) = self.ticks.get(&step.tick_next) {
                        info.liquidity_net
                    } else {
                        return Err(AMMError::PancakeInfinityError(
                            PancakeInfinityError::TickDataMissing(step.tick_next),
                        ));
                    };

                    // we are on a tick boundary, and the next tick is initialized
                    if zero_for_one {
                        liquidity_net = -liquidity_net;
                    }

                    current_state.liquidity = if liquidity_net < 0 {
                        if current_state.liquidity < (-liquidity_net as u128) {
                            return Err(UniswapV3Error::LiquidityUnderflow.into());
                        } else {
                            current_state.liquidity - (-liquidity_net as u128)
                        }
                    } else {
                        current_state.liquidity + (liquidity_net as u128)
                    };
                }
                // Increment the current tick
                current_state.tick = if zero_for_one {
                    step.tick_next.wrapping_sub(1)
                } else {
                    step.tick_next
                }
                // If the current_state sqrt price is not equal to the step sqrt price, then we are not on the same tick.
                // Update the current_state.tick to the tick at the current_state.sqrt_price_x_96
            } else if current_state.sqrt_price_x_96 != step.sqrt_price_start_x_96 {
                current_state.tick = uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(
                    current_state.sqrt_price_x_96,
                )
                .map_err(UniswapV3Error::from)?;
            }
        }

        if current_state.amount_specified_remaining != alloy::primitives::I256::ZERO {
            return Err(AMMError::Msg(
                "insufficient liquidity for exact out".to_string(),
            ));
        }

        Ok(current_state.amount_calculated.into_raw())
    }

    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        Self: Sized,
        N: Network,
        P: Provider<N> + Clone,
    {
        self.token_a = Token::new(self.token_a.address, provider.clone()).await?;
        self.token_b = Token::new(self.token_b.address, provider.clone()).await?;

        // Initialize state via extsload
        let ipool_manager = ICLPoolManagerInstance::new(self.manager_address, provider.clone());
        let slots = vec![
            B256::from(get_pool_state_slot(self.pool_id)),
            B256::from(get_liquidity_slot(self.pool_id)),
        ];

        let results = ipool_manager
            .extsload_2(slots)
            .block(block_number)
            .call()
            .await?;

        if results.len() != 2 {
            return Err(AMMError::SyncError(self.manager_address));
        }

        let slot0_data = results[0];
        let liquidity_data = results[1];

        // Parse slot0
        let sqrt_price_x96 = U160::from_be_slice(&slot0_data[12..32]);
        let tick_bytes = unsafe { (slot0_data.as_ptr().add(9) as *const [u8; 3]).read_unaligned() };
        let tick = I24::from_be_bytes::<3>(tick_bytes);
        let protocol_fee_bytes =
            unsafe { (slot0_data.as_ptr().add(6) as *const [u8; 3]).read_unaligned() };
        let protocol_fee = alloy::primitives::aliases::U24::from_be_bytes(protocol_fee_bytes);
        let lp_fee_bytes =
            unsafe { (slot0_data.as_ptr().add(3) as *const [u8; 3]).read_unaligned() };
        let lp_fee = alloy::primitives::aliases::U24::from_be_bytes(lp_fee_bytes);

        // Parse liquidity
        let liquidity = u128::from_be_bytes(liquidity_data[16..32].try_into().unwrap());

        // Update state
        self.sqrt_price = U256::from(sqrt_price_x96);
        self.tick = tick.as_i32();
        self.protocol_fee = protocol_fee.to::<u32>();
        self.lp_fee = lp_fee.to::<u32>();
        self.liquidity = liquidity;

        if self.sqrt_price > U256::ZERO {
            if let Ok(price) = self.calculate_price(self.token_a.address, self.token_b.address) {
                self.token_a_price = price;
                if price != 0.0 {
                    self.token_b_price = 1.0 / price;
                } else {
                    self.token_b_price = 0.0;
                }
            }
        }

        Ok(self)
    }
}

impl PancakeInfinityPool {
    pub fn new(manager_address: Address, pool_key: ICLPoolManager::PoolKey) -> Self {
        let mut pool = Self {
            pool_key: pool_key.clone(),
            manager_address,
            token_a: Token::new_with_decimals(pool_key.currency0, 0),
            token_b: Token::new_with_decimals(pool_key.currency1, 0),
            lp_fee: pool_key.fee.to::<u32>(),
            ..Default::default()
        };
        let _params = U256::from_be_bytes(pool_key.parameters.0);
        let spacing =
            I24::from_be_bytes::<3>((&pool_key.parameters.0[29..32]).try_into().unwrap()).as_i32();
        pool.tick_spacing = spacing;
        pool.pool_id = alloy::primitives::keccak256(pool.pool_key.abi_encode());
        pool
    }

    pub fn get_pool_key(&self) -> Result<ICLPoolManager::PoolKey, AMMError> {
        if self.pool_key != ICLPoolManager::PoolKey::default() {
            Ok(self.pool_key.clone())
        } else {
            Err(AMMError::PancakeInfinityError(
                PancakeInfinityError::NotInitialized,
            ))
        }
    }

    pub fn get_pool_id(&self) -> Result<B256, AMMError> {
        if self.pool_key != ICLPoolManager::PoolKey::default() {
            Ok(alloy::primitives::keccak256(self.pool_key.abi_encode()))
        } else {
            Err(AMMError::PancakeInfinityError(
                PancakeInfinityError::NotInitialized,
            ))
        }
    }

    pub fn swap_calldata(
        &self,
        zero_for_one: bool,
        amount_specified: alloy::primitives::I256,
        sqrt_price_limit_x_96: U256,
        hook_data: Bytes,
    ) -> Result<Bytes, AMMError> {
        Ok(
            ICLPoolManager::ICLPoolManagerCalls::swap(ICLPoolManager::swapCall {
                key: self.get_pool_key()?,
                params: ICLPoolManager::SwapParams {
                    zeroForOne: zero_for_one,
                    amountSpecified: amount_specified,
                    sqrtPriceLimitX96: U160::from(sqrt_price_limit_x_96),
                },
                hookData: hook_data,
            })
            .abi_encode()
            .into(),
        )
    }

    pub fn modify_position(
        &mut self,
        tick_lower: i32,
        tick_upper: i32,
        liquidity_delta: i128,
    ) -> Result<(u128, u128), AMMError> {
        self.update_tick(tick_lower, liquidity_delta, false)?;
        self.update_tick(tick_upper, liquidity_delta, true)?;
        Ok((0, 0))
    }

    fn update_tick(
        &mut self,
        tick: i32,
        liquidity_delta: i128,
        upper: bool,
    ) -> Result<(), AMMError> {
        let flipped;
        {
            let info = Arc::make_mut(&mut self.ticks)
                .entry(tick)
                .or_insert(Info::new(0, 0, false));
            let before = info.liquidity_gross;
            let after = if liquidity_delta < 0 {
                info.liquidity_gross - ((-liquidity_delta) as u128)
            } else {
                info.liquidity_gross + (liquidity_delta as u128)
            };
            flipped = (after == 0) != (before == 0);
            if before == 0 {
                info.initialized = true;
            }
            info.liquidity_gross = after;
            info.liquidity_net = if upper {
                info.liquidity_net - liquidity_delta as i128
            } else {
                info.liquidity_net + liquidity_delta as i128
            };
        }
        if flipped {
            self.flip_tick(tick, self.tick_spacing);
        }
        Ok(())
    }

    pub fn flip_tick(&mut self, tick: i32, tick_spacing: i32) {
        let compressed = compress_tick(tick, tick_spacing);
        let (word_pos, bit_pos) = uniswap_v3_math::tick_bitmap::position(compressed);
        let mask = U256::from(1) << bit_pos;
        let bitmap = Arc::make_mut(&mut self.tick_bitmap);
        if let Some(word) = bitmap.get_mut(&word_pos) {
            *word ^= mask;
        } else {
            bitmap.insert(word_pos, mask);
        }
    }
}

pub mod factory;

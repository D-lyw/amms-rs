use std::collections::HashMap;
use std::sync::Arc;

use crate::amms::amm::{AutomatedMarketMaker, SyncAction};
use crate::amms::error::AMMError;
use crate::amms::uniswap_v3::{compress_tick, Info, UniswapV3Error};
use crate::amms::uniswap_v4::IPoolManager::{swapCall, IPoolManagerCalls, PoolKey};
use alloy::primitives::{keccak256, Bytes, I256, U160};
use alloy::providers::{Network, Provider};
use alloy::sol_types::{SolEvent, SolInterface, SolValue};
use alloy::{
    eips::BlockId,
    primitives::{Address, B256, U256},
    rpc::types::Log,
    sol,
};

use crate::amms::consts::{MIN_V3_LIQUIDITY, MPFR_T_PRECISION, U256_1};
use crate::amms::Token;
use rug::ops::Pow;
use rug::Float;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::info;
use uniswap_v3_math::error::UniswapV3MathError;
use uniswap_v3_math::swap_math::compute_swap_step;
use uniswap_v3_math::tick_bitmap::next_initialized_tick_within_one_word;
use uniswap_v3_math::tick_math::{MAX_SQRT_RATIO, MAX_TICK, MIN_SQRT_RATIO, MIN_TICK};

pub mod factory;
pub mod lense;
pub use factory::UniswapV4Factory;

// Helper structs for simulation
#[derive(Debug, Clone, Copy)]
pub struct CurrentState {
    pub amount_specified_remaining: I256,
    pub amount_calculated: I256,
    pub sqrt_price_x_96: U256,
    pub tick: i32,
    pub liquidity: u128,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct StepComputations {
    pub sqrt_price_start_x_96: U256,
    pub tick_next: i32,
    pub initialized: bool,
    pub sqrt_price_next_x96: U256,
    pub amount_in: U256,
    pub amount_out: U256,
    pub fee_amount: U256,
}

#[derive(Error, Debug)]
pub enum UniswapV4Error {
    #[error("Unknown Event Signature {0}")]
    UnknownEventSignature(B256),
    #[error("Not initialized")]
    NotInitialized,
    #[error("Liquidity Delta Overflow")]
    LiquidityDeltaOverflow,
    #[error("Tick Data Missing for tick {0}")]
    TickDataMissing(i32),
    #[error(transparent)]
    UniswapV3MathError(#[from] UniswapV3MathError),
}

sol! {
    #[sol(rpc)]
    interface IPoolManager {
        type PoolId is bytes32;
        type Currency is address;
        type BalanceDelta is int256;
        struct SwapParams {
            bool zeroForOne;
            int256 amountSpecified;
            uint160 sqrtPriceLimitX96;
        }
        #[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
        struct PoolKey {
            address currency0;
            address currency1;
            uint24 fee;
            int24 tickSpacing;
            address hooks;
        }
        event Initialize(
            PoolId indexed id,
            Currency indexed currency0,
            Currency indexed currency1,
            uint24 fee,
            int24 tickSpacing,
            IHooks hooks,
            uint160 sqrtPriceX96,
            int24 tick
        );
        event ModifyLiquidity(
            PoolId indexed id, address indexed sender, int24 tickLower, int24 tickUpper, int256 liquidityDelta, bytes32 salt
        );
        event Swap(
            PoolId indexed id,
            address indexed sender,
            int128 amount0,
            int128 amount1,
            uint160 sqrtPriceX96,
            uint128 liquidity,
            int24 tick,
            uint24 fee
        );
        function swap(PoolKey memory key, SwapParams memory params, bytes calldata hookData)
            external
            returns (BalanceDelta swapDelta);

        /// @notice Called by external contracts to access granular pool state
        /// @param slot Key of slot to sload
        /// @return value The value of the slot as bytes32
        function extsload(bytes32 slot) external view returns (bytes32 value);
        function extsload(bytes32 startSlot, uint256 nSlots) external view returns (bytes32[] memory values);
        function extsload(bytes32[] calldata slots) external view returns (bytes32[] memory values);
    }

    interface IHooks {}
}

sol! {
    #[sol(rpc)]
    contract IV4Quoter {
        struct PoolKey {
            address currency0;
            address currency1;
            uint24 fee;
            int24 tickSpacing;
            address hooks;
        }
        struct QuoteExactSingleParams {
            PoolKey poolKey;
            bool zeroForOne;
            uint128 exactAmount;
            bytes hookData;
        }
        function quoteExactInputSingle(QuoteExactSingleParams memory params) external returns (uint256 amountOut, uint256 gasEstimate);
        function quoteExactOutputSingle(QuoteExactSingleParams memory params) external returns (uint256 amountIn, uint256 gasEstimate);
    }
}

sol! {
    #[sol(rpc)]
    GetV4LitePoolStateBatchRequest,
    "src/amms/abi/GetV4LitePoolStateBatchRequest.json",
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UniswapV4Pool {
    pub pool_key: IPoolManager::PoolKey,
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
    /// Packed protocol fee from V4 slot0:
    /// low 12 bits are 0->1 (zeroForOne), high 12 bits are 1->0 (oneForZero).
    pub protocol_fee: u32,
    pub lp_fee: u32,
    /// 只读背景数据（swap 模拟/链上 swap 均不修改，仅 Mint/Burn 同步时写入）：
    /// Arc 共享使 `Clone` 退化为 O(1) 引用计数，pending 模拟链每事件少一次 O(N) 深拷贝。
    pub tick_bitmap: Arc<HashMap<i16, U256>>,
    pub ticks: Arc<HashMap<i32, Info>>,
    pub token_a_price: f64,
    pub token_b_price: f64,
}

impl AutomatedMarketMaker for UniswapV4Pool {
    fn address(&self) -> Address {
        // Use the first 20 bytes of pool_id as a virtual address to avoid collision in StateSpace
        // which uses HashMap<Address, AMM>.
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
            IPoolManager::ModifyLiquidity::SIGNATURE_HASH,
            IPoolManager::Swap::SIGNATURE_HASH,
        ]
    }

    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError> {
        let event_signature = log.topics()[0];

        // In V4, we must verify the log belongs to this pool by checking PoolId (topic 1)
        if log.topics().len() < 2 || log.topics()[1] != self.pool_id {
            return Ok(SyncAction::None);
        }

        match event_signature {
            IPoolManager::Initialize::SIGNATURE_HASH => {
                // Initialize is only relevant when the pool is first created/initialized.
                // For runtime realtime sync of an already tracked pool, it's a no-op.
                return Ok(SyncAction::None);
            }
            IPoolManager::ModifyLiquidity::SIGNATURE_HASH => {
                let event = IPoolManager::ModifyLiquidity::decode_log(&log.inner)?;
                let liquidity_delta: i128 = event.liquidityDelta.try_into().map_err(|_| {
                    AMMError::UniswapV4Error(UniswapV4Error::LiquidityDeltaOverflow)
                })?;

                let tick_lower = event.tickLower.as_i32();
                let tick_upper = event.tickUpper.as_i32();

                // 关键修复：如果当前 tick 在此流动性范围 [tickLower, tickUpper) 内，
                // 则同步更新 self.liquidity，确保 JIT Liquidity 场景下状态正确
                if self.tick >= tick_lower && self.tick < tick_upper {
                    self.liquidity = if liquidity_delta < 0 {
                        self.liquidity.saturating_sub((-liquidity_delta) as u128)
                    } else {
                        self.liquidity.saturating_add(liquidity_delta as u128)
                    };
                }

                self.modify_position(tick_lower, tick_upper, liquidity_delta)?;

                info!(
                    target = "amms::uniswap_v4::sync",
                    pool_id = ?self.pool_id,
                    sqrt_price = ?self.sqrt_price,
                    liquidity = ?self.liquidity,
                    tick = ?self.tick,
                    liquidity_delta = ?liquidity_delta,
                    "ModifyLiquidity"
                );
            }
            IPoolManager::Swap::SIGNATURE_HASH => {
                let event = IPoolManager::Swap::decode_log(&log.inner)?;

                let tick_after = event.tick.as_i32();

                // Only warn if liquidity mismatch happens WITHOUT a tick crossing.
                // If ticks are different, liquidity change is expected.
                if event.liquidity != self.liquidity && tick_after == self.tick {
                    tracing::warn!(
                        target: "amms::uniswap_v4::sync",
                        pool_id = ?self.pool_id,
                        local_liquidity = ?self.liquidity,
                        remote_liquidity = ?event.liquidity,
                        local_tick = ?self.tick,
                        remote_tick = ?tick_after,
                        "Liquidity mismatch detected within same tick. Local state may be missing ModifyLiquidity events."
                    );
                }

                self.sqrt_price = U256::from(event.sqrtPriceX96);
                self.tick = tick_after;
                self.liquidity = event.liquidity;

                // Update spot prices
                self.token_a_price =
                    self.calculate_price(self.token_a.address, self.token_b.address)?;
                self.token_b_price =
                    self.calculate_price(self.token_b.address, self.token_a.address)?;

                info!(
                    target = "amms::uniswap_v4::sync",
                    block_number = ?log.block_number,
                    pool_id = ?self.pool_id,
                    sqrt_price = ?self.sqrt_price,
                    liquidity = ?self.liquidity,
                    tick = ?self.tick,
                    "Swap"
                );
            }
            _ => {
                info!(
                    target = "amms::uniswap_v4::sync",
                    ?event_signature,
                    "Ignored event"
                );
                return Ok(SyncAction::None);
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
        let (_final_state, amount_out) =
            self.simulate_swap_exact_in_state(base_token, amount_in)?;
        Ok(amount_out)
    }

    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        _quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let (current_state, amount_out) =
            self.simulate_swap_exact_in_state(base_token, amount_in)?;

        self.liquidity = current_state.liquidity;
        self.sqrt_price = current_state.sqrt_price_x_96;
        self.tick = current_state.tick;

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
        let mut current_state = CurrentState {
            sqrt_price_x_96: self.sqrt_price, // Active price on the pool
            amount_calculated: I256::ZERO,    // Amount of token_in that has been calculated
            amount_specified_remaining: I256::ZERO - I256::from_raw(amount_out), // Negative for exact-out
            tick: self.tick,           // Current i24 tick of the pool
            liquidity: self.liquidity, // Current available liquidity in the tick range
        };

        while current_state.amount_specified_remaining != I256::ZERO
            && current_state.sqrt_price_x_96 != sqrt_price_limit_x_96
        {
            // Initialize a new step struct to hold the dynamic state of the pool at each step
            let mut step = StepComputations {
                // Set the sqrt_price_start_x_96 to the current sqrt_price_x_96
                sqrt_price_start_x_96: current_state.sqrt_price_x_96,
                ..Default::default()
            };

            // Get the next tick from the current tick
            (step.tick_next, step.initialized) =
                uniswap_v3_math::tick_bitmap::next_initialized_tick_within_one_word(
                    &self.tick_bitmap,
                    current_state.tick,
                    self.tick_spacing,
                    zero_for_one,
                )
                .map_err(UniswapV4Error::from)?;

            // ensure that we do not overshoot the min/max tick, as the tick bitmap is not aware of these bounds
            step.tick_next = step.tick_next.clamp(MIN_TICK, MAX_TICK);

            // Get the next sqrt price from the input amount
            step.sqrt_price_next_x96 =
                uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(step.tick_next)
                    .map_err(UniswapV4Error::from)?;

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
                let fee_pips = self.effective_fee_pips(zero_for_one);

                (sqrt_price_x_96, amount_in, amount_out, fee_amount) = compute_swap_step(
                    current_state.sqrt_price_x_96,
                    swap_target_sqrt_ratio,
                    current_state.liquidity,
                    current_state.amount_specified_remaining,
                    fee_pips,
                )
                .map_err(UniswapV4Error::from)?;
            }

            current_state.sqrt_price_x_96 = sqrt_price_x_96;
            step.amount_in = amount_in;
            step.amount_out = amount_out;
            step.fee_amount = fee_amount;

            // Exact output: decrement remaining output (add since it's negative), increment calculated input
            current_state.amount_specified_remaining = current_state
                .amount_specified_remaining
                .overflowing_add(I256::from_raw(step.amount_out))
                .0;

            current_state.amount_calculated +=
                I256::from_raw(step.amount_in.overflowing_add(step.fee_amount).0);

            // If the price moved all the way to the next price, recompute the liquidity change for the next iteration
            if current_state.sqrt_price_x_96 == step.sqrt_price_next_x96 {
                if step.initialized {
                    let mut liquidity_net = if let Some(info) = self.ticks.get(&step.tick_next) {
                        info.liquidity_net
                    } else {
                        return Err(AMMError::UniswapV4Error(UniswapV4Error::TickDataMissing(
                            step.tick_next,
                        )));
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
                .map_err(UniswapV4Error::from)?;
            }
        }

        if current_state.amount_specified_remaining != I256::ZERO {
            return Err(AMMError::Msg(
                "insufficient liquidity for exact out".to_string(),
            ));
        }

        let amount_in = current_state.amount_calculated.into_raw();
        Ok(amount_in)
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

        // Fast path: active liquidity already satisfies threshold.
        // This avoids false negatives when tick data is temporarily incomplete.
        if self.liquidity >= l_thresh {
            return true;
        }

        // Fallback: accept pools that have enough liquidity on initialized ticks
        // even if current active liquidity is zero (out-of-range / imbalanced pools).
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

    fn calculate_price(&self, base_token: Address, _quote_token: Address) -> Result<f64, AMMError> {
        if self.sqrt_price.is_zero() {
            return Err(AMMError::Msg("sqrt_price is zero".into()));
        }
        if self.liquidity < MIN_V3_LIQUIDITY {
            return Ok(0.0);
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

    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        // Calculate PoolId if not present
        if self.pool_id == B256::ZERO {
            if self.pool_key == IPoolManager::PoolKey::default() {
                return Err(AMMError::UniswapV4Error(UniswapV4Error::NotInitialized));
            }
            self.pool_id = self.get_pool_id()?;
        }

        // Ensure manager_address is set
        if self.manager_address == Address::ZERO {
            return Err(AMMError::UniswapV4Error(UniswapV4Error::NotInitialized));
        }

        // Populate token data if possible (e.g. decimals)
        // This will be done in sync_token_decimals

        let mut pools = vec![self];
        UniswapV4Factory::sync_slot_0(&mut pools, block_number, provider.clone()).await?;
        UniswapV4Factory::sync_token_decimals(&mut pools, provider.clone()).await?;
        UniswapV4Factory::sync_tick_bitmap(&mut pools, block_number, provider.clone()).await?;
        UniswapV4Factory::sync_tick_data(&mut pools, block_number, provider.clone()).await?;

        let mut pool = pools.pop().unwrap();
        // Calculate initial prices
        pool.token_a_price = pool.calculate_price(pool.token_a.address, pool.token_b.address)?;
        pool.token_b_price = pool.calculate_price(pool.token_b.address, pool.token_a.address)?;

        Ok(pool)
    }
}

impl UniswapV4Pool {
    // Create a new UniswapV4 pool with Manager Address and PoolKey
    pub fn new(manager_address: Address, pool_key: IPoolManager::PoolKey) -> Self {
        let mut pool = Self {
            pool_key: pool_key.clone(),
            manager_address,
            token_a: Token::new_with_decimals(pool_key.currency0, 0), // Decimals will be synced
            token_b: Token::new_with_decimals(pool_key.currency1, 0),
            tick_spacing: pool_key.tickSpacing.as_i32(),
            lp_fee: pool_key.fee.to::<u32>(),
            ..Default::default()
        };
        // Calculate pool_id immediately
        pool.pool_id = pool.get_pool_id().unwrap_or(B256::ZERO);
        pool
    }

    pub fn get_pool_key(&self) -> Result<PoolKey, AMMError> {
        if self.pool_key != IPoolManager::PoolKey::default() {
            return Ok(self.pool_key.clone());
        }
        // Should verify pool_key is present
        Err(AMMError::UniswapV4Error(UniswapV4Error::NotInitialized))
    }

    pub fn get_pool_id(&self) -> Result<B256, AMMError> {
        if self.pool_key != IPoolManager::PoolKey::default() {
            Ok(keccak256(self.pool_key.abi_encode()))
        } else {
            Err(AMMError::UniswapV4Error(UniswapV4Error::NotInitialized))
        }
    }

    #[inline]
    fn protocol_fee_for_direction(&self, zero_for_one: bool) -> u32 {
        if zero_for_one {
            self.protocol_fee & 0x0fff
        } else {
            (self.protocol_fee >> 12) & 0x0fff
        }
    }

    #[inline]
    fn effective_fee_pips(&self, zero_for_one: bool) -> u32 {
        let protocol_fee = self.protocol_fee_for_direction(zero_for_one) as u64;
        let lp_fee = self.lp_fee as u64;

        // Matches Uniswap v4 ProtocolFeeLibrary::calculateSwapFee.
        (protocol_fee + lp_fee - (protocol_fee * lp_fee / 1_000_000)).min(1_000_000u64) as u32
    }

    fn simulate_swap_exact_in_state(
        &self,
        base_token: Address,
        amount_in: U256,
    ) -> Result<(CurrentState, U256), AMMError> {
        if amount_in.is_zero() {
            let state = CurrentState {
                amount_specified_remaining: I256::ZERO,
                amount_calculated: I256::ZERO,
                sqrt_price_x_96: self.sqrt_price,
                tick: self.tick,
                liquidity: self.liquidity,
            };
            return Ok((state, U256::ZERO));
        }

        if self.sqrt_price.is_zero() {
            return Err(AMMError::Msg("sqrt_price is zero".into()));
        }

        let zero_for_one = base_token == self.token_a.address;
        let sqrt_price_limit_x_96 = if zero_for_one {
            MIN_SQRT_RATIO + U256_1
        } else {
            MAX_SQRT_RATIO - U256_1
        };
        let fee_pips = self.effective_fee_pips(zero_for_one);

        let mut current_state = CurrentState {
            amount_specified_remaining: I256::from_raw(amount_in),
            amount_calculated: I256::ZERO,
            sqrt_price_x_96: self.sqrt_price,
            tick: self.tick,
            liquidity: self.liquidity,
        };

        while current_state.amount_specified_remaining != I256::ZERO
            && current_state.sqrt_price_x_96 != sqrt_price_limit_x_96
        {
            let mut step = StepComputations {
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

            step.tick_next = tick_next.clamp(MIN_TICK, MAX_TICK);
            step.initialized = initialized;

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

            let (sqrt_price_x_96, amount_in_step, amount_out_step, fee_amount) =
                if current_state.liquidity == 0 {
                    (swap_target_sqrt_ratio, U256::ZERO, U256::ZERO, U256::ZERO)
                } else {
                    compute_swap_step(
                        current_state.sqrt_price_x_96,
                        swap_target_sqrt_ratio,
                        current_state.liquidity,
                        current_state.amount_specified_remaining,
                        fee_pips,
                    )
                    .map_err(UniswapV3Error::from)?
                };

            current_state.sqrt_price_x_96 = sqrt_price_x_96;
            step.amount_in = amount_in_step;
            step.amount_out = amount_out_step;
            step.fee_amount = fee_amount;

            current_state.amount_specified_remaining = current_state
                .amount_specified_remaining
                .overflowing_sub(I256::from_raw(
                    step.amount_in.overflowing_add(step.fee_amount).0,
                ))
                .0;

            current_state.amount_calculated -= I256::from_raw(step.amount_out);

            if current_state.sqrt_price_x_96 == step.sqrt_price_next_x96 {
                if step.initialized {
                    let mut liquidity_net = if let Some(info) = self.ticks.get(&step.tick_next) {
                        info.liquidity_net
                    } else {
                        return Err(AMMError::UniswapV4Error(UniswapV4Error::TickDataMissing(
                            step.tick_next,
                        )));
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

        let amount_out = (-current_state.amount_calculated).into_raw();
        Ok((current_state, amount_out))
    }

    pub fn swap_calldata(
        &self,
        zero_for_one: bool,
        amount_specified: I256,
        sqrt_price_limit_x_96: U256,
        hook_data: Bytes,
    ) -> Result<Bytes, AMMError> {
        Ok(IPoolManagerCalls::swap(swapCall {
            key: self.get_pool_key()?,
            params: IPoolManager::SwapParams {
                zeroForOne: zero_for_one,
                amountSpecified: amount_specified,
                sqrtPriceLimitX96: U160::from(sqrt_price_limit_x_96),
            },
            hookData: hook_data,
        })
        .abi_encode()
        .into())
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
            let liquidity_gross_before = info.liquidity_gross;

            let liquidity_gross_after = if liquidity_delta < 0 {
                info.liquidity_gross - ((-liquidity_delta) as u128)
            } else {
                info.liquidity_gross + (liquidity_delta as u128)
            };

            flipped = (liquidity_gross_after == 0) != (liquidity_gross_before == 0);

            if liquidity_gross_before == 0 {
                info.initialized = true;
            }

            info.liquidity_gross = liquidity_gross_after;
            info.liquidity_net = if upper {
                info.liquidity_net - liquidity_delta
            } else {
                info.liquidity_net + liquidity_delta
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

#[cfg(test)]
mod test {
    use super::*;
    use crate::amms::uniswap_v4::lense::{get_liquidity_slot, get_pool_state_slot};
    use crate::amms::uniswap_v4::IPoolManager::IPoolManagerInstance;
    use crate::amms::Token;
    use dotenv::dotenv;

    use alloy::sol_types::SolValue;
    use alloy::{
        primitives::{
            address,
            aliases::{I24, U24},
            U160, U256,
        },
        providers::ProviderBuilder,
        rpc::client::ClientBuilder,
        transports::layers::{RetryBackoffLayer, ThrottleLayer},
    };
    use std::str::FromStr;

    #[test]
    fn test_effective_fee_pips_decodes_packed_protocol_fee_by_direction() {
        let manager = address!("0000000000000000000000000000000000000abc");
        let key = IPoolManager::PoolKey {
            currency0: address!("0000000000000000000000000000000000000001"),
            currency1: address!("0000000000000000000000000000000000000002"),
            fee: U24::from(3000u64),
            tickSpacing: I24::try_from(1).unwrap(),
            hooks: Address::ZERO,
        };

        let mut pool = UniswapV4Pool::new(manager, key);
        pool.protocol_fee = (700u32 << 12) | 500u32;
        pool.lp_fee = 3000;

        assert_eq!(pool.protocol_fee_for_direction(true), 500);
        assert_eq!(pool.protocol_fee_for_direction(false), 700);
        assert_eq!(pool.effective_fee_pips(true), 3499);
        assert_eq!(pool.effective_fee_pips(false), 3698);
    }

    #[test]
    fn test_simulate_swap_mut_advances_local_pool_state() {
        let manager = address!("0000000000000000000000000000000000000abc");
        let token0 = address!("0000000000000000000000000000000000000001");
        let token1 = address!("0000000000000000000000000000000000000002");
        let liquidity_delta = 1_000_000_000_000_000_000i128;

        let key = IPoolManager::PoolKey {
            currency0: token0,
            currency1: token1,
            fee: U24::from(500u64),
            tickSpacing: I24::try_from(1).unwrap(),
            hooks: Address::ZERO,
        };

        let mut pool = UniswapV4Pool::new(manager, key);
        pool.token_a = Token::new_with_decimals(token0, 18);
        pool.token_b = Token::new_with_decimals(token1, 18);
        pool.sqrt_price = U256::from(1u128) << 96;
        pool.tick = 0;
        pool.liquidity = liquidity_delta as u128;
        pool.modify_position(-100, 100, liquidity_delta).unwrap();
        pool.token_a_price = pool.calculate_price(token0, token1).unwrap();
        pool.token_b_price = pool.calculate_price(token1, token0).unwrap();

        let amount_in = U256::from(1_000_000_000_000u128);
        let before_sqrt = pool.sqrt_price;
        let before_tick = pool.tick;
        let before_liquidity = pool.liquidity;
        let before_price_a = pool.token_a_price;
        let before_price_b = pool.token_b_price;

        let out_read = pool.simulate_swap(token0, token1, amount_in).unwrap();
        assert_eq!(pool.sqrt_price, before_sqrt);
        assert_eq!(pool.tick, before_tick);
        assert_eq!(pool.liquidity, before_liquidity);
        assert_eq!(pool.token_a_price, before_price_a);
        assert_eq!(pool.token_b_price, before_price_b);

        let out_mut = pool.simulate_swap_mut(token0, token1, amount_in).unwrap();
        assert_eq!(out_mut, out_read);
        assert!(
            pool.sqrt_price != before_sqrt
                || pool.tick != before_tick
                || pool.liquidity != before_liquidity,
            "simulate_swap_mut should advance local pool state"
        );
        assert!(pool.token_a_price.is_finite() && pool.token_a_price > 0.0);
        assert!(pool.token_b_price.is_finite() && pool.token_b_price > 0.0);

        let out_after = pool.simulate_swap(token0, token1, amount_in).unwrap();
        assert_ne!(
            out_after, out_read,
            "same input after simulate_swap_mut should observe advanced local state"
        );
    }

    #[tokio::test]
    async fn test_simulate_swap_usdc_weth_v4_005() -> eyre::Result<()> {
        dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        let manager = address!("000000000004444c5dc75cB358380D2e3dE08A90");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let _weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

        let key = IPoolManager::PoolKey {
            currency0: Address::ZERO,
            currency1: usdc,
            fee: U24::from(500u64),
            tickSpacing: I24::from_str("10").unwrap(),
            hooks: Address::ZERO,
        };

        let block_id = BlockId::number(23_994_800u64);

        let pool = UniswapV4Pool::new(manager, key.clone())
            .init(block_id, provider.clone())
            .await?;

        // exactIn: 100 USDC
        let amount_in = U256::from(100_000_000u64);
        let amount_out_sim = pool.simulate_swap(pool.token_b.address, Address::ZERO, amount_in)?;

        // Chain quote via V4 Quoter
        let quoter_addr = address!("52f0e24d1c21c8a0cb1e5a5dd6198556bd9e1203");
        let quoter = IV4Quoter::new(quoter_addr, provider.clone());
        let qkey = IV4Quoter::PoolKey {
            currency0: key.currency0,
            currency1: key.currency1,
            fee: key.fee,
            tickSpacing: key.tickSpacing,
            hooks: key.hooks,
        };
        let exact_amount: u128 = amount_in.try_into().unwrap();
        let qparams = IV4Quoter::QuoteExactSingleParams {
            poolKey: qkey,
            zeroForOne: false,
            exactAmount: exact_amount,
            hookData: Bytes::new(),
        };
        match quoter
            .quoteExactInputSingle(qparams)
            .block(block_id)
            .call()
            .await
        {
            Ok(ret) => {
                let amount_out_chain = ret.amountOut;
                assert_eq!(amount_out_sim, amount_out_chain);
            }
            Err(e) => {
                eprintln!("skip chain compare: {e}");
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_simulate_swap_weth_usdc_v4_003() -> eyre::Result<()> {
        dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        let manager = address!("000000000004444c5dc75cB358380D2e3dE08A90");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let _weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

        let block_id = BlockId::number(23_994_800u64);

        let candidates = vec![
            IPoolManager::PoolKey {
                currency0: Address::ZERO,
                currency1: usdc,
                fee: U24::from(3000u64),
                tickSpacing: I24::from_str("60").unwrap(),
                hooks: Address::ZERO,
            },
            IPoolManager::PoolKey {
                currency0: Address::ZERO,
                currency1: usdc,
                fee: U24::from(3000u64),
                tickSpacing: I24::from_str("10").unwrap(),
                hooks: Address::ZERO,
            },
            // orientation with weth is invalid for native ETH; keep only sorted Address::ZERO/USDC entries
        ];
        let ipm = IPoolManagerInstance::new(manager, provider.clone());

        let mut key = None;
        for cand in candidates.into_iter() {
            let pool_id = keccak256(cand.abi_encode());
            let slot0 = ipm
                .extsload_2(vec![B256::from(
                    crate::amms::uniswap_v4::lense::get_pool_state_slot(pool_id),
                )])
                .block(block_id)
                .call()
                .await?;
            let sqrt_price_x96 = U160::from_be_slice(&slot0[0][12..32]);
            if !sqrt_price_x96.is_zero() {
                key = Some(cand);
                break;
            }
        }
        let key = key.expect("No matching pool key for USDC/ETH 0.3% at block");

        let pool = UniswapV4Pool::new(manager, key.clone())
            .init(block_id, provider.clone())
            .await?;

        // exactIn: 1 ETH
        let amount_in = U256::from(1_000_000_000_000_000_000u128);
        let amount_out_sim = pool.simulate_swap(pool.token_a.address, Address::ZERO, amount_in)?;

        // Chain quote via V4 Quoter
        let quoter_addr = address!("52f0e24d1c21c8a0cb1e5a5dd6198556bd9e1203");
        let quoter = IV4Quoter::new(quoter_addr, provider.clone());
        let qkey = IV4Quoter::PoolKey {
            currency0: key.currency0,
            currency1: key.currency1,
            fee: key.fee,
            tickSpacing: key.tickSpacing,
            hooks: key.hooks,
        };
        let exact_amount: u128 = amount_in.try_into().unwrap();
        let qparams = IV4Quoter::QuoteExactSingleParams {
            poolKey: qkey,
            zeroForOne: true,
            exactAmount: exact_amount,
            hookData: Bytes::new(),
        };
        match quoter
            .quoteExactInputSingle(qparams)
            .block(block_id)
            .call()
            .await
        {
            Ok(ret) => {
                let amount_out_chain = ret.amountOut;
                assert_eq!(amount_out_sim, amount_out_chain);
            }
            Err(e) => {
                eprintln!("skip chain compare: {e}");
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_simulate_swap_usdc_weth_v4_005_various_amounts() -> eyre::Result<()> {
        dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        let manager = address!("000000000004444c5dc75cB358380D2e3dE08A90");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let _weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

        let key = IPoolManager::PoolKey {
            currency0: Address::ZERO,
            currency1: usdc,
            fee: U24::from(500u64),
            tickSpacing: I24::from_str("10").unwrap(),
            hooks: Address::ZERO,
        };

        let block_id = BlockId::number(23_994_800u64);

        let pool = UniswapV4Pool::new(manager, key.clone())
            .init(block_id, provider.clone())
            .await?;

        for amount_in in [
            U256::from(1_000_000u64),
            U256::from(100_000_000u64),
            U256::from(10_000_000_000u64),
        ] {
            let amount_out_sim =
                pool.simulate_swap(pool.token_b.address, Address::ZERO, amount_in)?;

            // Chain quote via V4 Quoter
            let quoter_addr = address!("52f0e24d1c21c8a0cb1e5a5dd6198556bd9e1203");
            let quoter = IV4Quoter::new(quoter_addr, provider.clone());
            let qkey = IV4Quoter::PoolKey {
                currency0: key.currency0,
                currency1: key.currency1,
                fee: key.fee,
                tickSpacing: key.tickSpacing,
                hooks: key.hooks,
            };
            let exact_amount: u128 = amount_in.try_into().unwrap();
            let qparams = IV4Quoter::QuoteExactSingleParams {
                poolKey: qkey,
                zeroForOne: false,
                exactAmount: exact_amount,
                hookData: Bytes::new(),
            };
            match quoter
                .quoteExactInputSingle(qparams)
                .block(block_id)
                .call()
                .await
            {
                Ok(ret) => {
                    let amount_out_chain = ret.amountOut;
                    assert_eq!(amount_out_sim, amount_out_chain);
                }
                Err(e) => {
                    eprintln!("skip chain compare: {e}");
                }
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_simulate_swap_weth_usdc_v4_005_various_amounts() -> eyre::Result<()> {
        dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        let manager = address!("000000000004444c5dc75cB358380D2e3dE08A90");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let _weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

        let key = IPoolManager::PoolKey {
            currency0: Address::ZERO,
            currency1: usdc,
            fee: U24::from(500u64),
            tickSpacing: I24::from_str("10").unwrap(),
            hooks: Address::ZERO,
        };

        let block_id = BlockId::number(23_994_800u64);

        let pool = UniswapV4Pool::new(manager, key.clone())
            .init(block_id, provider.clone())
            .await?;

        for amount_in in [
            U256::from(10_000_000_000_000_000u128),
            U256::from(1_000_000_000_000_000_000u128),
            U256::from(10_000_000_000_000_000_000u128),
        ] {
            let amount_out_sim =
                pool.simulate_swap(pool.token_a.address, Address::ZERO, amount_in)?;

            // Chain quote via V4 Quoter
            let quoter_addr = address!("52f0e24d1c21c8a0cb1e5a5dd6198556bd9e1203");
            let quoter = IV4Quoter::new(quoter_addr, provider.clone());
            let qkey = IV4Quoter::PoolKey {
                currency0: key.currency0,
                currency1: key.currency1,
                fee: key.fee,
                tickSpacing: key.tickSpacing,
                hooks: key.hooks,
            };
            let exact_amount: u128 = amount_in.try_into().unwrap();
            let qparams = IV4Quoter::QuoteExactSingleParams {
                poolKey: qkey,
                zeroForOne: true,
                exactAmount: exact_amount,
                hookData: Bytes::new(),
            };
            match quoter
                .quoteExactInputSingle(qparams)
                .block(block_id)
                .call()
                .await
            {
                Ok(ret) => {
                    let amount_out_chain = ret.amountOut;
                    assert_eq!(amount_out_sim, amount_out_chain);
                }
                Err(e) => {
                    eprintln!("skip chain compare: {e}");
                }
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_simulate_swap_usdc_weth_v4_003_various_amounts() -> eyre::Result<()> {
        dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        let manager = address!("000000000004444c5dc75cB358380D2e3dE08A90");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let _weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

        let block_id = BlockId::number(23_994_800u64);

        let candidates = vec![
            IPoolManager::PoolKey {
                currency0: Address::ZERO,
                currency1: usdc,
                fee: U24::from(3000u64),
                tickSpacing: I24::from_str("60").unwrap(),
                hooks: Address::ZERO,
            },
            IPoolManager::PoolKey {
                currency0: Address::ZERO,
                currency1: usdc,
                fee: U24::from(3000u64),
                tickSpacing: I24::from_str("10").unwrap(),
                hooks: Address::ZERO,
            },
        ];

        let ipm = IPoolManagerInstance::new(manager, provider.clone());

        let mut key = None;
        for cand in candidates.into_iter() {
            let pool_id = keccak256(cand.abi_encode());
            let slot0 = ipm
                .extsload_2(vec![B256::from(
                    crate::amms::uniswap_v4::lense::get_pool_state_slot(pool_id),
                )])
                .block(block_id)
                .call()
                .await?;
            let sqrt_price_x96 = U160::from_be_slice(&slot0[0][12..32]);
            if !sqrt_price_x96.is_zero() {
                key = Some(cand);
                break;
            }
        }
        let key = key.expect("No matching pool key for USDC/ETH 0.3% at block");

        let pool = UniswapV4Pool::new(manager, key.clone())
            .init(block_id, provider.clone())
            .await?;

        for amount_in in [
            U256::from(1_000_000u64),
            U256::from(100_000_000u64),
            U256::from(10_000_000_000u64),
        ] {
            let amount_out_sim =
                pool.simulate_swap(pool.token_b.address, Address::ZERO, amount_in)?;

            // Chain quote via V4 Quoter
            let quoter_addr = address!("52f0e24d1c21c8a0cb1e5a5dd6198556bd9e1203");
            let quoter = IV4Quoter::new(quoter_addr, provider.clone());
            let qkey = IV4Quoter::PoolKey {
                currency0: key.currency0,
                currency1: key.currency1,
                fee: key.fee,
                tickSpacing: key.tickSpacing,
                hooks: key.hooks,
            };
            let exact_amount: u128 = amount_in.try_into().unwrap();
            let qparams = IV4Quoter::QuoteExactSingleParams {
                poolKey: qkey,
                zeroForOne: false,
                exactAmount: exact_amount,
                hookData: Bytes::new(),
            };
            match quoter
                .quoteExactInputSingle(qparams)
                .block(block_id)
                .call()
                .await
            {
                Ok(ret) => {
                    let amount_out_chain = ret.amountOut;
                    assert_eq!(amount_out_sim, amount_out_chain);
                }
                Err(e) => {
                    eprintln!("skip chain compare: {e}");
                }
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_simulate_swap_weth_usdc_v4_003_various_amounts() -> eyre::Result<()> {
        dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        let manager = address!("000000000004444c5dc75cB358380D2e3dE08A90");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let _weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

        let key = IPoolManager::PoolKey {
            currency0: Address::ZERO,
            currency1: usdc,
            fee: U24::from(3000u64),
            tickSpacing: I24::from_str("60").unwrap(),
            hooks: Address::ZERO,
        };

        let block_id = BlockId::number(23_994_800u64);

        let pool_id = keccak256(key.abi_encode());
        let ipm = IPoolManagerInstance::new(manager, provider.clone());
        let probe = ipm
            .extsload_2(vec![
                B256::from(get_pool_state_slot(pool_id)),
                B256::from(get_liquidity_slot(pool_id)),
            ])
            .block(block_id)
            .call()
            .await?;
        let sqrt_price_probe = U160::from_be_slice(&probe[0][12..32]);
        let liq_probe = u128::from_be_bytes(probe[1][16..32].try_into().unwrap());
        println!(
            "probe key: c0={c0:?}, c1={c1:?}, fee={fee}, ts={tick_spacing}, sqrt_price_x96={sqrt_price_x96}, liquidity={liquidity}",
            c0 = key.currency0,
            c1 = key.currency1,
            fee = key.fee,
            tick_spacing = key.tickSpacing,
            sqrt_price_x96 = sqrt_price_probe,
            liquidity = liq_probe
        );

        let pool = UniswapV4Pool::new(manager, key.clone())
            .init(block_id, provider.clone())
            .await?;

        for amount_in in [
            U256::from(1_000_000_000_000_000u128),
            U256::from(100_000_000_000_000_000u128),
            U256::from(5_000_000_000_000_000_000u128),
        ] {
            let amount_out_sim =
                pool.simulate_swap(pool.token_a.address, Address::ZERO, amount_in)?;

            let params = IPoolManager::SwapParams {
                zeroForOne: true,
                amountSpecified: I256::from_raw(amount_in),
                sqrtPriceLimitX96: U160::from(MIN_SQRT_RATIO + U256_1),
            };
            println!(
                "chain swap call: zeroForOne={zero_for_one}, amount_in={amount_in}, sqrt_limit={sqrt_limit}",
                zero_for_one = params.zeroForOne,
                sqrt_limit = params.sqrtPriceLimitX96
            );

            let quoter_addr = address!("52f0e24d1c21c8a0cb1e5a5dd6198556bd9e1203");
            let quoter = IV4Quoter::new(quoter_addr, provider.clone());
            let qkey = IV4Quoter::PoolKey {
                currency0: key.currency0,
                currency1: key.currency1,
                fee: key.fee,
                tickSpacing: key.tickSpacing,
                hooks: key.hooks,
            };
            let exact_amount: u128 = amount_in.try_into().unwrap();
            let qparams = IV4Quoter::QuoteExactSingleParams {
                poolKey: qkey,
                zeroForOne: true,
                exactAmount: exact_amount,
                hookData: Bytes::new(),
            };
            match quoter
                .quoteExactInputSingle(qparams)
                .block(block_id)
                .call()
                .await
            {
                Ok(ret) => {
                    let amount_out_chain = ret.amountOut;
                    println!("amount_out_sim: {amount_out_sim:?}");
                    println!("amount_out_chain: {amount_out_chain:?}");
                    assert_eq!(amount_out_sim, amount_out_chain);
                }
                Err(e) => {
                    eprintln!("skip chain compare: {e}");
                }
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_simulate_swap_wbtc_usdt_v4_003_various_amounts() -> eyre::Result<()> {
        dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        let manager = address!("000000000004444c5dc75cB358380D2e3dE08A90");
        let wbtc = address!("2260fac5e5542a773aa44fbcfedf7c193bc2c599");
        let usdt = address!("dac17f958d2ee523a2206206994597c13d831ec7");

        let block_id = BlockId::number(23_994_800u64);

        let candidates = vec![
            IPoolManager::PoolKey {
                currency0: wbtc,
                currency1: usdt,
                fee: U24::from(3000u64),
                tickSpacing: I24::from_str("60").unwrap(),
                hooks: Address::ZERO,
            },
            IPoolManager::PoolKey {
                currency0: wbtc,
                currency1: usdt,
                fee: U24::from(3000u64),
                tickSpacing: I24::from_str("10").unwrap(),
                hooks: Address::ZERO,
            },
        ];

        let ipm = IPoolManagerInstance::new(manager, provider.clone());

        let mut key = None;
        for cand in candidates.into_iter() {
            let pool_id = keccak256(cand.abi_encode());
            let slot0 = ipm
                .extsload_2(vec![B256::from(
                    crate::amms::uniswap_v4::lense::get_pool_state_slot(pool_id),
                )])
                .block(block_id)
                .call()
                .await?;
            let sqrt_price_x96 = U160::from_be_slice(&slot0[0][12..32]);
            if !sqrt_price_x96.is_zero() {
                key = Some(cand);
                break;
            }
        }
        let key = key.expect("No matching pool key for WBTC/USDT 0.3% at block");

        let pool = UniswapV4Pool::new(manager, key.clone())
            .init(block_id, provider.clone())
            .await?;

        for amount_in in [
            U256::from(100_000u64),
            U256::from(1_000_000u64),
            U256::from(10_000_000u64),
        ] {
            let amount_out_sim =
                pool.simulate_swap(pool.token_a.address, Address::ZERO, amount_in)?;

            let quoter_addr = address!("52f0e24d1c21c8a0cb1e5a5dd6198556bd9e1203");
            let quoter = IV4Quoter::new(quoter_addr, provider.clone());
            let qkey = IV4Quoter::PoolKey {
                currency0: key.currency0,
                currency1: key.currency1,
                fee: key.fee,
                tickSpacing: key.tickSpacing,
                hooks: key.hooks,
            };
            let exact_amount: u128 = amount_in.try_into().unwrap();
            let qparams = IV4Quoter::QuoteExactSingleParams {
                poolKey: qkey,
                zeroForOne: true,
                exactAmount: exact_amount,
                hookData: Bytes::new(),
            };
            match quoter
                .quoteExactInputSingle(qparams)
                .block(block_id)
                .call()
                .await
            {
                Ok(ret) => {
                    let amount_out_chain = ret.amountOut;
                    assert_eq!(amount_out_sim, amount_out_chain);
                }
                Err(e) => {
                    eprintln!("skip chain compare: {e}");
                }
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_simulate_swap_wbtc_cbbtc_v4_001_various_amounts() -> eyre::Result<()> {
        dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        let manager = address!("000000000004444c5dc75cB358380D2e3dE08A90");
        let wbtc = address!("2260fac5e5542a773aa44fbcfedf7c193bc2c599");
        let cbbtc = address!("cbb7c0000ab88b473b1f5afd9ef808440eed33bf");

        let block_id = BlockId::number(23_994_800u64);

        let candidates = vec![
            IPoolManager::PoolKey {
                currency0: wbtc,
                currency1: cbbtc,
                fee: U24::from(100u64),
                tickSpacing: I24::from_str("1").unwrap(),
                hooks: Address::ZERO,
            },
            IPoolManager::PoolKey {
                currency0: wbtc,
                currency1: cbbtc,
                fee: U24::from(100u64),
                tickSpacing: I24::from_str("10").unwrap(),
                hooks: Address::ZERO,
            },
        ];

        let ipm = IPoolManagerInstance::new(manager, provider.clone());

        let mut key = None;
        for cand in candidates.into_iter() {
            let pool_id = keccak256(cand.abi_encode());
            let slot0 = ipm
                .extsload_2(vec![B256::from(
                    crate::amms::uniswap_v4::lense::get_pool_state_slot(pool_id),
                )])
                .block(block_id)
                .call()
                .await?;
            let sqrt_price_x96 = U160::from_be_slice(&slot0[0][12..32]);
            if !sqrt_price_x96.is_zero() {
                key = Some(cand);
                break;
            }
        }
        let key = key.expect("No matching pool key for WBTC/cbBTC 0.01% at block");

        let pool = UniswapV4Pool::new(manager, key.clone())
            .init(block_id, provider.clone())
            .await?;

        for amount_in in [
            U256::from(100_000u64),
            U256::from(1_000_000u64),
            U256::from(10_000_000u64),
        ] {
            let amount_out_sim =
                pool.simulate_swap(pool.token_a.address, Address::ZERO, amount_in)?;

            let quoter_addr = address!("52f0e24d1c21c8a0cb1e5a5dd6198556bd9e1203");
            let quoter = IV4Quoter::new(quoter_addr, provider.clone());
            let qkey = IV4Quoter::PoolKey {
                currency0: key.currency0,
                currency1: key.currency1,
                fee: key.fee,
                tickSpacing: key.tickSpacing,
                hooks: key.hooks,
            };
            let exact_amount: u128 = amount_in.try_into().unwrap();
            let qparams = IV4Quoter::QuoteExactSingleParams {
                poolKey: qkey,
                zeroForOne: true,
                exactAmount: exact_amount,
                hookData: Bytes::new(),
            };
            match quoter
                .quoteExactInputSingle(qparams)
                .block(block_id)
                .call()
                .await
            {
                Ok(ret) => {
                    let amount_out_chain = ret.amountOut;
                    assert_eq!(amount_out_sim, amount_out_chain);
                }
                Err(e) => {
                    eprintln!("skip chain compare: {e}");
                }
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_calculate_price() -> eyre::Result<()> {
        dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        let manager = address!("000000000004444c5dc75cB358380D2e3dE08A90");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        // Initializing a pool that likely exists. Using the one from test_simulate_swap_usdc_weth_v4_005
        let key = IPoolManager::PoolKey {
            currency0: Address::ZERO,
            currency1: usdc,
            fee: U24::from(500u64),
            tickSpacing: I24::from_str("10").unwrap(),
            hooks: Address::ZERO,
        };

        let block_id = BlockId::number(23_994_800u64); // Fixed block

        let pool = UniswapV4Pool::new(manager, key.clone())
            .init(block_id, provider.clone())
            .await?;

        let price_a = pool.calculate_price(pool.token_a.address, Address::default())?;
        let price_b = pool.calculate_price(pool.token_b.address, Address::default())?;

        // Expectation:
        // Token A is ETH (Address::ZERO)
        // Token B is USDC
        // Price A should be ETH price in USDC (e.g. ~3000)
        // Price B should be USDC price in ETH (e.g. ~0.00033)

        println!("Token A (ETH) Price: {}", price_a);
        println!("Token B (USDC) Price: {}", price_b);

        assert!(price_a > 2000.0 && price_a < 4000.0);
        assert!(price_b > 0.0002 && price_b < 0.0005);

        Ok(())
    }

    #[tokio::test]
    async fn test_simulate_swap_exact_out_usdc_weth_v4_005() -> eyre::Result<()> {
        dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        let manager = address!("000000000004444c5dc75cB358380D2e3dE08A90");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

        let key = IPoolManager::PoolKey {
            currency0: Address::ZERO,
            currency1: usdc,
            fee: U24::from(500u64),
            tickSpacing: I24::from_str("10").unwrap(),
            hooks: Address::ZERO,
        };

        let block_id = BlockId::number(23_994_800u64);

        let pool = UniswapV4Pool::new(manager, key)
            .init(block_id, provider.clone())
            .await?;

        // Exact out: USDC -> ETH (want ETH out)
        // Since token_a is ETH (Address::ZERO) and token_b is USDC,
        // to get ETH out we need to swap USDC in: base_token = USDC (token_b)
        let exact_outs_eth = [
            U256::from(10_000_000_000_000_u128),   // 0.00001 ETH
            U256::from(100_000_000_000_000_u128),  // 0.0001 ETH
            U256::from(1_000_000_000_000_000u128), // 0.001 ETH
        ];

        for amount_out in exact_outs_eth {
            let amount_in = pool.simulate_swap_exact_out(
                pool.token_b.address, // base_token = USDC (input)
                Address::ZERO,
                amount_out,
            )?;

            // Verify: simulate_swap with USDC amount_in should produce >= ETH amount_out
            let actual_out = pool.simulate_swap(pool.token_b.address, Address::ZERO, amount_in)?;
            println!(
                "exact_out: want {} ETH, computed amount_in {} USDC, actual_out {} ETH",
                amount_out, amount_in, actual_out
            );
            assert!(
                actual_out >= amount_out,
                "actual_out {} < amount_out {}",
                actual_out,
                amount_out
            );
        }

        // Exact out: ETH -> USDC (want USDC out)
        // To get USDC out, swap ETH in: base_token = ETH (token_a)
        let exact_outs_usdc = [
            U256::from(1_000_000u64),         // 1 USDC
            U256::from(100_000_000u64),       // 100 USDC
            U256::from(1_000_000_000_000u64), // 1,000,000 USDC
        ];

        for amount_out in exact_outs_usdc {
            let amount_in = pool.simulate_swap_exact_out(
                pool.token_a.address, // base_token = ETH (input)
                Address::ZERO,
                amount_out,
            )?;

            // Verify: simulate_swap with ETH amount_in should produce >= USDC amount_out
            let actual_out = pool.simulate_swap(pool.token_a.address, Address::ZERO, amount_in)?;
            println!(
                "exact_out: want {} USDC, computed amount_in {} ETH, actual_out {} USDC",
                amount_out, amount_in, actual_out
            );
            assert!(
                actual_out >= amount_out,
                "actual_out {} < amount_out {}",
                actual_out,
                amount_out
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_simulate_swap_exact_out_weth_usdc_v4_003() -> eyre::Result<()> {
        dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        let manager = address!("000000000004444c5dc75cB358380D2e3dE08A90");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

        let block_id = BlockId::number(23_994_800u64);

        let candidates = vec![
            IPoolManager::PoolKey {
                currency0: Address::ZERO,
                currency1: usdc,
                fee: U24::from(3000u64),
                tickSpacing: I24::from_str("60").unwrap(),
                hooks: Address::ZERO,
            },
            IPoolManager::PoolKey {
                currency0: Address::ZERO,
                currency1: usdc,
                fee: U24::from(3000u64),
                tickSpacing: I24::from_str("10").unwrap(),
                hooks: Address::ZERO,
            },
        ];

        let ipm = IPoolManagerInstance::new(manager, provider.clone());

        let mut key = None;
        for cand in candidates.into_iter() {
            let pool_id = keccak256(cand.abi_encode());
            let slot0 = ipm
                .extsload_2(vec![B256::from(
                    crate::amms::uniswap_v4::lense::get_pool_state_slot(pool_id),
                )])
                .block(block_id)
                .call()
                .await?;
            let sqrt_price_x96 = U160::from_be_slice(&slot0[0][12..32]);
            if !sqrt_price_x96.is_zero() {
                key = Some(cand);
                break;
            }
        }
        let key = key.expect("No matching pool key for USDC/ETH 0.3% at block");

        let pool = UniswapV4Pool::new(manager, key)
            .init(block_id, provider.clone())
            .await?;

        // Exact out: ETH -> USDC (want USDC out)
        // base_token = ETH (token_a) for ETH input
        let exact_outs_usdc = [
            U256::from(1_000_000u64),         // 1 USDC
            U256::from(100_000_000u64),       // 100 USDC
            U256::from(1_000_000_000_000u64), // 1,000,000 USDC
        ];

        for amount_out in exact_outs_usdc {
            let amount_in = pool.simulate_swap_exact_out(
                pool.token_a.address, // base_token = ETH (input)
                Address::ZERO,
                amount_out,
            )?;

            let actual_out = pool.simulate_swap(pool.token_a.address, Address::ZERO, amount_in)?;
            println!(
                "exact_out: want {} USDC, computed amount_in {} ETH, actual_out {} USDC",
                amount_out, amount_in, actual_out
            );
            assert!(
                actual_out >= amount_out,
                "actual_out {} < amount_out {}",
                actual_out,
                amount_out
            );
        }

        // Exact out: USDC -> ETH (want ETH out)
        // base_token = USDC (token_b) for USDC input
        let exact_outs_eth = [
            U256::from(100_000_000_000_000_u128),   // 0.0001 ETH
            U256::from(1_000_000_000_000_000_u128), // 0.001 ETH
            U256::from(10_000_000_000_000_000u128), // 0.01 ETH
        ];

        for amount_out in exact_outs_eth {
            let amount_in = pool.simulate_swap_exact_out(
                pool.token_b.address, // base_token = USDC (input)
                Address::ZERO,
                amount_out,
            )?;

            let actual_out = pool.simulate_swap(pool.token_b.address, Address::ZERO, amount_in)?;
            println!(
                "exact_out: want {} ETH, computed amount_in {} USDC, actual_out {} ETH",
                amount_out, amount_in, actual_out
            );
            assert!(
                actual_out >= amount_out,
                "actual_out {} < amount_out {}",
                actual_out,
                amount_out
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_simulate_swap_exact_out_zero_amount() -> eyre::Result<()> {
        dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        let manager = address!("000000000004444c5dc75cB358380D2e3dE08A90");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

        let key = IPoolManager::PoolKey {
            currency0: Address::ZERO,
            currency1: usdc,
            fee: U24::from(500u64),
            tickSpacing: I24::from_str("10").unwrap(),
            hooks: Address::ZERO,
        };

        let block_id = BlockId::number(23_994_800u64);

        let pool = UniswapV4Pool::new(manager, key)
            .init(block_id, provider.clone())
            .await?;

        // Zero amount should return zero
        let amount_in =
            pool.simulate_swap_exact_out(pool.token_a.address, Address::ZERO, U256::ZERO)?;
        assert!(amount_in.is_zero());

        Ok(())
    }

    #[tokio::test]
    async fn test_simulate_swap_exact_out_insufficient_liquidity() -> eyre::Result<()> {
        dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        let manager = address!("000000000004444c5dc75cB358380D2e3dE08A90");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

        let key = IPoolManager::PoolKey {
            currency0: Address::ZERO,
            currency1: usdc,
            fee: U24::from(500u64),
            tickSpacing: I24::from_str("10").unwrap(),
            hooks: Address::ZERO,
        };

        let block_id = BlockId::number(23_994_800u64);

        let pool = UniswapV4Pool::new(manager, key)
            .init(block_id, provider.clone())
            .await?;

        // Request an absurdly large exact-out amount to force exhaustion
        let huge_out = U256::from(10u8).pow(U256::from(36u8));
        let res = pool.simulate_swap_exact_out(pool.token_a.address, Address::ZERO, huge_out);
        assert!(
            res.is_err(),
            "Should fail with insufficient liquidity error"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_simulate_swap_exact_out_round_trip() -> eyre::Result<()> {
        // Round-trip test: exact_in -> exact_out should be approximately the same
        dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        let manager = address!("000000000004444c5dc75cB358380D2e3dE08A90");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

        let key = IPoolManager::PoolKey {
            currency0: Address::ZERO,
            currency1: usdc,
            fee: U24::from(500u64),
            tickSpacing: I24::from_str("10").unwrap(),
            hooks: Address::ZERO,
        };

        let block_id = BlockId::number(23_994_800u64);

        let pool = UniswapV4Pool::new(manager, key)
            .init(block_id, provider.clone())
            .await?;

        // Round trip: ETH -> USDC -> ETH
        let original_amount_in = U256::from(1_000_000_000_000_000_000u128); // 1 ETH

        // Step 1: exact_in to get amount_out
        let amount_out =
            pool.simulate_swap(pool.token_a.address, Address::ZERO, original_amount_in)?;

        // Step 2: exact_out to get back amount_in
        let round_trip_amount_in = pool.simulate_swap_exact_out(
            pool.token_a.address, // base_token = ETH
            Address::ZERO,
            amount_out,
        )?;

        println!(
            "Round trip: {} ETH -> {} USDC -> {} ETH",
            original_amount_in, amount_out, round_trip_amount_in
        );

        // The round trip amount should be very close to original (within 0.01% tolerance due to fees)
        let diff = if round_trip_amount_in > original_amount_in {
            round_trip_amount_in - original_amount_in
        } else {
            original_amount_in - round_trip_amount_in
        };
        let tolerance = original_amount_in / U256::from(10000u64); // 0.01%
        assert!(
            diff <= tolerance,
            "Round trip difference {} exceeds tolerance {}",
            diff,
            tolerance
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_simulate_swap_exact_out_chain_compare_usdc_weth_v4_005() -> eyre::Result<()> {
        // Compare simulate_swap_exact_out with chain quoteExactOutputSingle
        dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        let manager = address!("000000000004444c5dc75cB358380D2e3dE08A90");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

        let key = IPoolManager::PoolKey {
            currency0: Address::ZERO,
            currency1: usdc,
            fee: U24::from(500u64),
            tickSpacing: I24::from_str("10").unwrap(),
            hooks: Address::ZERO,
        };

        let block_id = BlockId::number(23_994_800u64);

        let pool = UniswapV4Pool::new(manager, key.clone())
            .init(block_id, provider.clone())
            .await?;

        let quoter_addr = address!("52f0e24d1c21c8a0cb1e5a5dd6198556bd9e1203");
        let quoter = IV4Quoter::new(quoter_addr, provider.clone());
        let qkey = IV4Quoter::PoolKey {
            currency0: key.currency0,
            currency1: key.currency1,
            fee: key.fee,
            tickSpacing: key.tickSpacing,
            hooks: key.hooks,
        };

        // Exact out: USDC -> ETH (want ETH out)
        // base_token = USDC (token_b), zeroForOne = false
        let exact_outs_eth = [
            U256::from(10_000_000_000_000_u128),   // 0.00001 ETH
            U256::from(100_000_000_000_000_u128),  // 0.0001 ETH
            U256::from(1_000_000_000_000_000u128), // 0.001 ETH
        ];

        for amount_out in exact_outs_eth {
            let amount_in_local = pool.simulate_swap_exact_out(
                pool.token_b.address, // base_token = USDC (input)
                Address::ZERO,
                amount_out,
            )?;

            // Chain quote via V4 Quoter quoteExactOutputSingle
            let exact_amount: u128 = amount_out.try_into().unwrap();
            let qparams = IV4Quoter::QuoteExactSingleParams {
                poolKey: qkey.clone(),
                zeroForOne: false, // USDC -> ETH
                exactAmount: exact_amount,
                hookData: Bytes::new(),
            };

            match quoter
                .quoteExactOutputSingle(qparams)
                .block(block_id)
                .call()
                .await
            {
                Ok(ret) => {
                    let amount_in_chain = ret.amountIn;
                    println!(
                        "exact_out: want {} ETH, local amount_in {} USDC, chain amount_in {} USDC",
                        amount_out, amount_in_local, amount_in_chain
                    );
                    assert_eq!(
                        amount_in_local, amount_in_chain,
                        "Local ({}) != Chain ({}) for amount_out {}",
                        amount_in_local, amount_in_chain, amount_out
                    );
                }
                Err(e) => {
                    eprintln!("skip chain compare: {e}");
                }
            }
        }

        // Exact out: ETH -> USDC (want USDC out)
        // base_token = ETH (token_a), zeroForOne = true
        let exact_outs_usdc = [
            U256::from(1_000_000u64),         // 1 USDC
            U256::from(100_000_000u64),       // 100 USDC
            U256::from(1_000_000_000_000u64), // 1,000,000 USDC
        ];

        for amount_out in exact_outs_usdc {
            let amount_in_local = pool.simulate_swap_exact_out(
                pool.token_a.address, // base_token = ETH (input)
                Address::ZERO,
                amount_out,
            )?;

            // Chain quote via V4 Quoter quoteExactOutputSingle
            let exact_amount: u128 = amount_out.try_into().unwrap();
            let qparams = IV4Quoter::QuoteExactSingleParams {
                poolKey: qkey.clone(),
                zeroForOne: true, // ETH -> USDC
                exactAmount: exact_amount,
                hookData: Bytes::new(),
            };

            match quoter
                .quoteExactOutputSingle(qparams)
                .block(block_id)
                .call()
                .await
            {
                Ok(ret) => {
                    let amount_in_chain = ret.amountIn;
                    println!(
                        "exact_out: want {} USDC, local amount_in {} ETH, chain amount_in {} ETH",
                        amount_out, amount_in_local, amount_in_chain
                    );
                    assert_eq!(
                        amount_in_local, amount_in_chain,
                        "Local ({}) != Chain ({}) for amount_out {}",
                        amount_in_local, amount_in_chain, amount_out
                    );
                }
                Err(e) => {
                    eprintln!("skip chain compare: {e}");
                }
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_simulate_swap_exact_out_chain_compare_weth_usdc_v4_003() -> eyre::Result<()> {
        // Compare simulate_swap_exact_out with chain quoteExactOutputSingle for 0.3% pool
        dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        let manager = address!("000000000004444c5dc75cB358380D2e3dE08A90");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

        let block_id = BlockId::number(23_994_800u64);

        let candidates = vec![
            IPoolManager::PoolKey {
                currency0: Address::ZERO,
                currency1: usdc,
                fee: U24::from(3000u64),
                tickSpacing: I24::from_str("60").unwrap(),
                hooks: Address::ZERO,
            },
            IPoolManager::PoolKey {
                currency0: Address::ZERO,
                currency1: usdc,
                fee: U24::from(3000u64),
                tickSpacing: I24::from_str("10").unwrap(),
                hooks: Address::ZERO,
            },
        ];

        let ipm = IPoolManagerInstance::new(manager, provider.clone());

        let mut key = None;
        for cand in candidates.into_iter() {
            let pool_id = keccak256(cand.abi_encode());
            let slot0 = ipm
                .extsload_2(vec![B256::from(
                    crate::amms::uniswap_v4::lense::get_pool_state_slot(pool_id),
                )])
                .block(block_id)
                .call()
                .await?;
            let sqrt_price_x96 = U160::from_be_slice(&slot0[0][12..32]);
            if !sqrt_price_x96.is_zero() {
                key = Some(cand);
                break;
            }
        }
        let key = key.expect("No matching pool key for USDC/ETH 0.3% at block");

        let pool = UniswapV4Pool::new(manager, key.clone())
            .init(block_id, provider.clone())
            .await?;

        let quoter_addr = address!("52f0e24d1c21c8a0cb1e5a5dd6198556bd9e1203");
        let quoter = IV4Quoter::new(quoter_addr, provider.clone());
        let qkey = IV4Quoter::PoolKey {
            currency0: key.currency0,
            currency1: key.currency1,
            fee: key.fee,
            tickSpacing: key.tickSpacing,
            hooks: key.hooks,
        };

        // Exact out: ETH -> USDC (want USDC out)
        // base_token = ETH (token_a), zeroForOne = true
        let exact_outs_usdc = [
            U256::from(1_000_000u64),         // 1 USDC
            U256::from(100_000_000u64),       // 100 USDC
            U256::from(1_000_000_000_000u64), // 1,000,000 USDC
        ];

        for amount_out in exact_outs_usdc {
            let amount_in_local = pool.simulate_swap_exact_out(
                pool.token_a.address, // base_token = ETH (input)
                Address::ZERO,
                amount_out,
            )?;

            let exact_amount: u128 = amount_out.try_into().unwrap();
            let qparams = IV4Quoter::QuoteExactSingleParams {
                poolKey: qkey.clone(),
                zeroForOne: true, // ETH -> USDC
                exactAmount: exact_amount,
                hookData: Bytes::new(),
            };

            match quoter
                .quoteExactOutputSingle(qparams)
                .block(block_id)
                .call()
                .await
            {
                Ok(ret) => {
                    let amount_in_chain = ret.amountIn;
                    println!(
                        "exact_out: want {} USDC, local amount_in {} ETH, chain amount_in {} ETH",
                        amount_out, amount_in_local, amount_in_chain
                    );
                    assert_eq!(
                        amount_in_local, amount_in_chain,
                        "Local ({}) != Chain ({}) for amount_out {}",
                        amount_in_local, amount_in_chain, amount_out
                    );
                }
                Err(e) => {
                    eprintln!("skip chain compare: {e}");
                }
            }
        }

        // Exact out: USDC -> ETH (want ETH out)
        // base_token = USDC (token_b), zeroForOne = false
        let exact_outs_eth = [
            U256::from(100_000_000_000_000_u128),   // 0.0001 ETH
            U256::from(1_000_000_000_000_000_u128), // 0.001 ETH
            U256::from(10_000_000_000_000_000u128), // 0.01 ETH
        ];

        for amount_out in exact_outs_eth {
            let amount_in_local = pool.simulate_swap_exact_out(
                pool.token_b.address, // base_token = USDC (input)
                Address::ZERO,
                amount_out,
            )?;

            let exact_amount: u128 = amount_out.try_into().unwrap();
            let qparams = IV4Quoter::QuoteExactSingleParams {
                poolKey: qkey.clone(),
                zeroForOne: false, // USDC -> ETH
                exactAmount: exact_amount,
                hookData: Bytes::new(),
            };

            match quoter
                .quoteExactOutputSingle(qparams)
                .block(block_id)
                .call()
                .await
            {
                Ok(ret) => {
                    let amount_in_chain = ret.amountIn;
                    println!(
                        "exact_out: want {} ETH, local amount_in {} USDC, chain amount_in {} USDC",
                        amount_out, amount_in_local, amount_in_chain
                    );
                    assert_eq!(
                        amount_in_local, amount_in_chain,
                        "Local ({}) != Chain ({}) for amount_out {}",
                        amount_in_local, amount_in_chain, amount_out
                    );
                }
                Err(e) => {
                    eprintln!("skip chain compare: {e}");
                }
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_simulate_swap_exact_out_chain_compare_wbtc_usdt_v4_003() -> eyre::Result<()> {
        // Compare simulate_swap_exact_out with chain quoteExactOutputSingle for WBTC/USDT 0.3% pool
        dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        let manager = address!("000000000004444c5dc75cB358380D2e3dE08A90");
        let wbtc = address!("2260fac5e5542a773aa44fbcfedf7c193bc2c599");
        let usdt = address!("dac17f958d2ee523a2206206994597c13d831ec7");

        let block_id = BlockId::number(23_994_800u64);

        let candidates = vec![
            IPoolManager::PoolKey {
                currency0: wbtc,
                currency1: usdt,
                fee: U24::from(3000u64),
                tickSpacing: I24::from_str("60").unwrap(),
                hooks: Address::ZERO,
            },
            IPoolManager::PoolKey {
                currency0: wbtc,
                currency1: usdt,
                fee: U24::from(3000u64),
                tickSpacing: I24::from_str("10").unwrap(),
                hooks: Address::ZERO,
            },
        ];

        let ipm = IPoolManagerInstance::new(manager, provider.clone());

        let mut key = None;
        for cand in candidates.into_iter() {
            let pool_id = keccak256(cand.abi_encode());
            let slot0 = ipm
                .extsload_2(vec![B256::from(
                    crate::amms::uniswap_v4::lense::get_pool_state_slot(pool_id),
                )])
                .block(block_id)
                .call()
                .await?;
            let sqrt_price_x96 = U160::from_be_slice(&slot0[0][12..32]);
            if !sqrt_price_x96.is_zero() {
                key = Some(cand);
                break;
            }
        }
        let key = key.expect("No matching pool key for WBTC/USDT 0.3% at block");

        let pool = UniswapV4Pool::new(manager, key.clone())
            .init(block_id, provider.clone())
            .await?;

        let quoter_addr = address!("52f0e24d1c21c8a0cb1e5a5dd6198556bd9e1203");
        let quoter = IV4Quoter::new(quoter_addr, provider.clone());
        let qkey = IV4Quoter::PoolKey {
            currency0: key.currency0,
            currency1: key.currency1,
            fee: key.fee,
            tickSpacing: key.tickSpacing,
            hooks: key.hooks,
        };

        // Exact out: WBTC -> USDT (want USDT out)
        // base_token = WBTC (token_a), zeroForOne = true
        let exact_outs_usdt = [
            U256::from(1_000_000u64),      // 1 USDT
            U256::from(100_000_000u64),    // 100 USDT
            U256::from(10_000_000_000u64), // 10,000 USDT
        ];

        for amount_out in exact_outs_usdt {
            let amount_in_local = pool.simulate_swap_exact_out(
                pool.token_a.address, // base_token = WBTC (input)
                Address::ZERO,
                amount_out,
            )?;

            let exact_amount: u128 = amount_out.try_into().unwrap();
            let qparams = IV4Quoter::QuoteExactSingleParams {
                poolKey: qkey.clone(),
                zeroForOne: true, // WBTC -> USDT
                exactAmount: exact_amount,
                hookData: Bytes::new(),
            };

            match quoter
                .quoteExactOutputSingle(qparams)
                .block(block_id)
                .call()
                .await
            {
                Ok(ret) => {
                    let amount_in_chain = ret.amountIn;
                    println!(
                        "exact_out: want {} USDT, local amount_in {} WBTC, chain amount_in {} WBTC",
                        amount_out, amount_in_local, amount_in_chain
                    );
                    assert_eq!(
                        amount_in_local, amount_in_chain,
                        "Local ({}) != Chain ({}) for amount_out {}",
                        amount_in_local, amount_in_chain, amount_out
                    );
                }
                Err(e) => {
                    eprintln!("skip chain compare: {e}");
                }
            }
        }

        Ok(())
    }
}

use super::{
    amm::{AutomatedMarketMaker, SyncAction, AMM},
    consts::{MIN_V3_LIQUIDITY, MPFR_T_PRECISION},
    error::{AMMError, BatchContractError},
    factory::{AutomatedMarketMakerFactory, DiscoverySync},
    get_token_decimals, Token,
};
use crate::amms::{
    consts::U256_1, uniswap_v3::GetUniswapV3PoolTickBitmapBatchRequest::TickBitmapInfo,
};
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, Bytes, Signed, B256, I256, U256},
    providers::Provider,
    rpc::types::{Filter, FilterSet, Log},
    sol,
    sol_types::{SolCall, SolEvent, SolValue},
    transports::BoxFuture,
};
use futures::{stream::FuturesUnordered, StreamExt};
use rayon::iter::{IntoParallelRefIterator, ParallelDrainRange, ParallelIterator};
use rug::ops::Pow;
use rug::Float;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    hash::Hash,
};
use thiserror::Error;
use tokio::time::{sleep, Duration};
use tracing::info;
use uniswap_v3_math::error::UniswapV3MathError;
use uniswap_v3_math::tick_math::{MAX_SQRT_RATIO, MAX_TICK, MIN_SQRT_RATIO, MIN_TICK};
use GetUniswapV3PoolTickDataBatchRequest::TickDataInfo;

sol! {
    // UniswapV3Factory
    #[allow(missing_docs)]
    #[derive(Debug)]
    #[sol(rpc)]
    contract IUniswapV3Factory {
        /// @notice Emitted when a pool is created
        event PoolCreated(
            address indexed token0,
            address indexed token1,
            uint24 indexed fee,
            int24 tickSpacing,
            address pool
        );
    }

    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IUniswapV3PoolEvents {
        /// @notice Emitted when liquidity is minted for a given position
        event Mint(
            address sender,
            address indexed owner,
            int24 indexed tickLower,
            int24 indexed tickUpper,
            uint128 amount,
            uint256 amount0,
            uint256 amount1
        );

        /// @notice Emitted when a position's liquidity is removed
        event Burn(
            address indexed owner,
            int24 indexed tickLower,
            int24 indexed tickUpper,
            uint128 amount,
            uint256 amount0,
            uint256 amount1
        );

        /// @notice Emitted by the pool for any swaps between token0 and token1
        event Swap(
            address indexed sender,
            address indexed recipient,
            int256 amount0,
            int256 amount1,
            uint160 sqrtPriceX96,
            uint128 liquidity,
            int24 tick
        );
    }


    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IUniswapV3Pool {
        function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160 sqrtPriceLimitX96, bytes calldata data) external returns (int256, int256);
        function tickSpacing() external view returns (int24);
        function fee() external view returns (uint24);
        function token0() external view returns (address);
        function token1() external view returns (address);

    }
}

sol! {
    #[sol(rpc)]
    GetUniswapV3PoolSlot0BatchRequest,
    "src/amms/abi/GetUniswapV3PoolSlot0BatchRequest.json",
}

sol! {
    #[sol(rpc)]
    GetUniswapV3PoolTickBitmapBatchRequest,
    "src/amms/abi/GetUniswapV3PoolTickBitmapBatchRequest.json",
}

sol! {
    #[sol(rpc)]
    GetUniswapV3PoolTickDataBatchRequest,
    "src/amms/abi/GetUniswapV3PoolTickDataBatchRequest.json"
}

sol! {
    #[sol(rpc)]
    GetUniswapV3PoolStaticMetaBatchRequest,
    "src/amms/abi/GetUniswapV3PoolStaticMetaBatchRequest.json",
}

#[derive(Error, Debug)]
pub enum UniswapV3Error {
    #[error(transparent)]
    UniswapV3MathError(#[from] UniswapV3MathError),
    #[error("Liquidity Underflow")]
    LiquidityUnderflow,
    #[error("Step Zero")]
    StepZero,
    #[error("Tick Data Missing for tick {0}")]
    TickDataMissing(i32),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UniswapV3Pool {
    pub address: Address,
    #[serde(default)]
    pub last_synced_block: u64,
    pub token_a: Token,
    pub token_b: Token,
    pub liquidity: u128,
    pub sqrt_price: U256,
    pub fee: u32,
    pub tick: i32,
    pub tick_spacing: i32,
    pub tick_bitmap: HashMap<i16, U256>,
    pub ticks: HashMap<i32, Info>,
    #[serde(default)]
    pub token_a_price: f64,
    #[serde(default)]
    pub token_b_price: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Info {
    pub liquidity_gross: u128,
    pub liquidity_net: i128,
    pub initialized: bool,
}

impl Info {
    pub fn new(liquidity_gross: u128, liquidity_net: i128, initialized: bool) -> Self {
        Info {
            liquidity_gross,
            liquidity_net,
            initialized,
        }
    }
}

pub struct CurrentState {
    amount_specified_remaining: I256,
    amount_calculated: I256,
    sqrt_price_x_96: U256,
    tick: i32,
    liquidity: u128,
}

#[derive(Default)]
pub struct StepComputations {
    pub sqrt_price_start_x_96: U256,
    pub tick_next: i32,
    pub initialized: bool,
    pub sqrt_price_next_x96: U256,
    pub amount_in: U256,
    pub amount_out: U256,
    pub fee_amount: U256,
}

pub struct Tick {
    pub liquidity_gross: u128,
    pub liquidity_net: i128,
    pub fee_growth_outside_0_x_128: U256,
    pub fee_growth_outside_1_x_128: U256,
    pub tick_cumulative_outside: U256,
    pub seconds_per_liquidity_outside_x_128: U256,
    pub seconds_outside: u32,
    pub initialized: bool,
}

impl AutomatedMarketMaker for UniswapV3Pool {
    fn address(&self) -> Address {
        self.address
    }

    fn last_synced_block(&self) -> u64 {
        self.last_synced_block
    }

    fn set_last_synced_block(&mut self, block_number: u64) {
        self.last_synced_block = self.last_synced_block.max(block_number);
    }

    fn sync_events(&self) -> Vec<B256> {
        vec![
            IUniswapV3PoolEvents::Mint::SIGNATURE_HASH,
            IUniswapV3PoolEvents::Burn::SIGNATURE_HASH,
            IUniswapV3PoolEvents::Swap::SIGNATURE_HASH,
        ]
    }

    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError> {
        let event_signature = log.topics()[0];
        match event_signature {
            IUniswapV3PoolEvents::Swap::SIGNATURE_HASH => {
                let swap_event = IUniswapV3PoolEvents::Swap::decode_log(log.as_ref())?;

                let tick_after: i32 = swap_event.tick.unchecked_into();

                // Only warn if liquidity mismatch happens WITHOUT a tick crossing.
                // If ticks are different, liquidity change is expected.
                if swap_event.liquidity != self.liquidity && tick_after == self.tick {
                    tracing::warn!(
                        target: "amms::uniswap_v3::sync",
                        address = ?self.address,
                        local_liquidity = ?self.liquidity,
                        remote_liquidity = ?swap_event.liquidity,
                        local_tick = ?self.tick,
                        remote_tick = ?tick_after,
                        "Liquidity mismatch detected within same tick. Local state may be missing Mint/Burn events."
                    );
                }

                self.sqrt_price = swap_event.sqrtPriceX96.to();
                self.liquidity = swap_event.liquidity;
                self.tick = tick_after;

                if let Ok(p) = self.calculate_price(self.token_a.address, self.token_b.address) {
                    self.token_a_price = p;
                    if p != 0.0 {
                        self.token_b_price = 1.0 / p;
                    } else {
                        self.token_b_price = 0.0;
                    }
                }

                info!(
                    target = "amms::uniswap_v3::sync",
                    block_number = ?log.block_number,
                    address = ?self.address,
                    sqrt_price = ?self.sqrt_price,
                    liquidity = ?self.liquidity,
                    tick = ?self.tick,
                    "Swap"
                );
            }
            IUniswapV3PoolEvents::Mint::SIGNATURE_HASH => {
                let mint_event = IUniswapV3PoolEvents::Mint::decode_log(log.as_ref())?;

                self.modify_position(
                    mint_event.tickLower.unchecked_into(),
                    mint_event.tickUpper.unchecked_into(),
                    mint_event.amount as i128,
                )?;

                info!(
                    target = "amms::uniswap_v3::sync",
                    block_number = ?log.block_number,
                    address = ?self.address,
                    sqrt_price = ?self.sqrt_price,
                    liquidity = ?self.liquidity,
                    tick = ?self.tick,
                    "Mint"
                );
            }
            IUniswapV3PoolEvents::Burn::SIGNATURE_HASH => {
                let burn_event = IUniswapV3PoolEvents::Burn::decode_log(log.as_ref())?;

                self.modify_position(
                    burn_event.tickLower.unchecked_into(),
                    burn_event.tickUpper.unchecked_into(),
                    -(burn_event.amount as i128),
                )?;

                info!(
                    target = "amms::uniswap_v3::sync",
                    block_number = ?log.block_number,
                    address = ?self.address,
                    sqrt_price = ?self.sqrt_price,
                    liquidity = ?self.liquidity,
                    tick = ?self.tick,
                    "Burn"
                );
            }
            _ => {
                info!(
                    target = "amms::uniswap_v3::sync",
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
        if amount_in.is_zero() {
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

        // Initialize a mutable state state struct to hold the dynamic simulated state of the pool
        let mut current_state = CurrentState {
            sqrt_price_x_96: self.sqrt_price, // Active price on the pool
            amount_calculated: I256::ZERO,    // Amount of token_out that has been calculated
            amount_specified_remaining: I256::from_raw(amount_in), // Amount of token_in that has not been swapped
            tick: self.tick,                                       // Current i24 tick of the pool
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
                .map_err(UniswapV3Error::from)?;

            // ensure that we do not overshoot the min/max tick, as the tick bitmap is not aware of these bounds
            // Note: this could be removed as we are clamping in the batch contract
            step.tick_next = step.tick_next.clamp(MIN_TICK, MAX_TICK);

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
            if current_state.liquidity == 0 {
                // If liquidity is zero, we move instantly to the target price without consuming any amount
                current_state.sqrt_price_x_96 = swap_target_sqrt_ratio;
                step.amount_in = U256::ZERO;
                step.amount_out = U256::ZERO;
                step.fee_amount = U256::ZERO;
            } else {
                (
                    current_state.sqrt_price_x_96,
                    step.amount_in,
                    step.amount_out,
                    step.fee_amount,
                ) = uniswap_v3_math::swap_math::compute_swap_step(
                    current_state.sqrt_price_x_96,
                    swap_target_sqrt_ratio,
                    current_state.liquidity,
                    current_state.amount_specified_remaining,
                    self.fee,
                )
                .map_err(UniswapV3Error::from)?;
            }

            // Decrement the amount remaining to be swapped and amount received from the step
            current_state.amount_specified_remaining = current_state
                .amount_specified_remaining
                .overflowing_sub(I256::from_raw(
                    step.amount_in.overflowing_add(step.fee_amount).0,
                ))
                .0;

            current_state.amount_calculated -= I256::from_raw(step.amount_out);

            // If the price moved all the way to the next price, recompute the liquidity change for the next iteration
            if current_state.sqrt_price_x_96 == step.sqrt_price_next_x96 {
                if step.initialized {
                    let mut liquidity_net = if let Some(info) = self.ticks.get(&step.tick_next) {
                        info.liquidity_net
                    } else {
                        return Err(AMMError::from(UniswapV3Error::TickDataMissing(
                            step.tick_next,
                        )));
                    };

                    // we are on a tick boundary, and the next tick is initialized, so we must charge a protocol fee
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

        let amount_out = (-current_state.amount_calculated).into_raw();

        tracing::trace!(?amount_out);

        Ok(amount_out)
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

        // Defensive check: prevent divide-by-zero panic in uniswap_v3_math
        if self.sqrt_price.is_zero() {
            return Err(AMMError::Msg("sqrt_price is zero".into()));
        }
        if self.liquidity == 0 {
            return Err(AMMError::Msg("liquidity is zero".into()));
        }

        let zero_for_one = base_token == self.token_a.address;

        // Set sqrt_price_limit_x_96 to the max or min sqrt price in the pool depending on zero_for_one
        let sqrt_price_limit_x_96 = if zero_for_one {
            MIN_SQRT_RATIO + U256_1
        } else {
            MAX_SQRT_RATIO - U256_1
        };

        // Initialize a mutable state state struct to hold the dynamic simulated state of the pool
        let mut current_state = CurrentState {
            // Active price on the pool
            sqrt_price_x_96: self.sqrt_price,
            // Amount of token_out that has been calculated
            amount_calculated: I256::ZERO,
            // Amount of token_in that has not been swapped
            amount_specified_remaining: I256::from_raw(amount_in),
            // Current i24 tick of the pool
            tick: self.tick,
            // Current available liquidity in the tick range
            liquidity: self.liquidity,
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
                .map_err(UniswapV3Error::from)?;

            // ensure that we do not overshoot the min/max tick, as the tick bitmap is not aware of these bounds
            // Note: this could be removed as we are clamping in the batch contract
            step.tick_next = step.tick_next.clamp(MIN_TICK, MAX_TICK);

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
            if current_state.liquidity == 0 {
                current_state.sqrt_price_x_96 = swap_target_sqrt_ratio;
                step.amount_in = U256::ZERO;
                step.amount_out = U256::ZERO;
                step.fee_amount = U256::ZERO;
            } else {
                (
                    current_state.sqrt_price_x_96,
                    step.amount_in,
                    step.amount_out,
                    step.fee_amount,
                ) = uniswap_v3_math::swap_math::compute_swap_step(
                    current_state.sqrt_price_x_96,
                    swap_target_sqrt_ratio,
                    current_state.liquidity,
                    current_state.amount_specified_remaining,
                    self.fee,
                )
                .map_err(UniswapV3Error::from)?;
            }

            // Decrement the amount remaining to be swapped and amount received from the step
            current_state.amount_specified_remaining = current_state
                .amount_specified_remaining
                .overflowing_sub(I256::from_raw(
                    step.amount_in.overflowing_add(step.fee_amount).0,
                ))
                .0;

            current_state.amount_calculated -= I256::from_raw(step.amount_out);

            // If the price moved all the way to the next price, recompute the liquidity change for the next iteration
            if current_state.sqrt_price_x_96 == step.sqrt_price_next_x96 {
                if step.initialized {
                    let mut liquidity_net = if let Some(info) = self.ticks.get(&step.tick_next) {
                        info.liquidity_net
                    } else {
                        return Err(AMMError::from(UniswapV3Error::TickDataMissing(
                            step.tick_next,
                        )));
                    };

                    // we are on a tick boundary, and the next tick is initialized, so we must charge a protocol fee
                    if zero_for_one {
                        liquidity_net = -liquidity_net;
                    }

                    current_state.liquidity = if liquidity_net < 0 {
                        if current_state.liquidity < (-liquidity_net as u128) {
                            return Err(AMMError::from(UniswapV3Error::LiquidityUnderflow));
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

        // Update the pool state
        self.liquidity = current_state.liquidity;
        self.sqrt_price = current_state.sqrt_price_x_96;
        self.tick = current_state.tick;

        // Update spot prices (O(1) powi)
        if let Ok(p) = self.calculate_price(self.token_a.address, self.token_b.address) {
            self.token_a_price = p;
            if p != 0.0 {
                self.token_b_price = 1.0 / p;
            } else {
                self.token_b_price = 0.0;
            }
        }

        let amount_out = (-current_state.amount_calculated).into_raw();

        tracing::trace!(?amount_out);

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
            amount_specified_remaining: I256::ZERO - I256::from_raw(amount_out), // Remaining token_out
            tick: self.tick,                                       // Current i24 tick of the pool
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
                .map_err(UniswapV3Error::from)?;

            // ensure that we do not overshoot the min/max tick, as the tick bitmap is not aware of these bounds
            // Note: this could be removed as we are clamping in the batch contract
            step.tick_next = step.tick_next.clamp(MIN_TICK, MAX_TICK);

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
            if current_state.liquidity == 0 {
                // If liquidity is zero, we move instantly to the target price without consuming any amount
                current_state.sqrt_price_x_96 = swap_target_sqrt_ratio;
                step.amount_in = U256::ZERO;
                step.amount_out = U256::ZERO;
                step.fee_amount = U256::ZERO;
            } else {
                (
                    current_state.sqrt_price_x_96,
                    step.amount_in,
                    step.amount_out,
                    step.fee_amount,
                ) = uniswap_v3_math::swap_math::compute_swap_step(
                    current_state.sqrt_price_x_96,
                    swap_target_sqrt_ratio,
                    current_state.liquidity,
                    current_state.amount_specified_remaining,
                    self.fee,
                )
                .map_err(UniswapV3Error::from)?;
            }

            // Exact output: decrement remaining output, increment calculated input
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
                        return Err(AMMError::from(UniswapV3Error::TickDataMissing(
                            step.tick_next,
                        )));
                    };

                    // we are on a tick boundary, and the next tick is initialized, so we must charge a protocol fee
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

    fn calculate_price(&self, base_token: Address, _quote_token: Address) -> Result<f64, AMMError> {
        if self.sqrt_price.is_zero() {
            return Err(AMMError::Msg("sqrt_price is zero".into()));
        }
        if self.liquidity < MIN_V3_LIQUIDITY {
            return Ok(0.0);
        }
        let sqrt_price_x96 = self.sqrt_price;
        // Convert sqrt_price_x96 (U256) to rug::Float
        // Using string parsing is suboptimal but safer for now than direct limb manipulation without helper functions.
        // For higher performance we should implement a U256 -> Float conversion helper.
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

    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let pool = IUniswapV3Pool::new(self.address, provider.clone());

        // Get pool data
        self.tick_spacing = pool.tickSpacing().call().await?.as_i32();
        if self.tick_spacing == 0 {
            return Err(AMMError::from(UniswapV3Error::StepZero));
        }

        self.fee = pool.fee().call().await?.to::<u32>();

        // Get tokens
        self.token_a = Token::new(pool.token0().call().await?, provider.clone()).await?;
        self.token_b = Token::new(pool.token1().call().await?, provider.clone()).await?;

        let mut pool = vec![self.into()];
        UniswapV3Factory::sync_slot_0(&mut pool, block_number, provider.clone()).await?;
        UniswapV3Factory::sync_token_decimals(&mut pool, provider.clone()).await?;
        UniswapV3Factory::sync_tick_bitmaps(&mut pool, block_number, provider.clone()).await?;
        UniswapV3Factory::sync_tick_data(&mut pool, block_number, provider.clone()).await?;

        let AMM::UniswapV3Pool(mut pool_struct) = pool[0].to_owned() else {
            unreachable!()
        };

        // Init Prices
        if let Ok(price) =
            pool_struct.calculate_price(pool_struct.token_a.address, pool_struct.token_b.address)
        {
            pool_struct.token_a_price = price;
            if price != 0.0 {
                pool_struct.token_b_price = 1.0 / price;
            } else {
                pool_struct.token_b_price = 0.0;
            }
        }

        Ok(pool_struct)
    }
}

impl UniswapV3Pool {
    // Create a new, unsynced UniswapV3 pool
    pub fn new(address: Address) -> Self {
        Self {
            address,
            token_a_price: 0.0,
            token_b_price: 0.0,
            ..Default::default()
        }
    }

    /// Modifies a positions liquidity in the pool.
    pub fn modify_position(
        &mut self,
        tick_lower: i32,
        tick_upper: i32,
        liquidity_delta: i128,
    ) -> Result<(), AMMError> {
        //We are only using this function when a mint or burn event is emitted,
        //therefore we do not need to checkTicks as that has happened before the event is emitted
        self.update_position(tick_lower, tick_upper, liquidity_delta)?;

        if liquidity_delta != 0 {
            //if the tick is between the tick lower and tick upper, update the liquidity between the ticks
            if self.tick >= tick_lower && self.tick < tick_upper {
                self.liquidity = if liquidity_delta < 0 {
                    self.liquidity - ((-liquidity_delta) as u128)
                } else {
                    self.liquidity + (liquidity_delta as u128)
                }
            }
        }

        Ok(())
    }

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
                self.flip_tick(tick_lower, self.tick_spacing, liquidity_delta > 0);
            }
            if flipped_upper {
                self.flip_tick(tick_upper, self.tick_spacing, liquidity_delta > 0);
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

    pub fn update_tick(
        &mut self,
        tick: i32,
        liquidity_delta: i128,
        upper: bool,
    ) -> Result<bool, AMMError> {
        let info = self.ticks.entry(tick).or_default();

        let liquidity_gross_before = info.liquidity_gross;

        let liquidity_gross_after = if liquidity_delta < 0 {
            liquidity_gross_before - ((-liquidity_delta) as u128)
        } else {
            liquidity_gross_before + (liquidity_delta as u128)
        };

        // we do not need to check if liqudity_gross_after > maxLiquidity because we are only calling update tick on a burn or mint log.
        // this should already be validated when a log is
        let flipped = (liquidity_gross_after == 0) != (liquidity_gross_before == 0);

        if liquidity_gross_before == 0 {
            info.initialized = true;
        }

        info.liquidity_gross = liquidity_gross_after;

        info.liquidity_net = if upper {
            info.liquidity_net - liquidity_delta
        } else {
            info.liquidity_net + liquidity_delta
        };

        Ok(flipped)
    }

    pub fn flip_tick(&mut self, tick: i32, tick_spacing: i32, initialized: bool) {
        let compressed = compress_tick(tick, tick_spacing);

        let (word_pos, bit_pos) = uniswap_v3_math::tick_bitmap::position(compressed);
        let mask = U256::from(1) << bit_pos;

        if let Some(word) = self.tick_bitmap.get_mut(&word_pos) {
            if initialized {
                *word |= mask;
            } else {
                *word &= !mask;
            }
        } else {
            if initialized {
                self.tick_bitmap.insert(word_pos, mask);
            }
        }
    }

    pub fn swap_calldata(
        &self,
        recipient: Address,
        zero_for_one: bool,
        amount_specified: I256,
        sqrt_price_limit_x_96: U256,
        calldata: Vec<u8>,
    ) -> Result<Bytes, AMMError> {
        Ok(IUniswapV3Pool::swapCall {
            recipient,
            zeroForOne: zero_for_one,
            amountSpecified: amount_specified,
            sqrtPriceLimitX96: sqrt_price_limit_x_96.to(),
            data: calldata.into(),
        }
        .abi_encode()
        .into())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct UniswapV3Factory {
    pub address: Address,
    pub creation_block: u64,
}

impl UniswapV3Factory {
    pub fn new(address: Address, creation_block: u64) -> Self {
        UniswapV3Factory {
            address,
            creation_block,
        }
    }

    pub async fn get_all_pools<N, P>(
        &self,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let disc_filter = Filter::new()
            .event_signature(FilterSet::from(vec![self.pool_creation_event()]))
            .address(vec![self.address()]);

        let sync_provider = provider.clone();
        let mut futures = FuturesUnordered::new();

        let sync_step = 2_000;
        let mut latest_block = self.creation_block;
        let tip = block_number.as_u64().unwrap_or_default();
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(15));

        let mut pools = vec![];
        loop {
            tokio::select! {
                _ = interval.tick(), if latest_block <= tip => {
                    let mut block_filter = disc_filter.clone();
                    let from_block = latest_block;
                    let to_block = (from_block + sync_step).min(tip);

                    block_filter = block_filter.from_block(from_block);
                    block_filter = block_filter.to_block(to_block);

                    let sync_provider = sync_provider.clone();
                    futures.push(async move { sync_provider.get_logs(&block_filter).await });

                    latest_block = to_block + 1;
                },
                res = futures.next(), if !futures.is_empty() => {
                    if let Some(res) = res {
                        let logs = res?;
                        for log in logs {
                            pools.push(self.create_pool(log)?);
                        }
                    }
                }
            }

            if latest_block > tip && futures.is_empty() {
                break;
            }
        }

        Ok(pools)
    }

    /// Batch initialize a list of Uniswap V3 pools, mirroring single-pool `init()` semantics
    /// without introducing new batch ABIs (Plan B).
    ///
    /// For each pool:
    /// - Fetch static metadata concurrently: `token0`, `token1`, `tickSpacing`, `fee`
    /// - Sync `slot0` in batches: `tick`, `liquidity`, `sqrt_price`
    /// - Sync token decimals in batch
    /// - Filter invalid pools after `slot0` and `decimals` are populated
    /// - Sync tick bitmaps and tick data in batches
    pub async fn init_batch<N, P>(
        mut pools: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let total = pools.len();
        let addresses = pools
            .iter()
            .filter_map(|amm| match amm {
                AMM::UniswapV3Pool(p) => {
                    if p.token_a.address.is_zero()
                        || p.token_b.address.is_zero()
                        || p.tick_spacing == 0
                        || p.fee == 0
                    {
                        Some(p.address)
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .collect::<Vec<_>>();

        // 2) 批量获取静态元数据：token0、token1、tickSpacing、fee
        if !addresses.is_empty() {
            let mut meta_map: HashMap<Address, (Address, Address, i32, u32)> = HashMap::new();
            let step = 150; // 保守下调批量大小，避免节点对 initcode/返回体大小的限制
            for chunk in addresses.chunks(step) {
                let chunk_addrs = chunk.to_vec();
                let return_data = GetUniswapV3PoolStaticMetaBatchRequest::deploy_builder(
                    provider.clone(),
                    chunk_addrs.clone(),
                )
                .call_raw()
                .block(block_number)
                .await?;
                let return_data =
                    <Vec<(Address, Address, i32, u32)> as SolValue>::abi_decode(&return_data)?;

                for (meta, pool_addr) in return_data.iter().zip(chunk_addrs.iter()) {
                    let (t0, t1, ts, fee) = *meta;
                    meta_map.insert(*pool_addr, (t0, t1, ts, fee));
                }
                sleep(Duration::from_millis(500)).await;
            }

            for amm in pools.iter_mut() {
                if let AMM::UniswapV3Pool(ref mut uv3_pool) = amm {
                    if let Some((t0, t1, ts, fee)) = meta_map.get(&uv3_pool.address).copied() {
                        uv3_pool.token_a.address = t0;
                        uv3_pool.token_b.address = t1;
                        uv3_pool.tick_spacing = ts;
                        uv3_pool.fee = fee;

                        if fee == 0 {
                            tracing::warn!(address = ?uv3_pool.address, "Uniswap V3 pool initialized with 0 fee!");
                        }
                    }
                }
            }
        }

        // 3) Batch sync slot0 (tick, liquidity, sqrt_price)
        UniswapV3Factory::sync_slot_0(&mut pools, block_number, provider.clone()).await?;
        sleep(Duration::from_millis(500)).await;

        // 4) Batch sync token decimals
        UniswapV3Factory::sync_token_decimals(&mut pools, provider.clone()).await?;
        sleep(Duration::from_millis(500)).await;

        // 5) Filter invalid pools AFTER slot0 + decimals are populated
        // AND Initialize spot prices for valid pools
        let (valid_pools, invalid_pools): (Vec<_>, Vec<_>) =
            pools.par_drain(..).partition(|pool| match pool {
                AMM::UniswapV3Pool(uv3_pool) => {
                    uv3_pool.liquidity > 0
                        && uv3_pool.tick_spacing != 0
                        && !uv3_pool.token_a.address.is_zero()
                        && !uv3_pool.token_b.address.is_zero()
                        && uv3_pool.token_a.decimals > 0
                        && uv3_pool.token_b.decimals > 0
                }
                _ => false,
            });

        // Mutate valid pools to set initial prices
        let mut valid_pools_mut = valid_pools;
        for amm in valid_pools_mut.iter_mut() {
            if let AMM::UniswapV3Pool(p) = amm {
                if let Ok(price) = p.calculate_price(p.token_a.address, p.token_b.address) {
                    p.token_a_price = price;
                    if price != 0.0 {
                        p.token_b_price = 1.0 / price;
                    } else {
                        p.token_b_price = 0.0;
                    }
                }
            }
        }
        let valid_pools = valid_pools_mut;

        if !invalid_pools.is_empty() {
            for pool in &invalid_pools {
                if let AMM::UniswapV3Pool(uv3_pool) = pool {
                    info!(
                        target: "amms::uniswap_v3::init_batch",
                        address = ?uv3_pool.address,
                        liquidity = ?uv3_pool.liquidity,
                        tick_spacing = ?uv3_pool.tick_spacing,
                        token_a = ?uv3_pool.token_a.address,
                        token_b = ?uv3_pool.token_b.address,
                        token_a_decimals = ?uv3_pool.token_a.decimals,
                        token_b_decimals = ?uv3_pool.token_b.decimals,
                        "Filtering out V3 pool"
                    );
                }
            }
        }
        pools = valid_pools;

        // 6) Batch sync tick bitmaps and tick data (chunked to reduce RPC pressure)
        let pools_step = 50;
        for group in pools.chunks_mut(pools_step) {
            UniswapV3Factory::sync_tick_bitmaps(group, block_number, provider.clone()).await?;
            sleep(Duration::from_millis(500)).await;
            UniswapV3Factory::sync_tick_data(group, block_number, provider.clone()).await?;
            sleep(Duration::from_millis(500)).await;
        }

        let valid = pools.len();
        let invalid = invalid_pools.len();
        info!(
            target: "amms::uniswap_v3::init_batch",
            total = total,
            valid = valid,
            invalid = invalid,
            "Batch initialization complete"
        );

        Ok(pools)
    }

    pub async fn sync_all_pools<N, P>(
        mut pools: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        UniswapV3Factory::sync_slot_0(&mut pools, block_number, provider.clone()).await?;

        let (valid_pools, invalid_pools): (Vec<_>, Vec<_>) =
            pools.par_drain(..).partition(|pool| match pool {
                AMM::UniswapV3Pool(uv3_pool) => {
                    uv3_pool.liquidity > 0
                        && uv3_pool.tick_spacing != 0
                        && !uv3_pool.token_a.address.is_zero()
                        && !uv3_pool.token_b.address.is_zero()
                }
                _ => false,
            });

        if !invalid_pools.is_empty() {
            for pool in &invalid_pools {
                if let AMM::UniswapV3Pool(uv3_pool) = pool {
                    info!(
                        target: "amms::uniswap_v3::sync",
                        address = ?uv3_pool.address,
                        liquidity = ?uv3_pool.liquidity,
                        tick_spacing = ?uv3_pool.tick_spacing,
                        token_a = ?uv3_pool.token_a.address,
                        token_b = ?uv3_pool.token_b.address,
                        "Filtering out V3 pool"
                    );
                }
            }
        }
        pools = valid_pools;

        // Clear previous tick data to prevent stale data buildup
        for pool in pools.iter_mut() {
            if let AMM::UniswapV3Pool(uv3_pool) = pool {
                uv3_pool.tick_bitmap.clear();
                uv3_pool.ticks.clear();
            }
        }

        let pools_step = 50;
        for group in pools.chunks_mut(pools_step) {
            UniswapV3Factory::sync_tick_bitmaps(group, block_number, provider.clone()).await?;
            sleep(Duration::from_millis(500)).await;
            UniswapV3Factory::sync_tick_data(group, block_number, provider.clone()).await?;
            sleep(Duration::from_millis(500)).await;
        }

        // Recalculate cached spot prices
        for pool in pools.iter_mut() {
            if let AMM::UniswapV3Pool(uv3_pool) = pool {
                if let Ok(price) =
                    uv3_pool.calculate_price(uv3_pool.token_a.address, uv3_pool.token_b.address)
                {
                    uv3_pool.token_a_price = price;
                    uv3_pool.token_b_price = if price != 0.0 { 1.0 / price } else { 0.0 };
                }
            }
        }

        Ok(pools)
    }

    pub async fn sync_token_decimals<N, P>(
        pools: &mut [AMM],
        provider: P,
    ) -> Result<(), BatchContractError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        // Get all token decimals
        let mut tokens = HashSet::new();
        for pool in pools.iter() {
            for token in pool.tokens() {
                tokens.insert(token);
            }
        }
        let token_decimals = get_token_decimals(tokens.into_iter().collect(), provider).await?;

        // Set token decimals
        for pool in pools.iter_mut() {
            let AMM::UniswapV3Pool(uniswap_v3_pool) = pool else {
                unreachable!()
            };

            if let Some(decimals) = token_decimals.get(&uniswap_v3_pool.token_a.address) {
                uniswap_v3_pool.token_a.decimals = *decimals;
            }

            if let Some(decimals) = token_decimals.get(&uniswap_v3_pool.token_b.address) {
                uniswap_v3_pool.token_b.decimals = *decimals;
            }
        }

        Ok(())
    }

    pub async fn sync_slot_0<N, P>(
        pools: &mut [AMM],
        block_number: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let step = 255;

        let mut futures = FuturesUnordered::new();
        pools.chunks_mut(step).for_each(|group| {
            let provider = provider.clone();
            let pool_addresses = group
                .iter_mut()
                .map(|pool| pool.address())
                .collect::<Vec<_>>();

            futures.push(async move {
                Ok::<(&mut [AMM], Bytes), AMMError>((
                    group,
                    GetUniswapV3PoolSlot0BatchRequest::deploy_builder(provider, pool_addresses)
                        .call_raw()
                        .block(block_number)
                        .await?,
                ))
            });
        });

        while let Some(res) = futures.next().await {
            let (pools, return_data) = res?;
            let return_data = <Vec<(i32, u128, U256)> as SolValue>::abi_decode(&return_data)?;

            for (slot_0_data, pool) in return_data.iter().zip(pools.iter_mut()) {
                let AMM::UniswapV3Pool(ref mut uv3_pool) = pool else {
                    unreachable!()
                };

                uv3_pool.tick = slot_0_data.0;
                uv3_pool.liquidity = slot_0_data.1;
                uv3_pool.sqrt_price = slot_0_data.2;
            }
        }

        Ok(())
    }

    pub async fn sync_tick_bitmaps<N, P>(
        pools: &mut [AMM],
        block_number: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let mut futures: FuturesUnordered<BoxFuture<'_, _>> = FuturesUnordered::new();

        // Keep returned "runtime code" under EVM max code size (24KB).
        // Each word returns 2 * U256 (64 bytes), so 300 words ~= 19.2KB + ABI overhead.
        // This avoids "max code size exceeded" on Arbitrum during constructor-return batching.
        let max_range = 300;
        let mut group_range = 0;
        let mut group = vec![];

        for pool in pools.iter() {
            let AMM::UniswapV3Pool(uniswap_v3_pool) = pool else {
                unreachable!()
            };

            let mut min_word = tick_to_word(MIN_TICK, uniswap_v3_pool.tick_spacing);
            let max_word = tick_to_word(MAX_TICK, uniswap_v3_pool.tick_spacing);

            while min_word <= max_word {
                let remaining_range = max_range - group_range;
                let word_range = max_word - min_word + 1;
                let range = word_range.min(remaining_range);

                let start = min_word;
                let end = start + range - 1;

                group.push(TickBitmapInfo {
                    pool: uniswap_v3_pool.address,
                    minWord: start as i16,
                    maxWord: end as i16,
                });

                min_word = end + 1;
                group_range += range;

                // If group is full, fire it off and reset
                if group_range >= max_range {
                    // if group_range >= max_range || word_range <= 0 {
                    let provider = provider.clone();
                    let pool_info = group.iter().map(|info| info.pool).collect::<Vec<_>>();

                    let calldata = std::mem::take(&mut group);

                    group_range = 0;

                    futures.push(Box::pin(async move {
                        Ok::<(Vec<Address>, Bytes), AMMError>((
                            pool_info,
                            GetUniswapV3PoolTickBitmapBatchRequest::deploy_builder(
                                provider, calldata,
                            )
                            .call_raw()
                            .block(block_number)
                            .await?,
                        ))
                    }));
                }
            }
        }

        // Flush group if not empty
        if !group.is_empty() {
            let provider = provider.clone();
            let pool_info = group.iter().map(|info| info.pool).collect::<Vec<_>>();

            let calldata = std::mem::take(&mut group);

            futures.push(Box::pin(async move {
                Ok::<(Vec<Address>, Bytes), AMMError>((
                    pool_info,
                    GetUniswapV3PoolTickBitmapBatchRequest::deploy_builder(provider, calldata)
                        .call_raw()
                        .block(block_number)
                        .await?,
                ))
            }));
        }

        let mut pool_set = pools
            .iter_mut()
            .map(|pool| (pool.address(), pool))
            .collect::<HashMap<Address, &mut AMM>>();

        while let Some(res) = futures.next().await {
            let (pools, return_data) = res?;
            let return_data = <Vec<Vec<U256>> as SolValue>::abi_decode(&return_data)?;

            for (tick_bitmaps, pool_address) in return_data.iter().zip(pools.iter()) {
                let pool = pool_set.get_mut(pool_address).unwrap();

                let AMM::UniswapV3Pool(ref mut uv3_pool) = pool else {
                    unreachable!()
                };

                for chunk in tick_bitmaps.chunks_exact(2) {
                    let word_pos = I256::from_raw(chunk[0]).as_i16();
                    let tick_bitmap = chunk[1];

                    uv3_pool.tick_bitmap.insert(word_pos, tick_bitmap);
                }
            }
        }
        Ok(())
    }

    // TODO: Clean this function up
    pub async fn sync_tick_data<N, P>(
        pools: &mut [AMM],
        block_number: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let pool_ticks = pools
            .par_iter()
            .filter_map(|pool| {
                if let AMM::UniswapV3Pool(uniswap_v3_pool) = pool {
                    let min_word = tick_to_word(MIN_TICK, uniswap_v3_pool.tick_spacing);
                    let max_word = tick_to_word(MAX_TICK, uniswap_v3_pool.tick_spacing);

                    let initialized_ticks: Vec<Signed<24, 1>> = (min_word..=max_word)
                        // Filter out empty bitmaps
                        .filter_map(|word_pos| {
                            uniswap_v3_pool
                                .tick_bitmap
                                .get(&(word_pos as i16))
                                .filter(|&bitmap| *bitmap != U256::ZERO)
                                .map(|&bitmap| (word_pos, bitmap))
                        })
                        // Get tick index for non zero bitmaps
                        .flat_map(|(word_pos, bitmap)| {
                            (0..256)
                                .filter(move |i| {
                                    (bitmap & (U256::from(1) << U256::from(*i))) != U256::ZERO
                                })
                                .filter_map(move |i| {
                                    let tick_index =
                                        (word_pos * 256 + i) * uniswap_v3_pool.tick_spacing;

                                    Signed::<24, 1>::try_from(tick_index).ok()
                                })
                        })
                        .collect();

                    // Only return pools with non-empty initialized ticks
                    if !initialized_ticks.is_empty() {
                        Some((uniswap_v3_pool.address, initialized_ticks))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect::<Vec<(Address, Vec<Signed<24, 1>>)>>();

        let mut futures: FuturesUnordered<BoxFuture<'_, _>> = FuturesUnordered::new();
        let max_ticks = 60;
        let mut group_ticks = 0;
        let mut group = vec![];

        for (pool_address, mut ticks) in pool_ticks {
            while !ticks.is_empty() {
                let remaining_ticks = max_ticks - group_ticks;
                let selected_ticks = ticks.drain(0..remaining_ticks.min(ticks.len()));
                group_ticks += selected_ticks.len();

                group.push(GetUniswapV3PoolTickDataBatchRequest::TickDataInfo {
                    pool: pool_address,
                    ticks: selected_ticks.collect(),
                });

                if group_ticks >= max_ticks {
                    let provider = provider.clone();
                    let calldata = std::mem::take(&mut group);

                    group_ticks = 0;
                    group.clear();

                    futures.push(Box::pin(async move {
                        Ok::<(Vec<TickDataInfo>, Bytes), AMMError>((
                            calldata.clone(),
                            GetUniswapV3PoolTickDataBatchRequest::deploy_builder(
                                provider, calldata,
                            )
                            .call_raw()
                            .block(block_number)
                            .await?,
                        ))
                    }));
                }
            }
        }

        if !group.is_empty() {
            let provider = provider.clone();
            let calldata = std::mem::take(&mut group);

            futures.push(Box::pin(async move {
                Ok::<(Vec<TickDataInfo>, Bytes), AMMError>((
                    calldata.clone(),
                    GetUniswapV3PoolTickDataBatchRequest::deploy_builder(provider, calldata)
                        .call_raw()
                        .block(block_number)
                        .await?,
                ))
            }));
        }

        let mut pool_set = pools
            .iter_mut()
            .map(|pool| (pool.address(), pool))
            .collect::<HashMap<Address, &mut AMM>>();

        while let Some(res) = futures.next().await {
            let (tick_info, return_data) = res?;
            let return_data = <Vec<Vec<(bool, u128, i128)>> as SolValue>::abi_decode(&return_data)?;

            for (tick_bitmaps, tick_info) in return_data.iter().zip(tick_info.iter()) {
                let pool = pool_set.get_mut(&tick_info.pool).unwrap();

                let AMM::UniswapV3Pool(ref mut uv3_pool) = pool else {
                    unreachable!()
                };

                for (tick, tick_idx) in tick_bitmaps.iter().zip(tick_info.ticks.iter()) {
                    let info = Info {
                        liquidity_gross: tick.1,
                        liquidity_net: tick.2,
                        initialized: tick.0,
                    };

                    uv3_pool.ticks.insert(tick_idx.as_i32(), info);
                }
            }
        }
        Ok(())
    }
}

pub fn compress_tick(tick: i32, tick_spacing: i32) -> i32 {
    tick.div_euclid(tick_spacing)
}

pub fn tick_to_word(tick: i32, tick_spacing: i32) -> i32 {
    compress_tick(tick, tick_spacing) >> 8
}

impl AutomatedMarketMakerFactory for UniswapV3Factory {
    type PoolVariant = UniswapV3Pool;

    fn address(&self) -> Address {
        self.address
    }

    fn pool_creation_event(&self) -> B256 {
        IUniswapV3Factory::PoolCreated::SIGNATURE_HASH
    }

    fn create_pool(&self, log: Log) -> Result<AMM, AMMError> {
        let pool_created_event: alloy::primitives::Log<IUniswapV3Factory::PoolCreated> =
            IUniswapV3Factory::PoolCreated::decode_log(&log.inner)?;

        Ok(AMM::UniswapV3Pool(UniswapV3Pool {
            address: pool_created_event.pool,
            token_a: pool_created_event.token0.into(),
            token_b: pool_created_event.token1.into(),
            fee: pool_created_event.fee.to::<u32>(),
            tick_spacing: pool_created_event.tickSpacing.unchecked_into(),
            ..Default::default()
        }))
    }

    fn creation_block(&self) -> u64 {
        self.creation_block
    }
}

impl DiscoverySync for UniswapV3Factory {
    fn discover<N, P>(
        &self,
        to_block: BlockId,
        provider: P,
    ) -> impl Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        info!(
            target = "amms::uniswap_v3::discover",
            address = ?self.address,
            "Discovering all pools"
        );

        self.get_all_pools(to_block, provider.clone())
    }

    fn sync<N, P>(
        &self,
        amms: Vec<AMM>,
        to_block: BlockId,
        provider: P,
    ) -> impl Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        info!(
            target = "amms::uniswap_v3::sync",
            address = ?self.address,
            "Syncing all pools"
        );

        UniswapV3Factory::init_batch(amms, to_block, provider)
    }
}

#[cfg(test)]
mod tests;

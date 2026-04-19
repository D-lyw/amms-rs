//! Aerodrome Slipstream Pool Implementation
//!
//! This module implements Aerodrome's concentrated liquidity pools,
//! which are based on Uniswap V3 with some optimizations.
//!
//! # Key Differences from Uniswap V3
//!
//! - Uses tickSpacing instead of fee for pool identification
//! - Pools created via EIP-1167 deterministic clones
//! - Dynamic fee via swap fee module (fetches fee from factory)
//! - Different tick spacing values: 1, 50, 100, 200, 2000
//! - Compatible gauge system for ve(3,3) tokenomics
//!
//! # Code Reuse
//!
//! The swap calculation logic is 100% compatible with Uniswap V3.
//! Only the pool creation and fee fetching differ.

use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{address, Address, Bytes, Signed, I256, U256},
    providers::Provider,
    rpc::types::Log,
    sol,
    sol_types::{SolCall, SolEvent, SolValue},
    transports::BoxFuture,
};
use futures::{stream::FuturesUnordered, StreamExt};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use std::collections::HashMap;
use thiserror::Error;
use tokio::time::{sleep, Duration};

use crate::amms::{
    amm::{AutomatedMarketMaker, SyncAction, AMM},
    consts::{MIN_V3_LIQUIDITY, MPFR_T_PRECISION, U256_1},
    error::AMMError,
    factory::{AutomatedMarketMakerFactory, DiscoverySync},
    uniswap_v3::{tick_to_word, GetUniswapV3PoolTickBitmapBatchRequest, UniswapV3Factory},
    Token,
};
use rug::ops::Pow;
use rug::Float;
use uniswap_v3_math::tick_math::{MAX_SQRT_RATIO, MAX_TICK, MIN_SQRT_RATIO, MIN_TICK};

pub const BASE_SLIPSTREAM_FACTORY: Address = address!("5e7BB104d84c7CB9B682AaC2F3d509f5F406809A");
const SLIPSTREAM_BATCH_RETRY_ATTEMPTS: u8 = 3;
const SLIPSTREAM_BATCH_RETRY_BASE_DELAY_MS: u64 = 200;

fn slipstream_retry_delay(attempt: u8) -> Duration {
    let exp = attempt.saturating_sub(1).min(5);
    Duration::from_millis(SLIPSTREAM_BATCH_RETRY_BASE_DELAY_MS.saturating_mul(1u64 << exp))
}

sol! {
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract ICLPool {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            bool unlocked
        );
        function liquidity() external view returns (uint128);
        function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160 sqrtPriceLimitX96, bytes calldata data) external returns (int256, int256);
        function tickSpacing() external view returns (int24);
        function fee() external view returns (uint24);
        function token0() external view returns (address);
        function token1() external view returns (address);
    }

    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract ICLPoolEvents {
        event Mint(
            address sender,
            address indexed owner,
            int24 indexed tickLower,
            int24 indexed tickUpper,
            uint128 amount,
            uint256 amount0,
            uint256 amount1
        );

        event Burn(
            address indexed owner,
            int24 indexed tickLower,
            int24 indexed tickUpper,
            uint128 amount,
            uint256 amount0,
            uint256 amount1
        );

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

    #[derive(Debug)]
    #[sol(rpc)]
    contract ICLPoolFactory {
        function swapFeeModule() external view returns (address);

        event PoolCreated(
            address indexed token0,
            address indexed token1,
            int24 indexed tickSpacing,
            address pool
        );
    }

    /// Fee change event emitted by the FeeModule contract
    /// Note: This event is emitted from the FeeModule contract, not the pool contract
    #[derive(Debug, PartialEq, Eq)]
    contract ICustomFeeModule {
        event CustomFeeSet(address indexed pool, uint24 indexed fee);
    }
}

// Aerodrome Slipstream specific batch request contracts
// Slipstream slot0 returns 6 values (no feeProtocol) vs UniswapV3's 7
// Slipstream ticks() returns only (liquidityGross, liquidityNet) vs UniswapV3's 8 fields
sol! {
    #[sol(rpc)]
    GetAerodromeSlipstreamSlot0BatchRequest,
    "src/amms/abi/GetAerodromeSlipstreamSlot0BatchRequest.json",
}

sol! {
    #[sol(rpc)]
    GetAerodromeSlipstreamPoolTickDataBatchRequest,
    "src/amms/abi/GetAerodromeSlipstreamPoolTickDataBatchRequest.json",
}

/// Aerodrome Slipstream specific errors
#[derive(Error, Debug)]
pub enum AerodromeSlipstreamError {
    #[error(transparent)]
    UniswapV3MathError(#[from] uniswap_v3_math::error::UniswapV3MathError),
    #[error("Liquidity Underflow")]
    LiquidityUnderflow,
    #[error("Step Zero")]
    StepZero,
    #[error("Tick Data Missing for tick {0}")]
    TickDataMissing(i32),
}

/// Aerodrome Slipstream Pool
///
/// This pool uses concentrated liquidity with custom tick spacing
/// (typically 2x larger than Uniswap V3).
///
/// # Key Features
///
/// - Tick-based pricing: `price = 1.0001^tick`
/// - sqrtPriceX96: `sqrt(price) * 2^96`
/// - Concentrated liquidity within custom price ranges
/// - Dynamic fee configuration
///
/// # Example
///
/// ```rust,no_run
/// use amms::aerodrome_slipstream::AerodromeSlipstreamPool;
///
/// let pool = AerodromeSlipstreamPool::new(address!("0x..."));
/// ```
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AerodromeSlipstreamPool {
    /// Pool address
    pub address: Address,
    /// Last synced block number
    #[serde(default)]
    pub last_synced_block: u64,
    /// Token A (token0)
    pub token_a: crate::amms::Token,
    /// Token B (token1)
    pub token_b: crate::amms::Token,
    /// Current liquidity
    pub liquidity: u128,
    /// Current square root price (sqrt(price) * 2^96)
    pub sqrt_price: U256,
    /// Pool fee (in hundredths of a bip, e.g., 3000 = 0.3%)
    pub fee: u32,
    /// Current tick
    pub tick: i32,
    /// Tick spacing (typically 2x Uniswap V3)
    pub tick_spacing: i32,
    /// Tick bitmap for efficient initialized tick lookup
    pub tick_bitmap: std::collections::HashMap<i16, U256>,
    /// Tick data (liquidity_net, liquidity_gross, fee_growth, etc.)
    pub ticks: std::collections::HashMap<i32, TickInfo>,
    /// Cached price of token A in terms of token B
    #[serde(default)]
    pub token_a_price: f64,
    /// Cached price of token B in terms of token A
    #[serde(default)]
    pub token_b_price: f64,
}

/// Tick information for concentrated liquidity
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TickInfo {
    /// Total liquidity at this tick
    pub liquidity_gross: u128,
    /// Net liquidity change when tick is crossed
    pub liquidity_net: i128,
    /// Whether this tick has been initialized
    pub initialized: bool,
}

impl TickInfo {
    /// Create a new tick info
    pub fn new(liquidity_gross: u128, liquidity_net: i128, initialized: bool) -> Self {
        TickInfo {
            liquidity_gross,
            liquidity_net,
            initialized,
        }
    }
}

/// Current state during swap simulation
pub struct CurrentState {
    pub amount_specified_remaining: I256,
    pub amount_calculated: I256,
    pub sqrt_price_x_96: U256,
    pub tick: i32,
    pub liquidity: u128,
}

/// Step computations during swap simulation
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

impl AerodromeSlipstreamPool {
    /// Create a new Aerodrome Slipstream pool
    pub fn new(address: Address) -> Self {
        Self {
            address,
            ..Default::default()
        }
    }
}

/// Aerodrome Slipstream Factory
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Hash, PartialEq, Eq)]
pub struct AerodromeSlipstreamFactory {
    pub address: Address,
    pub creation_block: u64,
}

impl AerodromeSlipstreamFactory {
    /// Create a new factory
    pub fn new(address: Address, creation_block: u64) -> Self {
        Self {
            address,
            creation_block,
        }
    }
}

// ============================================================================
// AutomatedMarketMaker Trait Implementation
// ============================================================================

impl AutomatedMarketMaker for AerodromeSlipstreamPool {
    fn address(&self) -> Address {
        self.address
    }

    fn last_synced_block(&self) -> u64 {
        self.last_synced_block
    }

    fn set_last_synced_block(&mut self, block_number: u64) {
        self.last_synced_block = self.last_synced_block.max(block_number);
    }

    fn sync_events(&self) -> Vec<alloy::primitives::FixedBytes<32>> {
        vec![
            ICLPoolEvents::Mint::SIGNATURE_HASH,
            ICLPoolEvents::Burn::SIGNATURE_HASH,
            ICLPoolEvents::Swap::SIGNATURE_HASH,
            // Fee change event from FeeModule contract
            // Note: This event is emitted from FeeModule, not the pool
            // The event has `pool` as indexed parameter, so we can filter by pool address
            ICustomFeeModule::CustomFeeSet::SIGNATURE_HASH,
        ]
    }

    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError> {
        let event_signature = log.topics()[0];
        match event_signature {
            ICLPoolEvents::Swap::SIGNATURE_HASH => {
                let swap_event = ICLPoolEvents::Swap::decode_log(log.as_ref())?;
                let tick_after: i32 = swap_event.tick.unchecked_into();

                if swap_event.liquidity != self.liquidity && tick_after == self.tick {
                    tracing::warn!(
                        target: "amms::aerodrome_slipstream::sync",
                        address = ?self.address,
                        local_liquidity = ?self.liquidity,
                        remote_liquidity = ?swap_event.liquidity,
                        local_tick = ?self.tick,
                        remote_tick = ?tick_after,
                        "Liquidity mismatch detected within same tick."
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

                tracing::info!(
                    target: "amms::aerodrome_slipstream::sync",
                    block_number = ?log.block_number,
                    address = ?self.address,
                    sqrt_price = ?self.sqrt_price,
                    liquidity = ?self.liquidity,
                    tick = ?self.tick,
                    "Swap"
                );
            }
            ICLPoolEvents::Mint::SIGNATURE_HASH => {
                let mint_event = ICLPoolEvents::Mint::decode_log(log.as_ref())?;
                self.modify_position(
                    mint_event.tickLower.unchecked_into(),
                    mint_event.tickUpper.unchecked_into(),
                    mint_event.amount as i128,
                )?;

                tracing::info!(
                    target: "amms::aerodrome_slipstream::sync",
                    block_number = ?log.block_number,
                    address = ?self.address,
                    sqrt_price = ?self.sqrt_price,
                    liquidity = ?self.liquidity,
                    tick = ?self.tick,
                    "Mint"
                );
            }
            ICLPoolEvents::Burn::SIGNATURE_HASH => {
                let burn_event = ICLPoolEvents::Burn::decode_log(log.as_ref())?;
                self.modify_position(
                    burn_event.tickLower.unchecked_into(),
                    burn_event.tickUpper.unchecked_into(),
                    -(burn_event.amount as i128),
                )?;

                tracing::info!(
                    target: "amms::aerodrome_slipstream::sync",
                    block_number = ?log.block_number,
                    address = ?self.address,
                    sqrt_price = ?self.sqrt_price,
                    liquidity = ?self.liquidity,
                    tick = ?self.tick,
                    "Burn"
                );
            }
            ICustomFeeModule::CustomFeeSet::SIGNATURE_HASH => {
                // This event is emitted from FeeModule contract, not the pool
                // The event has `pool` as indexed parameter (topic[1])
                // Verify this event is for our pool
                if log.topics().len() > 1 {
                    let event_pool = Address::from_word(log.topics()[1]);
                    if event_pool != self.address {
                        // Not our pool's fee event, skip
                        return Ok(SyncAction::None);
                    }
                }

                let fee_event = ICustomFeeModule::CustomFeeSet::decode_log(log.as_ref())?;
                let old_fee = self.fee;
                self.fee = fee_event.fee.to::<u32>();

                tracing::info!(
                    target: "amms::aerodrome_slipstream::sync",
                    block_number = ?log.block_number,
                    address = ?self.address,
                    old_fee = ?old_fee,
                    new_fee = ?self.fee,
                    fee_percent = ?(self.fee as f64 / 10000.0),
                    "CustomFeeSet"
                );
            }
            _ => {
                tracing::info!(
                    target: "amms::aerodrome_slipstream::sync",
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

        // Defensive checks
        if self.sqrt_price.is_zero() {
            return Err(AMMError::Msg("sqrt_price is zero".into()));
        }
        if self.liquidity == 0 {
            return Err(AMMError::Msg("liquidity is zero".into()));
        }

        let zero_for_one = base_token == self.token_a.address;
        let sqrt_price_limit_x_96 = if zero_for_one {
            MIN_SQRT_RATIO + U256_1
        } else {
            MAX_SQRT_RATIO - U256_1
        };

        let mut current_state = CurrentState {
            sqrt_price_x_96: self.sqrt_price,
            amount_calculated: I256::ZERO,
            amount_specified_remaining: I256::from_raw(amount_in),
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

            (step.tick_next, step.initialized) =
                uniswap_v3_math::tick_bitmap::next_initialized_tick_within_one_word(
                    &self.tick_bitmap,
                    current_state.tick,
                    self.tick_spacing,
                    zero_for_one,
                )
                .map_err(AerodromeSlipstreamError::from)?;

            step.tick_next = step.tick_next.clamp(MIN_TICK, MAX_TICK);

            step.sqrt_price_next_x96 =
                uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(step.tick_next)
                    .map_err(AerodromeSlipstreamError::from)?;

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
                .map_err(AerodromeSlipstreamError::from)?;
            }

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
                        return Err(AMMError::Msg(format!(
                            "Tick data missing for tick {}",
                            step.tick_next
                        )));
                    };

                    if zero_for_one {
                        liquidity_net = -liquidity_net;
                    }

                    current_state.liquidity = if liquidity_net < 0 {
                        if current_state.liquidity < (-liquidity_net as u128) {
                            return Err(AMMError::Msg("Liquidity underflow".into()));
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
                }
            } else if current_state.sqrt_price_x_96 != step.sqrt_price_start_x_96 {
                current_state.tick = uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(
                    current_state.sqrt_price_x_96,
                )
                .map_err(AerodromeSlipstreamError::from)?;
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

        if self.sqrt_price.is_zero() {
            return Err(AMMError::Msg("sqrt_price is zero".into()));
        }

        let zero_for_one = base_token == self.token_a.address;
        let sqrt_price_limit_x_96 = if zero_for_one {
            MIN_SQRT_RATIO + U256_1
        } else {
            MAX_SQRT_RATIO - U256_1
        };

        let mut current_state = CurrentState {
            sqrt_price_x_96: self.sqrt_price,
            amount_calculated: I256::ZERO,
            amount_specified_remaining: I256::from_raw(amount_in),
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

            (step.tick_next, step.initialized) =
                uniswap_v3_math::tick_bitmap::next_initialized_tick_within_one_word(
                    &self.tick_bitmap,
                    current_state.tick,
                    self.tick_spacing,
                    zero_for_one,
                )
                .map_err(AerodromeSlipstreamError::from)?;

            step.tick_next = step.tick_next.clamp(MIN_TICK, MAX_TICK);

            step.sqrt_price_next_x96 =
                uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(step.tick_next)
                    .map_err(AerodromeSlipstreamError::from)?;

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
                .map_err(AerodromeSlipstreamError::from)?;
            }

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
                        return Err(AMMError::Msg(format!(
                            "Tick data missing for tick {}",
                            step.tick_next
                        )));
                    };

                    if zero_for_one {
                        liquidity_net = -liquidity_net;
                    }

                    current_state.liquidity = if liquidity_net < 0 {
                        if current_state.liquidity < (-liquidity_net as u128) {
                            return Err(AMMError::Msg("Liquidity underflow".into()));
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
                }
            } else if current_state.sqrt_price_x_96 != step.sqrt_price_start_x_96 {
                current_state.tick = uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(
                    current_state.sqrt_price_x_96,
                )
                .map_err(AerodromeSlipstreamError::from)?;
            }
        }

        // Update pool state
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

        if self.sqrt_price.is_zero() {
            return Err(AMMError::Msg("sqrt_price is zero".into()));
        }
        if self.liquidity == 0 {
            return Err(AMMError::Msg("liquidity is zero".into()));
        }

        let zero_for_one = base_token == self.token_a.address;
        let sqrt_price_limit_x_96 = if zero_for_one {
            MIN_SQRT_RATIO + U256_1
        } else {
            MAX_SQRT_RATIO - U256_1
        };

        let mut current_state = CurrentState {
            sqrt_price_x_96: self.sqrt_price,
            amount_calculated: I256::ZERO,
            amount_specified_remaining: I256::ZERO - I256::from_raw(amount_out),
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

            (step.tick_next, step.initialized) =
                uniswap_v3_math::tick_bitmap::next_initialized_tick_within_one_word(
                    &self.tick_bitmap,
                    current_state.tick,
                    self.tick_spacing,
                    zero_for_one,
                )
                .map_err(AerodromeSlipstreamError::from)?;

            step.tick_next = step.tick_next.clamp(MIN_TICK, MAX_TICK);

            step.sqrt_price_next_x96 =
                uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(step.tick_next)
                    .map_err(AerodromeSlipstreamError::from)?;

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
                .map_err(AerodromeSlipstreamError::from)?;
            }

            // Exact output path:
            // - remaining specified output moves toward 0 by produced amount_out
            // - calculated input accumulates amount_in + fee
            current_state.amount_specified_remaining = current_state
                .amount_specified_remaining
                .overflowing_add(I256::from_raw(step.amount_out))
                .0;

            current_state.amount_calculated +=
                I256::from_raw(step.amount_in.overflowing_add(step.fee_amount).0);

            if current_state.sqrt_price_x_96 == step.sqrt_price_next_x96 {
                if step.initialized {
                    let mut liquidity_net = if let Some(info) = self.ticks.get(&step.tick_next) {
                        info.liquidity_net
                    } else {
                        return Err(
                            AerodromeSlipstreamError::TickDataMissing(step.tick_next).into()
                        );
                    };

                    if zero_for_one {
                        liquidity_net = -liquidity_net;
                    }

                    current_state.liquidity = if liquidity_net < 0 {
                        if current_state.liquidity < (-liquidity_net as u128) {
                            return Err(AerodromeSlipstreamError::LiquidityUnderflow.into());
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
                }
            } else if current_state.sqrt_price_x_96 != step.sqrt_price_start_x_96 {
                current_state.tick = uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(
                    current_state.sqrt_price_x_96,
                )
                .map_err(AerodromeSlipstreamError::from)?;
            }
        }

        if current_state.amount_specified_remaining != I256::ZERO {
            return Err(AMMError::Msg(
                "insufficient liquidity for exact out".to_string(),
            ));
        }

        Ok(current_state.amount_calculated.into_raw())
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
        let sqrt_price_str = sqrt_price_x96.to_string();
        let sqrt_price_val = Float::parse_radix(&sqrt_price_str, 10)
            .map_err(|e| AMMError::Msg(format!("Float parse error: {}", e)))?;
        let sqrt_price_float = Float::with_val(MPFR_T_PRECISION, sqrt_price_val);

        let mut denom = Float::with_val(MPFR_T_PRECISION, 1);
        denom <<= 96u32;

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

        if price <= 0.0 || !price.is_finite() {
            return Err(AMMError::Msg("Invalid cached spot price".to_string()));
        }

        Ok(price)
    }

    fn has_sufficient_liquidity(&self) -> bool {
        let d_a = self.token_a.decimals;
        let d_b = self.token_b.decimals;

        let t_a_u128 = if d_a >= 18 {
            10u128.pow(d_a as u32 - 4)
        } else if d_a >= 6 {
            100u128.saturating_mul(10u128.pow(d_a as u32))
        } else {
            100_000
        };

        let t_b_u128 = if d_b >= 18 {
            10u128.pow(d_b as u32 - 4)
        } else if d_b >= 6 {
            100u128.saturating_mul(10u128.pow(d_b as u32))
        } else {
            100_000
        };

        let l_thresh = if let Some(prod) = t_a_u128.checked_mul(t_b_u128) {
            prod.isqrt()
        } else {
            u128::MAX.isqrt()
        };

        // Fast path: active in-range liquidity already meets the threshold.
        if self.liquidity >= l_thresh {
            return true;
        }

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

    /// Aerodrome Slipstream is only deployed on Base chain (chain ID: 8453)
    fn supported_chains(&self) -> Option<Vec<u64>> {
        Some(vec![8453]) // Base mainnet
    }

    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let pool = ICLPool::new(self.address, provider.clone());

        self.tick_spacing = pool
            .tickSpacing()
            .block(block_number)
            .call()
            .await?
            .as_i32();
        if self.tick_spacing == 0 {
            return Err(AMMError::Msg("tick_spacing is zero".into()));
        }

        // IMPORTANT: Use block_number to get historical fee value
        // Slipstream has dynamic fee mechanism, fee can change over time
        self.fee = pool.fee().block(block_number).call().await?.to::<u32>();

        self.token_a = Token::new(
            pool.token0().block(block_number).call().await?,
            provider.clone(),
        )
        .await?;
        self.token_b = Token::new(
            pool.token1().block(block_number).call().await?,
            provider.clone(),
        )
        .await?;

        let mut pool = vec![self.into()];
        AerodromeSlipstreamFactory::sync_slot_0(&mut pool, block_number, provider.clone()).await?;
        AerodromeSlipstreamFactory::sync_token_decimals(&mut pool, provider.clone()).await?;

        let AMM::AerodromeSlipstreamPool(mut pool_struct) = pool[0].to_owned() else {
            unreachable!()
        };

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

// ============================================================================
// AerodromeSlipstreamPool Internal Methods
// ============================================================================

impl AerodromeSlipstreamPool {
    /// Modifies a position's liquidity in the pool
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
                    self.liquidity.saturating_sub((-liquidity_delta) as u128)
                } else {
                    self.liquidity.saturating_add(liquidity_delta as u128)
                };
            }
        }

        Ok(())
    }

    fn update_position(
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

    fn update_tick(
        &mut self,
        tick: i32,
        liquidity_delta: i128,
        upper: bool,
    ) -> Result<bool, AMMError> {
        let info = self.ticks.entry(tick).or_default();

        let liquidity_gross_before = info.liquidity_gross;

        let liquidity_gross_after = if liquidity_delta < 0 {
            liquidity_gross_before.saturating_sub((-liquidity_delta) as u128)
        } else {
            liquidity_gross_before.saturating_add(liquidity_delta as u128)
        };

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

    /// Compress a tick by dividing by tick spacing
    pub fn compress_tick(tick: i32, tick_spacing: i32) -> i32 {
        tick.div_euclid(tick_spacing)
    }

    /// Convert tick to word position in bitmap
    pub fn tick_to_word(tick: i32, tick_spacing: i32) -> i32 {
        Self::compress_tick(tick, tick_spacing) >> 8
    }

    pub fn flip_tick(&mut self, tick: i32, tick_spacing: i32, initialized: bool) {
        let compressed = Self::compress_tick(tick, tick_spacing);
        let (word_pos, bit_pos) = uniswap_v3_math::tick_bitmap::position(compressed);
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

    pub fn swap_calldata(
        &self,
        recipient: Address,
        zero_for_one: bool,
        amount_specified: I256,
        sqrt_price_limit_x_96: U256,
        calldata: Vec<u8>,
    ) -> Result<alloy::primitives::Bytes, AMMError> {
        Ok(ICLPool::swapCall {
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

// ============================================================================
// AerodromeSlipstreamFactory Implementation
// ============================================================================

impl AutomatedMarketMakerFactory for AerodromeSlipstreamFactory {
    type PoolVariant = AerodromeSlipstreamPool;

    fn address(&self) -> Address {
        self.address
    }

    fn creation_block(&self) -> u64 {
        self.creation_block
    }

    fn pool_creation_event(&self) -> alloy::primitives::FixedBytes<32> {
        ICLPoolFactory::PoolCreated::SIGNATURE_HASH
    }

    fn create_pool(&self, log: Log) -> Result<AMM, AMMError> {
        let pool_created_event = ICLPoolFactory::PoolCreated::decode_log(&log.inner)?;
        let pool = AerodromeSlipstreamPool::new(pool_created_event.pool);
        Ok(AMM::AerodromeSlipstreamPool(pool))
    }
}

impl AerodromeSlipstreamFactory {
    pub async fn sync_slot_0<N, P>(
        pools: &mut [AMM],
        block_number: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let step = 150;
        let mut futures = futures::stream::FuturesUnordered::new();

        for group in pools.chunks_mut(step) {
            let provider = provider.clone();
            let pool_addresses = group
                .iter_mut()
                .map(|pool| pool.address())
                .collect::<Vec<_>>();

            futures.push(async move {
                Ok::<(&mut [AMM], alloy::primitives::Bytes), AMMError>((
                    group,
                    GetAerodromeSlipstreamSlot0BatchRequest::deploy_builder(
                        provider,
                        pool_addresses,
                    )
                    .call_raw()
                    .block(block_number)
                    .await?,
                ))
            });
            sleep(Duration::from_millis(500)).await;
        }

        while let Some(res) = futures.next().await {
            let (group, return_data) = match res {
                Ok(data) => data,
                Err(e) => {
                    tracing::warn!(
                        target = "amms::aerodrome_slipstream::sync_slot_0",
                        error = ?e,
                        "Batch slot0 call failed, skipping batch"
                    );
                    continue;
                }
            };
            use alloy::sol_types::SolValue;

            let return_data = match <Vec<(u32, u128, U256)> as SolValue>::abi_decode(&return_data) {
                Ok(data) => data,
                Err(e) => {
                    tracing::warn!(
                        target = "amms::aerodrome_slipstream::sync_slot_0",
                        error = ?e,
                        return_data_len = return_data.len(),
                        "Failed to decode slot0 data, skipping batch"
                    );
                    continue;
                }
            };

            for (pool, (tick, liquidity, sqrt_price)) in
                group.iter_mut().zip(return_data.into_iter())
            {
                let AMM::AerodromeSlipstreamPool(pool) = pool else {
                    continue;
                };

                pool.tick = tick as i32;
                pool.liquidity = liquidity;
                pool.sqrt_price = sqrt_price;

                if let Ok(price) = pool.calculate_price(pool.token_a.address, pool.token_b.address)
                {
                    pool.token_a_price = price;
                    if price != 0.0 {
                        pool.token_b_price = 1.0 / price;
                    } else {
                        pool.token_b_price = 0.0;
                    }
                }
            }
        }

        Ok(())
    }

    pub async fn sync_token_decimals<N, P>(
        pools: &mut [AMM],
        provider: P,
    ) -> Result<(), crate::amms::error::BatchContractError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        use std::collections::HashSet;

        let mut tokens = HashSet::new();
        for pool in pools.iter() {
            for token in pool.tokens() {
                tokens.insert(token);
            }
        }

        let token_decimals =
            crate::amms::get_token_decimals(tokens.into_iter().collect(), provider).await?;

        for pool in pools.iter_mut() {
            let AMM::AerodromeSlipstreamPool(aerodrome_pool) = pool else {
                continue;
            };

            if let Some(decimals) = token_decimals.get(&aerodrome_pool.token_a.address) {
                aerodrome_pool.token_a.decimals = *decimals;
            }

            if let Some(decimals) = token_decimals.get(&aerodrome_pool.token_b.address) {
                aerodrome_pool.token_b.decimals = *decimals;
            }
        }

        Ok(())
    }

    /// Batch sync tick bitmaps using the same batch contract as UniswapV3.
    /// Aerodrome Slipstream tickBitmap interface is ABI-compatible with UniswapV3.
    pub async fn sync_tick_bitmaps<N, P>(
        pools: &mut [AMM],
        block_number: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        use crate::amms::uniswap_v3::GetUniswapV3PoolTickBitmapBatchRequest::TickBitmapInfo;

        let max_range = 300;
        let max_in_flight = 8;
        let mut group_range = 0;
        let mut group = vec![];
        let mut jobs: Vec<(Vec<Address>, Vec<TickBitmapInfo>)> = Vec::new();

        for pool in pools.iter() {
            let AMM::AerodromeSlipstreamPool(slipstream_pool) = pool else {
                continue;
            };

            let mut min_word = tick_to_word(MIN_TICK, slipstream_pool.tick_spacing);
            let max_word = tick_to_word(MAX_TICK, slipstream_pool.tick_spacing);

            while min_word <= max_word {
                let remaining_range = max_range - group_range;
                let word_range = max_word - min_word + 1;
                let range = word_range.min(remaining_range);

                let start = min_word;
                let end = start + range - 1;

                group.push(TickBitmapInfo {
                    pool: slipstream_pool.address,
                    minWord: start as i16,
                    maxWord: end as i16,
                });

                min_word = end + 1;
                group_range += range;

                if group_range >= max_range {
                    let pool_info = group.iter().map(|info| info.pool).collect::<Vec<_>>();
                    let calldata = std::mem::take(&mut group);
                    jobs.push((pool_info, calldata));
                    group_range = 0;
                }
            }
        }

        if !group.is_empty() {
            let pool_info = group.iter().map(|info| info.pool).collect::<Vec<_>>();
            let calldata = std::mem::take(&mut group);
            jobs.push((pool_info, calldata));
        }

        let mut pool_index = HashMap::new();
        for (idx, pool) in pools.iter().enumerate() {
            pool_index.insert(pool.address(), idx);
        }

        let mut futures: FuturesUnordered<BoxFuture<'_, _>> = FuturesUnordered::new();

        for (pool_info, calldata) in jobs {
            let provider = provider.clone();
            futures.push(Box::pin(async move {
                let mut attempt = 1u8;
                loop {
                    match GetUniswapV3PoolTickBitmapBatchRequest::deploy_builder(
                        provider.clone(),
                        calldata.clone(),
                    )
                    .call_raw()
                    .block(block_number)
                    .await
                    {
                        Ok(return_data) => {
                            break Ok::<(Vec<Address>, Bytes), AMMError>((pool_info, return_data));
                        }
                        Err(e) if attempt < SLIPSTREAM_BATCH_RETRY_ATTEMPTS => {
                            let delay = slipstream_retry_delay(attempt);
                            tracing::warn!(
                                target = "amms::aerodrome_slipstream::sync_tick_bitmaps",
                                attempt,
                                max_attempts = SLIPSTREAM_BATCH_RETRY_ATTEMPTS,
                                error = ?e,
                                "Batch tick bitmap call failed, retrying"
                            );
                            sleep(delay).await;
                            attempt = attempt.saturating_add(1);
                        }
                        Err(e) => break Err(e.into()),
                    }
                }
            }));

            if futures.len() >= max_in_flight {
                if let Some(res) = futures.next().await {
                    let (pools_addrs, return_data) = res.map_err(|e| {
                        AMMError::Msg(format!(
                            "Slipstream tick bitmap batch failed after retries: {}",
                            e
                        ))
                    })?;
                    let return_data = match <Vec<Vec<U256>> as SolValue>::abi_decode(&return_data) {
                        Ok(data) => data,
                        Err(e) => {
                            tracing::warn!(
                                target = "amms::aerodrome_slipstream::sync_tick_bitmaps",
                                error = ?e,
                                return_data_len = return_data.len(),
                                "Failed to decode tick bitmap data"
                            );
                            return Err(AMMError::Msg(format!(
                                "Slipstream tick bitmap decode failed: {}",
                                e
                            )));
                        }
                    };

                    for (tick_bitmaps, pool_address) in return_data.iter().zip(pools_addrs.iter()) {
                        let Some(pool_idx) = pool_index.get(pool_address).copied() else {
                            continue;
                        };
                        let AMM::AerodromeSlipstreamPool(ref mut slipstream_pool) = pools[pool_idx]
                        else {
                            continue;
                        };

                        for chunk in tick_bitmaps.chunks_exact(2) {
                            let word_pos = I256::from_raw(chunk[0]).as_i16();
                            let tick_bitmap = chunk[1];
                            slipstream_pool.tick_bitmap.insert(word_pos, tick_bitmap);
                        }
                    }
                }
            }
        }

        while let Some(res) = futures.next().await {
            let (pools_addrs, return_data) = res.map_err(|e| {
                AMMError::Msg(format!(
                    "Slipstream tick bitmap batch failed after retries: {}",
                    e
                ))
            })?;
            let return_data = match <Vec<Vec<U256>> as SolValue>::abi_decode(&return_data) {
                Ok(data) => data,
                Err(e) => {
                    tracing::warn!(
                        target = "amms::aerodrome_slipstream::sync_tick_bitmaps",
                        error = ?e,
                        return_data_len = return_data.len(),
                        "Failed to decode tick bitmap data"
                    );
                    return Err(AMMError::Msg(format!(
                        "Slipstream tick bitmap decode failed: {}",
                        e
                    )));
                }
            };

            for (tick_bitmaps, pool_address) in return_data.iter().zip(pools_addrs.iter()) {
                let Some(pool_idx) = pool_index.get(pool_address).copied() else {
                    continue;
                };
                let AMM::AerodromeSlipstreamPool(ref mut slipstream_pool) = pools[pool_idx] else {
                    continue;
                };

                for chunk in tick_bitmaps.chunks_exact(2) {
                    let word_pos = I256::from_raw(chunk[0]).as_i16();
                    let tick_bitmap = chunk[1];
                    slipstream_pool.tick_bitmap.insert(word_pos, tick_bitmap);
                }
            }
        }
        Ok(())
    }

    /// Batch sync tick data using the same batch contract as UniswapV3.
    /// Aerodrome Slipstream ticks() interface is ABI-compatible with UniswapV3.
    pub async fn sync_tick_data<N, P>(
        pools: &mut [AMM],
        block_number: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        use GetAerodromeSlipstreamPoolTickDataBatchRequest::TickDataInfo;

        let pool_ticks = pools
            .par_iter()
            .filter_map(|pool| {
                if let AMM::AerodromeSlipstreamPool(slipstream_pool) = pool {
                    let min_word = tick_to_word(MIN_TICK, slipstream_pool.tick_spacing);
                    let max_word = tick_to_word(MAX_TICK, slipstream_pool.tick_spacing);

                    let initialized_ticks: Vec<Signed<24, 1>> = (min_word..=max_word)
                        .filter_map(|word_pos| {
                            slipstream_pool
                                .tick_bitmap
                                .get(&(word_pos as i16))
                                .filter(|&bitmap| *bitmap != U256::ZERO)
                                .map(|&bitmap| (word_pos, bitmap))
                        })
                        .flat_map(|(word_pos, bitmap)| {
                            (0..256)
                                .filter(move |i| {
                                    (bitmap & (U256::from(1) << U256::from(*i))) != U256::ZERO
                                })
                                .filter_map(move |i| {
                                    let tick_index =
                                        (word_pos * 256 + i) * slipstream_pool.tick_spacing;
                                    Signed::<24, 1>::try_from(tick_index).ok()
                                })
                        })
                        .collect();

                    if !initialized_ticks.is_empty() {
                        Some((slipstream_pool.address, initialized_ticks))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect::<Vec<(Address, Vec<Signed<24, 1>>)>>();

        let max_in_flight = 8;
        let max_ticks = 60;
        let mut group_ticks = 0;
        let mut group = vec![];
        let mut jobs: Vec<Vec<TickDataInfo>> = Vec::new();

        for (pool_address, mut ticks) in pool_ticks {
            while !ticks.is_empty() {
                let remaining_ticks = max_ticks - group_ticks;
                let selected_ticks = ticks.drain(0..remaining_ticks.min(ticks.len()));
                group_ticks += selected_ticks.len();

                group.push(TickDataInfo {
                    pool: pool_address,
                    ticks: selected_ticks.collect(),
                });

                if group_ticks >= max_ticks {
                    let calldata = std::mem::take(&mut group);
                    jobs.push(calldata);
                    group_ticks = 0;
                    group.clear();
                }
            }
        }

        if !group.is_empty() {
            let calldata = std::mem::take(&mut group);
            jobs.push(calldata);
        }

        let mut pool_index = HashMap::new();
        for (idx, pool) in pools.iter().enumerate() {
            pool_index.insert(pool.address(), idx);
        }

        let mut futures: FuturesUnordered<BoxFuture<'_, _>> = FuturesUnordered::new();

        for calldata in jobs {
            let provider = provider.clone();
            let calldata_clone = calldata.clone();
            futures.push(Box::pin(async move {
                let mut attempt = 1u8;
                loop {
                    match GetAerodromeSlipstreamPoolTickDataBatchRequest::deploy_builder(
                        provider.clone(),
                        calldata.clone(),
                    )
                    .call_raw()
                    .block(block_number)
                    .await
                    {
                        Ok(return_data) => {
                            break Ok::<(Vec<TickDataInfo>, Bytes), AMMError>((
                                calldata_clone,
                                return_data,
                            ));
                        }
                        Err(e) if attempt < SLIPSTREAM_BATCH_RETRY_ATTEMPTS => {
                            let delay = slipstream_retry_delay(attempt);
                            tracing::warn!(
                                target = "amms::aerodrome_slipstream::sync_tick_data",
                                attempt,
                                max_attempts = SLIPSTREAM_BATCH_RETRY_ATTEMPTS,
                                error = ?e,
                                "Batch tick data call failed, retrying"
                            );
                            sleep(delay).await;
                            attempt = attempt.saturating_add(1);
                        }
                        Err(e) => break Err(e.into()),
                    }
                }
            }));

            if futures.len() >= max_in_flight {
                if let Some(res) = futures.next().await {
                    let (tick_info, return_data) = res.map_err(|e| {
                        AMMError::Msg(format!(
                            "Slipstream tick data batch failed after retries: {}",
                            e
                        ))
                    })?;
                    let return_data =
                        match <Vec<Vec<(u128, i128)>> as SolValue>::abi_decode(&return_data) {
                            Ok(data) => data,
                            Err(e) => {
                                tracing::warn!(
                                    target = "amms::aerodrome_slipstream::sync_tick_data",
                                    error = ?e,
                                    return_data_len = return_data.len(),
                                    "Failed to decode tick data"
                                );
                                return Err(AMMError::Msg(format!(
                                    "Slipstream tick data decode failed: {}",
                                    e
                                )));
                            }
                        };

                    for (tick_results, tick_info_item) in return_data.iter().zip(tick_info.iter()) {
                        let Some(pool_idx) = pool_index.get(&tick_info_item.pool).copied() else {
                            continue;
                        };
                        let AMM::AerodromeSlipstreamPool(ref mut slipstream_pool) = pools[pool_idx]
                        else {
                            continue;
                        };

                        for (tick, tick_idx) in tick_results.iter().zip(tick_info_item.ticks.iter())
                        {
                            let info = TickInfo {
                                liquidity_gross: tick.0,
                                liquidity_net: tick.1,
                                initialized: tick.0 > 0,
                            };
                            slipstream_pool.ticks.insert(tick_idx.as_i32(), info);
                        }
                    }
                }
            }
        }

        while let Some(res) = futures.next().await {
            let (tick_info, return_data) = res.map_err(|e| {
                AMMError::Msg(format!(
                    "Slipstream tick data batch failed after retries: {}",
                    e
                ))
            })?;
            let return_data = match <Vec<Vec<(u128, i128)>> as SolValue>::abi_decode(&return_data) {
                Ok(data) => data,
                Err(e) => {
                    tracing::warn!(
                        target = "amms::aerodrome_slipstream::sync_tick_data",
                        error = ?e,
                        return_data_len = return_data.len(),
                        "Failed to decode tick data"
                    );
                    return Err(AMMError::Msg(format!(
                        "Slipstream tick data decode failed: {}",
                        e
                    )));
                }
            };

            for (tick_results, tick_info_item) in return_data.iter().zip(tick_info.iter()) {
                let Some(pool_idx) = pool_index.get(&tick_info_item.pool).copied() else {
                    continue;
                };
                let AMM::AerodromeSlipstreamPool(ref mut slipstream_pool) = pools[pool_idx] else {
                    continue;
                };

                for (tick, tick_idx) in tick_results.iter().zip(tick_info_item.ticks.iter()) {
                    let info = TickInfo {
                        liquidity_gross: tick.0,
                        liquidity_net: tick.1,
                        initialized: tick.0 > 0,
                    };
                    slipstream_pool.ticks.insert(tick_idx.as_i32(), info);
                }
            }
        }
        Ok(())
    }

    /// Batch initialize pools with all necessary data (static metadata, slot0, tick bitmaps, tick data)
    pub async fn init_batch<N, P>(
        mut amms: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        use crate::amms::uniswap_v3::GetUniswapV3PoolStaticMetaBatchRequest;

        let step = 150;
        let mut futures: FuturesUnordered<BoxFuture<'_, _>> = FuturesUnordered::new();

        let pool_addresses: Vec<Address> = amms.iter().map(|p| p.address()).collect();

        for chunk in pool_addresses.chunks(step) {
            let provider = provider.clone();
            let addresses = chunk.to_vec();
            let addresses_clone = addresses.clone();
            futures.push(Box::pin(async move {
                Ok::<(Vec<Address>, Bytes), AMMError>((
                    addresses,
                    GetUniswapV3PoolStaticMetaBatchRequest::deploy_builder(
                        provider,
                        addresses_clone,
                    )
                    .call_raw()
                    .block(block_number)
                    .await?,
                ))
            }));
            sleep(Duration::from_millis(500)).await;
        }

        while let Some(res) = futures.next().await {
            let (addresses, return_data) = match res {
                Ok(data) => data,
                Err(e) => {
                    tracing::warn!(
                        target = "amms::aerodrome_slipstream::init_batch",
                        error = ?e,
                        "Batch static meta call failed, skipping batch"
                    );
                    continue;
                }
            };
            let static_data =
                match <Vec<(Address, Address, i32, u32)> as SolValue>::abi_decode(&return_data) {
                    Ok(data) => data,
                    Err(e) => {
                        tracing::warn!(
                            target = "amms::aerodrome_slipstream::init_batch",
                            error = ?e,
                            return_data_len = return_data.len(),
                            "Failed to decode static data, skipping batch"
                        );
                        continue;
                    }
                };

            let addr_to_data: HashMap<Address, (Address, Address, i32, u32)> =
                addresses.into_iter().zip(static_data.into_iter()).collect();

            for pool in amms.iter_mut() {
                if let Some((token0, token1, tick_spacing, fee)) = addr_to_data.get(&pool.address())
                {
                    let AMM::AerodromeSlipstreamPool(ref mut slipstream_pool) = pool else {
                        continue;
                    };
                    slipstream_pool.token_a.address = *token0;
                    slipstream_pool.token_b.address = *token1;
                    slipstream_pool.tick_spacing = *tick_spacing;
                    slipstream_pool.fee = *fee;
                }
            }
        }

        Self::sync_slot_0(&mut amms, block_number, provider.clone()).await?;
        sleep(Duration::from_millis(500)).await;

        Self::sync_token_decimals(&mut amms, provider.clone())
            .await
            .map_err(AMMError::from)?;
        sleep(Duration::from_millis(500)).await;

        let (structurally_valid, structurally_invalid): (Vec<_>, Vec<_>) =
            amms.into_iter().partition(|amm| {
                if let AMM::AerodromeSlipstreamPool(pool) = amm {
                    pool.tick_spacing != 0
                        && !pool.token_a.address.is_zero()
                        && !pool.token_b.address.is_zero()
                        && pool.token_a.decimals > 0
                        && pool.token_b.decimals > 0
                } else {
                    false
                }
            });
        let structural_total = structurally_valid.len() + structurally_invalid.len();

        if !structurally_invalid.is_empty() {
            for amm in &structurally_invalid {
                tracing::warn!(
                    target = "amms::aerodrome_slipstream::init_batch",
                    addr = ?amm.address(),
                    "Filtered out structurally invalid pool"
                );
            }
        }

        let mut valid_amms = structurally_valid;
        let pools_step = 50;
        for group in valid_amms.chunks_mut(pools_step) {
            Self::sync_tick_bitmaps(group, block_number, provider.clone()).await?;
            sleep(Duration::from_millis(500)).await;
            Self::sync_tick_data(group, block_number, provider.clone()).await?;
            sleep(Duration::from_millis(500)).await;
        }

        let (liquid_amms, dust_amms): (Vec<_>, Vec<_>) = valid_amms.into_iter().partition(|amm| {
            if let AMM::AerodromeSlipstreamPool(pool) = amm {
                pool.has_sufficient_liquidity()
            } else {
                false
            }
        });

        if !dust_amms.is_empty() {
            for amm in &dust_amms {
                if let AMM::AerodromeSlipstreamPool(pool) = amm {
                    tracing::warn!(
                        target = "amms::aerodrome_slipstream::init_batch",
                        addr = ?pool.address,
                        liquidity = pool.liquidity,
                        ticks = pool.ticks.len(),
                        "Filtering out dust Slipstream pool by has_sufficient_liquidity"
                    );
                }
            }
        }

        let mut valid_amms = liquid_amms;
        for pool in valid_amms.iter_mut() {
            let AMM::AerodromeSlipstreamPool(ref mut slipstream_pool) = pool else {
                continue;
            };
            if let Ok(price) = slipstream_pool.calculate_price(
                slipstream_pool.token_a.address,
                slipstream_pool.token_b.address,
            ) {
                slipstream_pool.token_a_price = price;
                if price != 0.0 {
                    slipstream_pool.token_b_price = 1.0 / price;
                }
            }
        }

        tracing::info!(
            target = "amms::aerodrome_slipstream::init_batch",
            total = structural_total,
            valid = valid_amms.len(),
            invalid = structurally_invalid.len() + dust_amms.len(),
            "Batch initialization complete"
        );

        Ok(valid_amms)
    }

    pub async fn sync_all_pools<N, P>(
        mut amms: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        use crate::amms::uniswap_v3::GetUniswapV3PoolStaticMetaBatchRequest;

        // Slipstream fee is dynamic; refresh it at the target block so sync snapshots stay accurate
        // even if fee events were missed.
        let pool_addresses: Vec<Address> = amms.iter().map(|p| p.address()).collect();
        if !pool_addresses.is_empty() {
            let mut fee_map: HashMap<Address, u32> = HashMap::new();
            let step = 150;
            for chunk in pool_addresses.chunks(step) {
                let chunk_addrs = chunk.to_vec();
                let call_result = GetUniswapV3PoolStaticMetaBatchRequest::deploy_builder(
                    provider.clone(),
                    chunk_addrs.clone(),
                )
                .call_raw()
                .block(block_number)
                .await;

                let Ok(return_data) = call_result else {
                    tracing::warn!(
                        target = "amms::aerodrome_slipstream::sync",
                        "Batch static meta call failed while refreshing slipstream fee; skipping chunk"
                    );
                    continue;
                };

                let decoded =
                    <Vec<(Address, Address, i32, u32)> as SolValue>::abi_decode(&return_data);
                let Ok(meta) = decoded else {
                    tracing::warn!(
                        target = "amms::aerodrome_slipstream::sync",
                        return_data_len = return_data.len(),
                        "Failed to decode static meta while refreshing slipstream fee; skipping chunk"
                    );
                    continue;
                };

                for ((_, _, _, fee), pool_addr) in meta.iter().zip(chunk_addrs.iter()) {
                    fee_map.insert(*pool_addr, *fee);
                }

                sleep(Duration::from_millis(500)).await;
            }

            for amm in amms.iter_mut() {
                let AMM::AerodromeSlipstreamPool(pool) = amm else {
                    continue;
                };
                if let Some(fee) = fee_map.get(&pool.address).copied() {
                    pool.fee = fee;
                }
            }
        }

        Self::sync_slot_0(&mut amms, block_number, provider.clone()).await?;
        sleep(Duration::from_millis(500)).await;

        for amm in amms.iter_mut() {
            let AMM::AerodromeSlipstreamPool(pool) = amm else {
                continue;
            };
            pool.tick_bitmap.clear();
            pool.ticks.clear();
        }

        let pools_step = 50;
        for group in amms.chunks_mut(pools_step) {
            Self::sync_tick_bitmaps(group, block_number, provider.clone()).await?;
            sleep(Duration::from_millis(500)).await;
            Self::sync_tick_data(group, block_number, provider.clone()).await?;
            sleep(Duration::from_millis(500)).await;
        }

        for amm in amms.iter_mut() {
            let AMM::AerodromeSlipstreamPool(pool) = amm else {
                continue;
            };
            match pool.calculate_price(pool.token_a.address, pool.token_b.address) {
                Ok(price) => {
                    pool.token_a_price = price;
                    pool.token_b_price = if price != 0.0 { 1.0 / price } else { 0.0 };
                }
                Err(e) => {
                    tracing::warn!(
                        target = "amms::aerodrome_slipstream::sync",
                        address = ?pool.address,
                        error = ?e,
                        "Failed to refresh Slipstream spot prices; keeping previous values"
                    );
                }
            }
        }

        Ok(amms)
    }
}

impl DiscoverySync for AerodromeSlipstreamFactory {
    fn discover<N, P>(
        &self,
        to_block: BlockId,
        provider: P,
    ) -> impl std::future::Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: alloy::network::Network,
        P: Provider<N> + Clone,
    {
        let address = self.address;
        let creation_block = self.creation_block;
        async move {
            // Use UniswapV3 factory's get_all_pools method (compatible interface)
            let pools = UniswapV3Factory::new(address, creation_block)
                .get_all_pools::<N, _>(to_block, provider.clone())
                .await?;

            // Convert to AerodromeSlipstreamPool
            Ok(pools
                .into_iter()
                .filter_map(|amm| {
                    if let AMM::UniswapV3Pool(pool) = amm {
                        Some(AMM::AerodromeSlipstreamPool(AerodromeSlipstreamPool {
                            address: pool.address,
                            token_a: pool.token_a,
                            token_b: pool.token_b,
                            fee: pool.fee,
                            tick_spacing: pool.tick_spacing,
                            ..Default::default()
                        }))
                    } else {
                        None
                    }
                })
                .collect())
        }
    }

    fn sync<N, P>(
        &self,
        amms: Vec<AMM>,
        to_block: BlockId,
        provider: P,
    ) -> impl std::future::Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: alloy::network::Network,
        P: Provider<N> + Clone,
    {
        async move { AerodromeSlipstreamFactory::init_batch::<N, _>(amms, to_block, provider).await }
    }
}

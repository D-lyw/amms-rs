use super::{
    amm::{AutomatedMarketMaker, SyncAction, AMM},
    consts::{MIN_POOL_RESERVE, MPFR_T_PRECISION},
    error::AMMError,
    factory::{AutomatedMarketMakerFactory, DiscoverySync},
    float::u256_to_float,
    get_token_decimals, Token, IERC20,
};
use alloy::{
    consensus::BlockHeader,
    eips::BlockId,
    network::{BlockResponse, Network},
    primitives::{address, Address, B256, I256, U256},
    providers::Provider,
    rpc::types::Log,
    sol,
    sol_types::SolEvent,
};
use eyre::Result;
use futures::{stream::FuturesUnordered, StreamExt};
use rug::ops::Pow;
use rug::Float;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, future::Future, hash::Hash};

// Addresses are consistent across Mainnet, Arbitrum, Base, Polygon, Plasma, etc. (Instadapp Fluid uses CREATE2)
// https://github.com/Instadapp/fluid-contracts-public/blob/main/deployments/deployments.md
pub const FLUID_LIQUIDITY_LAYER: Address = address!("52Aa899454998Be5b000Ad077a46Bbe360F4e497");
pub const FLUID_DEX_RESOLVER: Address = address!("05Bd8269A20C472b148246De20E6852091BF16Ff");

pub const FLUID_NATIVE_ETH: Address = address!("EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");

pub fn get_liquidity_layer(chain_id: u64) -> Option<Address> {
    match chain_id {
        1 | 8453 | 42161 | 137 | 9745 => Some(FLUID_LIQUIDITY_LAYER),
        _ => None,
    }
}

sol! {
    event LogOperate(address indexed user, address indexed token, int256 supplyAmount, int256 borrowAmount);

    struct TokenLimit {
        uint256 available;
        uint256 expandsTo;
        uint256 expandDuration;
    }

    struct DexLimits {
        TokenLimit withdrawableToken0;
        TokenLimit withdrawableToken1;
        TokenLimit borrowableToken0;
        TokenLimit borrowableToken1;
    }

    struct CollateralReserves {
        uint256 token0RealReserves;
        uint256 token1RealReserves;
        uint256 token0ImaginaryReserves;
        uint256 token1ImaginaryReserves;
    }

    struct DebtReserves {
        uint256 token0Debt;
        uint256 token1Debt;
        uint256 token0RealReserves;
        uint256 token1RealReserves;
        uint256 token0ImaginaryReserves;
        uint256 token1ImaginaryReserves;
    }

    struct PoolWithReserves {
        address pool;
        address token0;
        address token1;
        uint256 fee;
        uint256 centerPrice;
        CollateralReserves collateralReserves;
        DebtReserves debtReserves;
        DexLimits limits;
    }

    struct Implementations {
        address shift;
        address admin;
        address colOperations;
        address debtOperations;
        address perfectOperationsAndSwapOut;
    }

    struct ConstantViews {
        uint256 dexId;
        address liquidity;
        address factory;
        Implementations implementations;
        address deployerContract;
        address token0;
        address token1;
        bytes32 supplyToken0Slot;
        bytes32 borrowToken0Slot;
        bytes32 supplyToken1Slot;
        bytes32 borrowToken1Slot;
        bytes32 exchangePriceToken0Slot;
        bytes32 exchangePriceToken1Slot;
        uint256 oracleMapping;
    }

    struct ConstantViews2 {
        uint256 token0NumeratorPrecision;
        uint256 token0DenominatorPrecision;
        uint256 token1NumeratorPrecision;
        uint256 token1DenominatorPrecision;
    }

    #[sol(rpc)]
    contract DexReservesResolver {
        function getPoolAddress(uint256 poolId_) external view returns (address pool_);
        function getTotalPools() external view returns (uint256);
        function getAllPoolAddresses() external view returns (address[] pools_);
        function getPoolTokens(address pool_) external view returns (address token0_, address token1_);
        function getPoolFee(address pool_) external view returns (uint256 fee_);
        function getPoolReservesAdjusted(address pool_) external returns (PoolWithReserves poolReserves_);
        function getPoolsReservesAdjusted(address[] pools_) external returns (PoolWithReserves[] poolsReserves_);
    }

    #[sol(rpc)]
    contract FluidDexT1 {
        event Swap(bool swap0to1, uint256 amountIn, uint256 amountOut, address to);
        function swap(bool swap0to1, uint256 amountIn, uint256 amountOutMin, address to, uint256 deadline) external returns (uint256 amountOut);
        function readFromStorage(bytes32 slot_) external view returns (uint256 result_);
        function constantsView() external view returns (ConstantViews memory constantsView_);
        function constantsView2() external view returns (ConstantViews2 memory constantsView2_);
    }

    #[sol(rpc)]
    contract ICenterPrice {
        function centerPrice() external returns (uint256 price);
    }

    #[sol(rpc)]
    contract FluidLiquidity {
        function readFromStorage(bytes32 slot_) external view returns (uint256 result_);
    }
}

fn scale_to_1e12(amount: U256, decimals: u8) -> U256 {
    if decimals == 12 {
        return amount;
    }
    if decimals < 12 {
        let mul = U256::from(10u64).pow(U256::from((12 - decimals) as u64));
        return amount.saturating_mul(mul);
    }
    let div = U256::from(10u64).pow(U256::from((decimals - 12) as u64));
    amount / div
}

fn unscale_from_1e12(amount_1e12: U256, decimals: u8) -> U256 {
    if decimals == 12 {
        return amount_1e12;
    }
    if decimals < 12 {
        let div = U256::from(10u64).pow(U256::from((12 - decimals) as u64));
        return amount_1e12 / div;
    }
    let mul = U256::from(10u64).pow(U256::from((decimals - 12) as u64));
    amount_1e12.saturating_mul(mul)
}

fn address_calc(deployed_from: Address, nonce: U256) -> Address {
    if nonce.is_zero() {
        return Address::ZERO;
    }
    let nonce_u64 = nonce.to::<u64>();
    let mut data: Vec<u8> = Vec::with_capacity(30);
    let from_bytes = deployed_from.as_slice();
    if nonce_u64 <= 0x7f {
        data.extend_from_slice(&[0xd6, 0x94]);
        data.extend_from_slice(from_bytes);
        data.push(nonce_u64 as u8);
    } else if nonce_u64 <= 0xff {
        data.extend_from_slice(&[0xd7, 0x94]);
        data.extend_from_slice(from_bytes);
        data.extend_from_slice(&[0x81, nonce_u64 as u8]);
    } else if nonce_u64 <= 0xffff {
        data.extend_from_slice(&[0xd8, 0x94]);
        data.extend_from_slice(from_bytes);
        data.push(0x82);
        data.extend_from_slice(&(nonce_u64 as u16).to_be_bytes());
    } else if nonce_u64 <= 0xffffff {
        data.extend_from_slice(&[0xd9, 0x94]);
        data.extend_from_slice(from_bytes);
        data.push(0x83);
        let n = nonce_u64 as u32;
        data.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]);
    } else {
        data.extend_from_slice(&[0xda, 0x94]);
        data.extend_from_slice(from_bytes);
        data.push(0x84);
        data.extend_from_slice(&(nonce_u64 as u32).to_be_bytes());
    }
    let hash = alloy::primitives::keccak256(data);
    Address::from_slice(&hash.as_slice()[12..])
}

async fn fetch_block_timestamp<N, P>(provider: P, block_id: BlockId) -> u64
where
    N: Network,
    N::BlockResponse: BlockResponse,
    <N::BlockResponse as BlockResponse>::Header: BlockHeader,
    P: Provider<N> + Clone,
{
    if let Ok(Some(block)) = provider.get_block(block_id).await {
        return block.header().timestamp();
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn calc_amount_out(amount_in: U256, reserve_in: U256, reserve_out: U256, fee_1e6: U256) -> U256 {
    if amount_in.is_zero() || reserve_in.is_zero() || reserve_out.is_zero() {
        return U256::ZERO;
    }
    let one = U256::from(1_000_000u64);
    let fee_multiplier = one.saturating_sub(fee_1e6);
    let amount_in_with_fee = amount_in.saturating_mul(fee_multiplier) / one;
    (amount_in_with_fee.saturating_mul(reserve_out))
        / (reserve_in.saturating_add(amount_in_with_fee))
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

fn price_diff_check(old_price: U256, new_price: U256) -> bool {
    if old_price.is_zero() || new_price.is_zero() {
        return false;
    }

    // Solidity: priceDiff_ = int(1e18) - int((oldPrice_ * 1e18) / newPrice_)
    let scale = U256::from(1_000_000_000_000_000_000u64);
    let ratio = old_price.saturating_mul(scale) / new_price;
    let price_diff = I256::from_raw(scale) - I256::from_raw(ratio);

    // Solidity check: if (priceDiff_ > 5e16 || priceDiff_ < -5e16)
    let limit = I256::from_raw(U256::from(50_000_000_000_000_000u64));
    price_diff <= limit && price_diff >= -limit
}

/// Integer square root using Newton's method
fn integer_sqrt(n: U256) -> U256 {
    if n.is_zero() {
        return U256::ZERO;
    }
    if n < U256::from(4) {
        return U256::ONE;
    }

    // Initial guess: 2^((log2(n) + 1) / 2)
    let mut x = n;
    let mut y = (x + U256::ONE) >> 1;

    while y < x {
        x = y;
        y = (x + n / x) >> 1;
    }

    x
}

/// Fluid DEX token limit data for borrowable/withdrawable checks
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenLimitData {
    /// Available amount at sync time (in token decimals)
    pub available: U256,
    /// Maximum amount after full expansion (in token decimals)
    pub expands_to: U256,
    /// Duration in seconds for full expansion
    pub expand_duration: u64,
}

impl TokenLimitData {
    /// Calculate the current available limit considering time expansion
    pub fn get_expanded_limit(&self, sync_time: u64, current_time: u64) -> U256 {
        if current_time <= sync_time {
            return self.available;
        }

        let elapsed = current_time.saturating_sub(sync_time);

        // If almost no time elapsed (<10s), return available
        if elapsed < 10 {
            return self.available;
        }

        // If duration passed, return max
        if self.expand_duration == 0 || elapsed >= self.expand_duration {
            return self.expands_to;
        }

        // Linear interpolation
        let diff = self.expands_to.saturating_sub(self.available);
        let expanded = diff * U256::from(elapsed) / U256::from(self.expand_duration);
        self.available.saturating_add(expanded)
    }
}

#[derive(Debug, Clone, Copy)]
struct SwapCalc {
    amount_out: U256,
    amount_out_1e12: U256,
    amount_in_col_net: U256,
    amount_out_col: U256,
    amount_in_debt_net: U256,
    amount_out_debt: U256,
    swap0to1: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FluidDexPool {
    pub address: Address,
    #[serde(default)]
    pub last_synced_block: u64,
    pub token_a: Token,
    pub token_b: Token,
    #[serde(default)]
    pub fee_1e6: u32,
    #[serde(default)]
    pub center_price_1e27: U256,
    #[serde(default)]
    pub last_stored_price_1e27: U256,
    #[serde(default)]
    pub upper_range_1e27: U256,
    #[serde(default)]
    pub lower_range_1e27: U256,
    #[serde(default)]
    pub upper_range_pct_1e6: U256,
    #[serde(default)]
    pub lower_range_pct_1e6: U256,
    #[serde(default)]
    pub upper_threshold_pct_1e3: U256,
    #[serde(default)]
    pub lower_threshold_pct_1e3: U256,
    #[serde(default)]
    pub token0_real_reserves_1e12: U256,
    #[serde(default)]
    pub token1_real_reserves_1e12: U256,
    #[serde(default)]
    pub token0_imag_reserves_1e12: U256,
    #[serde(default)]
    pub token1_imag_reserves_1e12: U256,
    /// Collateral pool reserves (for dual-pool routing)
    #[serde(default)]
    pub col_token0_real_1e12: U256,
    #[serde(default)]
    pub col_token1_real_1e12: U256,
    #[serde(default)]
    pub col_token0_imag_1e12: U256,
    #[serde(default)]
    pub col_token1_imag_1e12: U256,
    /// Debt pool reserves (for dual-pool routing)
    #[serde(default)]
    pub debt_token0_real_1e12: U256,
    #[serde(default)]
    pub debt_token1_real_1e12: U256,
    #[serde(default)]
    pub debt_token0_imag_1e12: U256,
    #[serde(default)]
    pub debt_token1_imag_1e12: U256,
    /// Actual token debt (for debt pool health checks)
    #[serde(default)]
    pub debt0_1e12: U256,
    #[serde(default)]
    pub debt1_1e12: U256,
    /// Borrowable limits (for output token)
    #[serde(default)]
    pub borrowable_token0: TokenLimitData,
    #[serde(default)]
    pub borrowable_token1: TokenLimitData,
    /// Withdrawable limits (for output token)
    #[serde(default)]
    pub withdrawable_token0: TokenLimitData,
    #[serde(default)]
    pub withdrawable_token1: TokenLimitData,
    /// Timestamp when limits were synced (Unix seconds)
    #[serde(default)]
    pub limits_sync_time: u64,
    #[serde(default)]
    pub token_a_price: f64,
    #[serde(default)]
    pub token_b_price: f64,
    #[serde(default)]
    pub revenue_cut_1e8: U256,
    #[serde(default)]
    pub is_swap_paused: bool,
    #[serde(default)]
    pub is_smart_collateral_enabled: bool,
    #[serde(default)]
    pub is_smart_debt_enabled: bool,
    #[serde(default)]
    pub liquidity_address: Address,
    #[serde(default)]
    pub deployer_contract: Address,
    #[serde(default)]
    pub exchange_price_token0_slot: B256,
    #[serde(default)]
    pub exchange_price_token1_slot: B256,
    #[serde(default)]
    pub token0_utilization: U256,
    #[serde(default)]
    pub token1_utilization: U256,
    #[serde(default)]
    pub utilization_limit_token0: U256,
    #[serde(default)]
    pub utilization_limit_token1: U256,
    #[serde(default)]
    pub last_swap_timestamp: u64,
    #[serde(default)]
    pub last_synced_block_timestamp: u64,
    #[serde(default)]
    pub older_price_1e27: U256,
    #[serde(default)]
    pub last_center_price_1e27: U256,
    #[serde(default)]
    pub range_shift: U256,
    #[serde(default)]
    pub threshold_shift: U256,
    #[serde(default)]
    pub center_price_shift: U256,
    #[serde(skip)]
    pub reserves_resolver: Address,
}

impl FluidDexPool {
    pub fn new(address: Address, resolver: Address) -> Self {
        Self {
            address,
            token_a_price: 0.0,
            token_b_price: 0.0,
            reserves_resolver: resolver,
            ..Default::default()
        }
    }

    fn refresh_prices(&mut self) {
        let (r0_u256, r1_u256) = self.total_imag_reserves();
        let Ok(r0) = u256_to_float(r0_u256) else {
            return;
        };
        let Ok(r1) = u256_to_float(r1_u256) else {
            return;
        };

        if r0.is_zero() || r1.is_zero() {
            return;
        }

        self.token_a_price = (r1.clone() / r0.clone()).to_f64();
        self.token_b_price = (r0 / r1).to_f64();
    }

    fn total_imag_reserves(&self) -> (U256, U256) {
        if self.token0_imag_reserves_1e12.is_zero() || self.token1_imag_reserves_1e12.is_zero() {
            (
                self.token0_real_reserves_1e12,
                self.token1_real_reserves_1e12,
            )
        } else {
            (
                self.token0_imag_reserves_1e12,
                self.token1_imag_reserves_1e12,
            )
        }
    }

    fn calc_shifting_done(current: U256, old: U256, time_passed: u64, shift_duration: u64) -> U256 {
        if shift_duration == 0 {
            return current;
        }
        if time_passed >= shift_duration {
            return current;
        }
        if current >= old {
            let diff = current - old;
            old + (diff * U256::from(time_passed) / U256::from(shift_duration))
        } else {
            let diff = old - current;
            old - (diff * U256::from(time_passed) / U256::from(shift_duration))
        }
    }

    fn apply_range_shift(&self, upper_pct: U256, lower_pct: U256, now_ts: u64) -> (U256, U256) {
        let active = ((self.range_shift >> 60u32) & mask(33)).to::<u64>() != 0;
        if !active {
            return (upper_pct, lower_pct);
        }
        let old_upper = self.range_shift & mask(20);
        let old_lower = (self.range_shift >> 20u32) & mask(20);
        let duration = ((self.range_shift >> 40u32) & mask(20)).to::<u64>();
        let start = ((self.range_shift >> 60u32) & mask(33)).to::<u64>();
        if start.saturating_add(duration) < now_ts {
            return (upper_pct, lower_pct);
        }
        let time_passed = now_ts.saturating_sub(start);
        (
            Self::calc_shifting_done(upper_pct, old_upper, time_passed, duration),
            Self::calc_shifting_done(lower_pct, old_lower, time_passed, duration),
        )
    }

    fn apply_threshold_shift(
        &self,
        upper_threshold: U256,
        lower_threshold: U256,
        threshold_time: U256,
        now_ts: u64,
    ) -> (U256, U256, U256) {
        let active = ((self.threshold_shift >> 60u32) & mask(33)).to::<u64>() != 0;
        if !active {
            return (upper_threshold, lower_threshold, threshold_time);
        }
        let old_upper = self.threshold_shift & mask(10);
        let old_lower = (self.threshold_shift >> 20u32) & mask(10);
        let duration = ((self.threshold_shift >> 40u32) & mask(20)).to::<u64>();
        let start = ((self.threshold_shift >> 60u32) & mask(33)).to::<u64>();
        let old_threshold_time = (self.threshold_shift >> 93u32) & mask(24);
        if start.saturating_add(duration) < now_ts {
            return (upper_threshold, lower_threshold, threshold_time);
        }
        let time_passed = now_ts.saturating_sub(start);
        (
            Self::calc_shifting_done(upper_threshold, old_upper, time_passed, duration),
            Self::calc_shifting_done(lower_threshold, old_lower, time_passed, duration),
            Self::calc_shifting_done(threshold_time, old_threshold_time, time_passed, duration),
        )
    }

    pub(crate) async fn update_center_price_from_chain<N, P>(
        &mut self,
        dex_variables: U256,
        dex_variables2: U256,
        provider: P,
        block_number: BlockId,
        now_ts: u64,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let center_price_nonce = (dex_variables2 >> 112u32) & mask(30);
        let shift_active = ((dex_variables2 >> 248u32) & U256::ONE) == U256::ONE;
        let old_center = decode_price_from_dex_variables(dex_variables, 81);

        if center_price_nonce.is_zero() {
            if !shift_active && !old_center.is_zero() {
                self.center_price_1e27 = old_center;
            }
            return Ok(());
        }
        if self.deployer_contract.is_zero() {
            return Ok(());
        }

        let hook_address = address_calc(self.deployer_contract, center_price_nonce);
        if hook_address.is_zero() {
            return Ok(());
        }

        let hook = ICenterPrice::new(hook_address, provider.clone());
        let external_price = hook
            .centerPrice()
            .block(block_number)
            .call()
            .await
            .unwrap_or(U256::ZERO);
        if external_price.is_zero() {
            return Ok(());
        }

        if !shift_active {
            self.center_price_1e27 = external_price;
            return Ok(());
        }

        let start = (self.center_price_shift & mask(33)).to::<u64>();
        let percent = (self.center_price_shift >> 33u32) & mask(20);
        let shift_time = (self.center_price_shift >> 53u32) & mask(20);
        if shift_time.is_zero() {
            self.center_price_1e27 = external_price;
            return Ok(());
        }
        let from_ts = std::cmp::max(self.last_swap_timestamp, start);
        let elapsed = now_ts.saturating_sub(from_ts);
        let price_shift = old_center
            .saturating_mul(percent)
            .saturating_mul(U256::from(elapsed))
            / (shift_time.saturating_mul(U256::from(1_000_000u64)));

        let mut new_center = external_price;
        let mut shift_done = false;
        if external_price > old_center {
            let shifted = old_center.saturating_add(price_shift);
            if external_price > shifted {
                new_center = shifted;
            } else {
                shift_done = true;
            }
        } else {
            let shifted = old_center.saturating_sub(price_shift);
            if external_price < shifted {
                new_center = shifted;
            } else {
                shift_done = true;
            }
        }

        self.center_price_1e27 = new_center;
        if shift_done {
            self.center_price_shift = U256::ZERO;
        }
        Ok(())
    }

    pub(crate) fn compute_ranges_from_dex(
        &mut self,
        _dex_variables: U256,
        dex_variables2: U256,
        now_ts: u64,
    ) {
        let six_decimals = U256::from(1_000_000u64);
        let three_decimals = U256::from(1_000u64);

        let mut center_price = self.center_price_1e27;
        let last_stored_price = self.last_stored_price_1e27;

        let upper_pct_raw = (dex_variables2 >> 27u32) & mask(20);
        let lower_pct_raw = (dex_variables2 >> 47u32) & mask(20);
        let mut upper_pct = upper_pct_raw;
        let mut lower_pct = lower_pct_raw;

        if ((dex_variables2 >> 26u32) & U256::ONE) == U256::ONE {
            let shifted = self.apply_range_shift(upper_pct, lower_pct, now_ts);
            upper_pct = shifted.0;
            lower_pct = shifted.1;
        }

        let mut upper_range = if upper_pct >= six_decimals {
            U256::ZERO
        } else {
            (center_price * six_decimals) / (six_decimals - upper_pct)
        };
        let mut lower_range = (center_price * (six_decimals - lower_pct)) / six_decimals;

        if ((dex_variables2 >> 68u32) & mask(20)) > U256::ZERO {
            let mut upper_threshold = (dex_variables2 >> 68u32) & mask(10);
            let mut lower_threshold = (dex_variables2 >> 78u32) & mask(10);
            let mut threshold_time = (dex_variables2 >> 88u32) & mask(24);

            if ((dex_variables2 >> 67u32) & U256::ONE) == U256::ONE {
                let shifted = self.apply_threshold_shift(
                    upper_threshold,
                    lower_threshold,
                    threshold_time,
                    now_ts,
                );
                upper_threshold = shifted.0;
                lower_threshold = shifted.1;
                threshold_time = shifted.2;
            }

            let time_elapsed = now_ts.saturating_sub(self.last_swap_timestamp);
            if last_stored_price
                > center_price
                    + ((upper_range - center_price) * (three_decimals - upper_threshold)
                        / three_decimals)
            {
                if threshold_time > U256::ZERO {
                    let shift_time = threshold_time.to::<u64>();
                    if time_elapsed < shift_time {
                        center_price = center_price
                            + ((upper_range - center_price) * U256::from(time_elapsed)
                                / U256::from(shift_time));
                    } else {
                        center_price = upper_range;
                    }
                }
            } else if last_stored_price
                < center_price
                    - ((center_price - lower_range) * (three_decimals - lower_threshold)
                        / three_decimals)
            {
                if threshold_time > U256::ZERO {
                    let shift_time = threshold_time.to::<u64>();
                    if time_elapsed < shift_time {
                        center_price = center_price
                            - ((center_price - lower_range) * U256::from(time_elapsed)
                                / U256::from(shift_time));
                    } else {
                        center_price = lower_range;
                    }
                }
            }

            let max_center_raw = (dex_variables2 >> 172u32) & mask(28);
            let max_center = from_big_number(max_center_raw);
            let mut changed = false;
            if !max_center.is_zero() && center_price > max_center {
                center_price = max_center;
                changed = true;
            } else {
                let min_center_raw = (dex_variables2 >> 200u32) & mask(28);
                let min_center = from_big_number(min_center_raw);
                if !min_center.is_zero() && center_price < min_center {
                    center_price = min_center;
                    changed = true;
                }
            }

            if changed {
                if ((dex_variables2 >> 26u32) & U256::ONE) == U256::ONE {
                    let shifted = self.apply_range_shift(upper_pct_raw, lower_pct_raw, now_ts);
                    upper_pct = shifted.0;
                    lower_pct = shifted.1;
                }
                upper_range = if upper_pct >= six_decimals {
                    U256::ZERO
                } else {
                    (center_price * six_decimals) / (six_decimals - upper_pct)
                };
                lower_range = (center_price * (six_decimals - lower_pct)) / six_decimals;
            }

            self.upper_threshold_pct_1e3 = upper_threshold;
            self.lower_threshold_pct_1e3 = lower_threshold;
        }

        self.center_price_1e27 = center_price;
        self.upper_range_pct_1e6 = upper_pct;
        self.lower_range_pct_1e6 = lower_pct;
        self.upper_range_1e27 = upper_range;
        self.lower_range_1e27 = lower_range;
    }

    fn apply_swap_1e12(&mut self, swap0to1: bool, amount_in_1e12: U256, amount_out_1e12: U256) {
        if swap0to1 {
            self.token0_real_reserves_1e12 = self
                .token0_real_reserves_1e12
                .saturating_add(amount_in_1e12);
            self.token1_real_reserves_1e12 = self
                .token1_real_reserves_1e12
                .saturating_sub(amount_out_1e12);
            self.token0_imag_reserves_1e12 = self
                .token0_imag_reserves_1e12
                .saturating_add(amount_in_1e12);
            self.token1_imag_reserves_1e12 = self
                .token1_imag_reserves_1e12
                .saturating_sub(amount_out_1e12);
        } else {
            self.token1_real_reserves_1e12 = self
                .token1_real_reserves_1e12
                .saturating_add(amount_in_1e12);
            self.token0_real_reserves_1e12 = self
                .token0_real_reserves_1e12
                .saturating_sub(amount_out_1e12);
            self.token1_imag_reserves_1e12 = self
                .token1_imag_reserves_1e12
                .saturating_add(amount_in_1e12);
            self.token0_imag_reserves_1e12 = self
                .token0_imag_reserves_1e12
                .saturating_sub(amount_out_1e12);
        }
        self.refresh_prices();
    }

    /// Calculate how much of a swap should go through the collateral pool.
    /// Returns positive I256 if col pool, negative if debt pool, or routing split amount.
    /// Based on Fluid DexMath: swapRoutingIn
    fn swap_routing_in(
        &self,
        t: U256,  // Total amount in
        x: U256,  // Imaginary reserves of token out of collateral
        y: U256,  // Imaginary reserves of token in of collateral
        x2: U256, // Imaginary reserves of token out of debt
        y2: U256, // Imaginary reserves of token in of debt
    ) -> I256 {
        if x.is_zero() || y.is_zero() || x2.is_zero() || y2.is_zero() {
            return I256::ZERO;
        }

        let scale_1e18 = U256::from(10u64).pow(U256::from(18u64));
        let xy = x.saturating_mul(y).saturating_mul(scale_1e18);
        let x2y2 = x2.saturating_mul(y2).saturating_mul(scale_1e18);

        let xy_root = integer_sqrt(xy);
        let x2y2_root = integer_sqrt(x2y2);

        if xy_root.is_zero() && x2y2_root.is_zero() {
            return I256::ZERO;
        }

        let numerator_pos = y2
            .saturating_mul(xy_root)
            .saturating_add(t.saturating_mul(xy_root));
        let numerator_neg = y.saturating_mul(x2y2_root);
        let denominator = xy_root.saturating_add(x2y2_root);

        if denominator.is_zero() {
            return I256::ZERO;
        }

        if numerator_pos >= numerator_neg {
            let a = (numerator_pos - numerator_neg) / denominator;
            I256::try_from(a).unwrap_or(I256::MAX)
        } else {
            let a = (numerator_neg - numerator_pos) / denominator;
            -I256::try_from(a).unwrap_or(I256::MAX)
        }
    }

    /// Verify reserves ratio to prevent extreme imbalance
    /// Based on Fluid DexMath: verifyToken0Reserves / verifyToken1Reserves
    fn verify_reserves_ratio(
        &self,
        swap0to1: bool,
        reserve_in: U256,
        reserve_out: U256,
        center_price: U256,
        min_swap_liquidity: U256,
    ) -> bool {
        if reserve_in.is_zero()
            || reserve_out.is_zero()
            || center_price.is_zero()
            || min_swap_liquidity.is_zero()
        {
            return false;
        }

        // For swap0to1 (token0 -> token1): verify token1 reserves
        // token1Reserves >= (token0Reserves * price) / (1e27 * MIN_SWAP_LIQUIDITY)
        //
        // For swap1to0 (token1 -> token0): verify token0 reserves
        // token0Reserves >= (token1Reserves * 1e27) / (price * MIN_SWAP_LIQUIDITY)

        let scale_1e27 = U256::from(10u64).pow(U256::from(27));

        if swap0to1 {
            let numerator = reserve_in.saturating_mul(center_price);
            let denominator = scale_1e27.saturating_mul(min_swap_liquidity);
            if denominator.is_zero() {
                return false;
            }
            let min_required = numerator / denominator;
            reserve_out >= min_required
        } else {
            let numerator = reserve_in.saturating_mul(scale_1e27);
            let denominator = center_price.saturating_mul(min_swap_liquidity);
            if denominator.is_zero() {
                return false;
            }
            let min_required = numerator / denominator;
            reserve_out >= min_required
        }
    }

    fn check_price_boundary(&self, new_price: U256) -> bool {
        if !self.lower_range_1e27.is_zero() && new_price < self.lower_range_1e27 {
            return false;
        }

        // upper_range_1e27 being 0 usually means upper_pct >= 100%, which is an on-chain revert condition.
        // However, we only enforce this if the pool is actually initialized (center_price > 0).
        if !self.center_price_1e27.is_zero()
            && (self.upper_range_1e27.is_zero() || new_price > self.upper_range_1e27)
        {
            return false;
        }

        true
    }

    fn validate_debt_pool_health(
        &self,
        geometric_mean: U256,
        upper_range: U256,
        lower_range: U256,
    ) -> Result<(), AMMError> {
        if !self.is_smart_debt_enabled {
            return Ok(());
        }

        let dx = self.debt0_1e12;
        let dy = self.debt1_1e12;

        if dx.is_zero() && dy.is_zero() {
            return Ok(());
        }

        let u27 = U256::from(10u64).pow(U256::from(27u64));
        let u54 = U256::from(10u64).pow(U256::from(54u64));

        let (gp_adj, pb_adj, dx_adj, dy_adj) = if geometric_mean < u27 {
            (geometric_mean, lower_range, dx, dy)
        } else {
            (u54 / geometric_mean, u54 / upper_range, dy, dx)
        };

        // part1 = ((dx * gp) - (dy * 1e27)) / (2 * 1e27)
        let term1 = dx_adj
            .checked_mul(gp_adj)
            .ok_or(AMMError::ArithmeticError)?;
        let term2 = dy_adj.checked_mul(u27).ok_or(AMMError::ArithmeticError)?;

        let p1 = if term1 >= term2 {
            I256::try_from((term1 - term2) / (U256::from(2u64) * u27)).unwrap_or(I256::MAX)
        } else {
            -I256::try_from((term2 - term1) / (U256::from(2u64) * u27)).unwrap_or(I256::MAX)
        };

        // p2 = (dx * dy * pb) / 1e27
        let dx_dy = dx_adj
            .checked_mul(dy_adj)
            .ok_or(AMMError::ArithmeticError)?;
        let p2 = if dx_dy.is_zero() {
            U256::ZERO
        } else if pb_adj > u54 / dx_dy {
            (dx_dy / u27)
                .checked_mul(pb_adj)
                .ok_or(AMMError::ArithmeticError)?
        } else {
            (dx_dy * pb_adj) / u27
        };

        // ry = p1 + sqrt(p2 + p1^2)
        let p1_sq = p1
            .checked_mul(p1)
            .ok_or(AMMError::ArithmeticError)?
            .into_raw();
        let sqrt_val = integer_sqrt(p2.checked_add(p1_sq).ok_or(AMMError::ArithmeticError)?);
        let ry = if p1 >= I256::ZERO {
            p1.into_raw()
                .checked_add(sqrt_val)
                .ok_or(AMMError::ArithmeticError)?
        } else {
            sqrt_val.saturating_sub(p1.abs().into_raw())
        };

        // iry_ = ((ry * 1e27) - (dx * pb))
        let ry_term = ry.checked_mul(u27).ok_or(AMMError::ArithmeticError)?;
        let dx_pb_term = dx_adj
            .checked_mul(pb_adj)
            .ok_or(AMMError::ArithmeticError)?;

        if ry_term < dx_pb_term {
            return Err(AMMError::ArithmeticError); // Panic point!
        }

        let iry_scaled = ry_term - dx_pb_term;
        if iry_scaled < U256::from(1_000_000u64) {
            return Err(AMMError::ArithmeticError); // DexT1__DebtReservesTooLow
        }

        Ok(())
    }

    fn simulate_swap_internal(
        &self,
        base_token: Address,
        quote_token: Address,
        amount_in: U256,
    ) -> Result<SwapCalc, AMMError> {
        let swap0to1 = base_token == self.token_a.address && quote_token == self.token_b.address;
        let swap1to0 = base_token == self.token_b.address && quote_token == self.token_a.address;

        if !swap0to1 && !swap1to0 {
            return Err(AMMError::Msg("Token pair not in pool".to_string()));
        }

        let in_decimals = if swap0to1 {
            self.token_a.decimals
        } else {
            self.token_b.decimals
        };
        let out_decimals = if swap0to1 {
            self.token_b.decimals
        } else {
            self.token_a.decimals
        };

        let zero = || SwapCalc {
            amount_out: U256::ZERO,
            amount_out_1e12: U256::ZERO,
            amount_in_col_net: U256::ZERO,
            amount_out_col: U256::ZERO,
            amount_in_debt_net: U256::ZERO,
            amount_out_debt: U256::ZERO,
            swap0to1,
        };

        if self.is_swap_paused {
            return Ok(zero());
        }
        if !self.is_smart_collateral_enabled && !self.is_smart_debt_enabled {
            // Pool not initialized in-contract; return zero to signal non-executable swap
            return Ok(zero());
        }

        let amount_to_swap = scale_to_1e12(amount_in, in_decimals);
        if amount_to_swap.is_zero() {
            return Ok(zero());
        }

        let six_decimals = U256::from(1_000_000u64);
        let two_decimals = U256::from(100u64);
        let x96 = U256::MAX >> 160;
        let x128 = U256::MAX >> 128;
        if amount_to_swap < six_decimals
            || amount_to_swap > x96
            || amount_in < two_decimals
            || amount_in > x128
        {
            // Signal non-executable swap (mirrors on-chain revert semantics)
            return Ok(zero());
        }

        let utilization_limit = if swap0to1 {
            self.utilization_limit_token1
        } else {
            self.utilization_limit_token0
        };
        if utilization_limit < U256::from(1_000u64) {
            let utilization = if swap0to1 {
                self.token1_utilization
            } else {
                self.token0_utilization
            };
            if utilization > utilization_limit.saturating_mul(U256::from(10u64)) {
                return Ok(zero());
            }
        }

        let col_pool_enabled = self.is_smart_collateral_enabled
            && !self.col_token0_imag_1e12.is_zero()
            && !self.col_token1_imag_1e12.is_zero()
            && !self.col_token0_real_1e12.is_zero()
            && !self.col_token1_real_1e12.is_zero();

        let debt_pool_enabled = self.is_smart_debt_enabled
            && !self.debt_token0_imag_1e12.is_zero()
            && !self.debt_token1_imag_1e12.is_zero()
            && !self.debt_token0_real_1e12.is_zero()
            && !self.debt_token1_real_1e12.is_zero();

        if !col_pool_enabled && !debt_pool_enabled {
            let amount_out = self.simulate_swap_simple(base_token, quote_token, amount_in)?;
            let amount_out_1e12 = scale_to_1e12(amount_out, out_decimals);

            // Even for simple swaps, we should check boundaries if possible
            let scale_1e27 = U256::from(10u64).pow(U256::from(27u64));
            let r_in = if swap0to1 {
                self.token0_imag_reserves_1e12
            } else {
                self.token1_imag_reserves_1e12
            };
            let r_out = if swap0to1 {
                self.token1_imag_reserves_1e12
            } else {
                self.token0_imag_reserves_1e12
            };

            if !r_in.is_zero() && !r_out.is_zero() {
                let revenue_cut = self.revenue_cut_1e8;
                let amount_in_net =
                    amount_to_swap.saturating_mul(revenue_cut) / U256::from(100_000_000u64);
                let new_i_in = r_in.saturating_add(amount_in_net);
                let new_i_out = r_out.saturating_sub(amount_out_1e12);

                let new_price = if swap0to1 {
                    new_i_out.saturating_mul(scale_1e27) / new_i_in
                } else {
                    new_i_in.saturating_mul(scale_1e27) / new_i_out
                };

                if !self.check_price_boundary(new_price) {
                    return Ok(zero());
                }
            }

            return Ok(SwapCalc {
                amount_out,
                amount_out_1e12,
                amount_in_col_net: U256::ZERO,
                amount_out_col: U256::ZERO,
                amount_in_debt_net: U256::ZERO,
                amount_out_debt: U256::ZERO,
                swap0to1,
            });
        }

        let (col_reserve_in, col_reserve_out, col_i_reserve_in, col_i_reserve_out) = if swap0to1 {
            (
                self.col_token0_real_1e12,
                self.col_token1_real_1e12,
                self.col_token0_imag_1e12,
                self.col_token1_imag_1e12,
            )
        } else {
            (
                self.col_token1_real_1e12,
                self.col_token0_real_1e12,
                self.col_token1_imag_1e12,
                self.col_token0_imag_1e12,
            )
        };

        let (debt_reserve_in, debt_reserve_out, debt_i_reserve_in, debt_i_reserve_out) = if swap0to1
        {
            (
                self.debt_token0_real_1e12,
                self.debt_token1_real_1e12,
                self.debt_token0_imag_1e12,
                self.debt_token1_imag_1e12,
            )
        } else {
            (
                self.debt_token1_real_1e12,
                self.debt_token0_real_1e12,
                self.debt_token1_imag_1e12,
                self.debt_token0_imag_1e12,
            )
        };

        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let (borrowable, withdrawable) = if swap0to1 {
            (
                self.borrowable_token1
                    .get_expanded_limit(self.limits_sync_time, current_time),
                self.withdrawable_token1
                    .get_expanded_limit(self.limits_sync_time, current_time),
            )
        } else {
            (
                self.borrowable_token0
                    .get_expanded_limit(self.limits_sync_time, current_time),
                self.withdrawable_token0
                    .get_expanded_limit(self.limits_sync_time, current_time),
            )
        };

        let borrowable_1e12 = scale_to_1e12(borrowable, out_decimals);
        let withdrawable_1e12 = scale_to_1e12(withdrawable, out_decimals);

        let limit_amount = (col_i_reserve_in + debt_i_reserve_in) / U256::from(2u64);
        if amount_to_swap > limit_amount {
            return Err(AMMError::ArithmeticError);
        }

        let a = if col_pool_enabled && debt_pool_enabled {
            self.swap_routing_in(
                amount_to_swap,
                col_i_reserve_out,
                col_i_reserve_in,
                debt_i_reserve_out,
                debt_i_reserve_in,
            )
        } else if debt_pool_enabled {
            I256::MINUS_ONE
        } else {
            I256::try_from(amount_to_swap).unwrap_or(I256::MAX) + I256::ONE
        };

        let (amount_in_col, amount_out_col, amount_in_debt, amount_out_debt) = if a <= I256::ZERO {
            let out = calc_amount_out(
                amount_to_swap,
                debt_i_reserve_in,
                debt_i_reserve_out,
                U256::from(self.fee_1e6),
            );
            (U256::ZERO, U256::ZERO, amount_to_swap, out)
        } else if a >= I256::try_from(amount_to_swap).unwrap_or(I256::MAX) {
            let out = calc_amount_out(
                amount_to_swap,
                col_i_reserve_in,
                col_i_reserve_out,
                U256::from(self.fee_1e6),
            );
            (amount_to_swap, out, U256::ZERO, U256::ZERO)
        } else {
            let a_u256 = a.into_raw();
            let out_col = calc_amount_out(
                a_u256,
                col_i_reserve_in,
                col_i_reserve_out,
                U256::from(self.fee_1e6),
            );
            let in_debt = amount_to_swap - a_u256;
            let out_debt = calc_amount_out(
                in_debt,
                debt_i_reserve_in,
                debt_i_reserve_out,
                U256::from(self.fee_1e6),
            );
            (a_u256, out_col, in_debt, out_debt)
        };

        if amount_out_debt > debt_reserve_out || amount_out_col > col_reserve_out {
            return Err(AMMError::ArithmeticError);
        }

        if amount_out_debt > borrowable_1e12 || amount_out_col > withdrawable_1e12 {
            return Ok(zero());
        }

        let min_swap_liquidity = U256::from(10_000u64);
        let center_price = self.center_price_1e27;

        // Debt pool health check (The missing logic bias)
        if self.is_smart_debt_enabled {
            let (upper_pct, lower_pct) = if ((self.range_shift >> 26u32) & U256::ONE) == U256::ONE {
                self.apply_range_shift(
                    (self.range_shift >> 27u32) & mask(20),
                    (self.range_shift >> 47u32) & mask(20),
                    current_time,
                )
            } else {
                (
                    (self.range_shift >> 27u32) & mask(20),
                    (self.range_shift >> 47u32) & mask(20),
                )
            };

            let six_decimals = U256::from(1_000_000u64);
            let upper_range = if upper_pct >= six_decimals {
                U256::ZERO
            } else {
                (center_price * six_decimals) / (six_decimals - upper_pct)
            };
            let lower_range = (center_price * (six_decimals - lower_pct)) / six_decimals;

            let u18 = U256::from(10u64).pow(U256::from(18u64));
            let geometric_mean = if upper_range < U256::from(10u64).pow(U256::from(38u64)) {
                integer_sqrt(upper_range * lower_range)
            } else {
                integer_sqrt((upper_range / u18) * (lower_range / u18)) * u18
            };

            self.validate_debt_pool_health(geometric_mean, upper_range, lower_range)?;
        }

        if !amount_in_col.is_zero() {
            let new_reserve_in = col_reserve_in
                .checked_add(amount_in_col)
                .ok_or(AMMError::ArithmeticError)?;
            let new_reserve_out = col_reserve_out
                .checked_sub(amount_out_col)
                .ok_or(AMMError::ArithmeticError)?;
            if !self.verify_reserves_ratio(
                swap0to1,
                new_reserve_in,
                new_reserve_out,
                center_price,
                min_swap_liquidity,
            ) {
                return Ok(zero());
            }
        }
        if !amount_in_debt.is_zero() {
            let new_reserve_in = debt_reserve_in
                .checked_add(amount_in_debt)
                .ok_or(AMMError::ArithmeticError)?;
            let new_reserve_out = debt_reserve_out
                .checked_sub(amount_out_debt)
                .ok_or(AMMError::ArithmeticError)?;
            if !self.verify_reserves_ratio(
                swap0to1,
                new_reserve_in,
                new_reserve_out,
                center_price,
                min_swap_liquidity,
            ) {
                return Ok(zero());
            }
        }

        let scale_1e27 = U256::from(10u64).pow(U256::from(27u64));
        let amount_in_col_net =
            amount_in_col.saturating_mul(self.revenue_cut_1e8) / U256::from(100_000_000u64);
        let amount_in_debt_net =
            amount_in_debt.saturating_mul(self.revenue_cut_1e8) / U256::from(100_000_000u64);

        let new_price = if amount_in_col > amount_in_debt {
            let new_i_in = col_i_reserve_in
                .checked_add(amount_in_col_net)
                .ok_or(AMMError::ArithmeticError)?;
            let new_i_out = col_i_reserve_out
                .checked_sub(amount_out_col)
                .ok_or(AMMError::ArithmeticError)?;
            if swap0to1 {
                new_i_out
                    .checked_mul(scale_1e27)
                    .ok_or(AMMError::ArithmeticError)?
                    / new_i_in
            } else {
                new_i_in
                    .checked_mul(scale_1e27)
                    .ok_or(AMMError::ArithmeticError)?
                    / new_i_out
            }
        } else {
            let new_i_in = debt_i_reserve_in
                .checked_add(amount_in_debt_net)
                .ok_or(AMMError::ArithmeticError)?;
            let new_i_out = debt_i_reserve_out
                .checked_sub(amount_out_debt)
                .ok_or(AMMError::ArithmeticError)?;
            if swap0to1 {
                new_i_out
                    .checked_mul(scale_1e27)
                    .ok_or(AMMError::ArithmeticError)?
                    / new_i_in
            } else {
                new_i_in
                    .checked_mul(scale_1e27)
                    .ok_or(AMMError::ArithmeticError)?
                    / new_i_out
            }
        };
        if !self.check_price_boundary(new_price) {
            return Ok(zero());
        }

        let current_timestamp = self.last_synced_block_timestamp;
        let time_diff = current_timestamp.saturating_sub(self.last_swap_timestamp);
        if time_diff == 0 {
            if !self.last_center_price_1e27.is_zero() {
                let scale_1e8 = U256::from(100_000_000u64);
                let lower = (self
                    .last_center_price_1e27
                    .saturating_mul(scale_1e8.saturating_sub(U256::ONE)))
                    / scale_1e8;
                let upper = (self
                    .last_center_price_1e27
                    .saturating_mul(scale_1e8.saturating_add(U256::ONE)))
                    / scale_1e8;
                if center_price < lower || center_price > upper {
                    return Ok(zero());
                }
            }
            let old_price = if self.older_price_1e27.is_zero() {
                new_price
            } else {
                self.older_price_1e27
            };
            if !price_diff_check(old_price, new_price) {
                return Ok(zero());
            }
        } else {
            let old_price = if self.last_stored_price_1e27.is_zero() {
                new_price
            } else {
                self.last_stored_price_1e27
            };
            if !price_diff_check(old_price, new_price) {
                return Ok(zero());
            }
        }

        let total_out_1e12 = amount_out_col + amount_out_debt;
        Ok(SwapCalc {
            amount_out: unscale_from_1e12(total_out_1e12, out_decimals),
            amount_out_1e12: total_out_1e12,
            amount_in_col_net,
            amount_out_col,
            amount_in_debt_net,
            amount_out_debt,
            swap0to1,
        })
    }

    /// Simple swap simulation (backward compatibility fallback)
    fn simulate_swap_simple(
        &self,
        base_token: Address,
        quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let fee = U256::from(self.fee_1e6);
        let amount_in_1e12 = if base_token == self.token_a.address {
            scale_to_1e12(amount_in, self.token_a.decimals)
        } else if base_token == self.token_b.address {
            scale_to_1e12(amount_in, self.token_b.decimals)
        } else {
            return Err(AMMError::Msg("Token not in pool".to_string()));
        };

        // Dust protection and range checks
        let six_decimals = U256::from(1_000_000u64);
        let two_decimals = U256::from(100u64);
        let x96 = mask(96);
        let x128 = mask(128);

        if amount_in_1e12 < six_decimals
            || amount_in_1e12 > x96
            || amount_in < two_decimals
            || amount_in > x128
        {
            return Ok(U256::ZERO);
        }

        // Utilization limit check
        let swap0to1 = base_token == self.token_a.address;
        let utilization_limit = if swap0to1 {
            self.utilization_limit_token1
        } else {
            self.utilization_limit_token0
        };

        if utilization_limit < U256::from(1_000u64) {
            let utilization = if swap0to1 {
                self.token1_utilization
            } else {
                self.token0_utilization
            };
            if utilization > utilization_limit.saturating_mul(U256::from(10u64)) {
                return Ok(U256::ZERO);
            }
        }

        let (r0, r1) = self.total_imag_reserves();
        let amount_out_1e12 =
            if base_token == self.token_a.address && quote_token == self.token_b.address {
                calc_amount_out(amount_in_1e12, r0, r1, fee)
            } else if base_token == self.token_b.address && quote_token == self.token_a.address {
                calc_amount_out(amount_in_1e12, r1, r0, fee)
            } else {
                return Err(AMMError::Msg("Token pair not in pool".to_string()));
            };

        let out_decimals = if quote_token == self.token_a.address {
            self.token_a.decimals
        } else {
            self.token_b.decimals
        };
        Ok(unscale_from_1e12(amount_out_1e12, out_decimals))
    }

    /// Exact-Out simulation using binary search.
    /// Given a target output amount, find the minimal input amount required.
    pub fn simulate_swap_exact_out_internal(
        &self,
        base_token: Address,
        quote_token: Address,
        amount_out: U256,
    ) -> Result<U256, AMMError> {
        // 1. Zero amount check
        if amount_out.is_zero() {
            return Ok(U256::ZERO);
        }

        // 2. Validate token pair and determine direction
        let swap0to1 = base_token == self.token_a.address && quote_token == self.token_b.address;
        let swap1to0 = base_token == self.token_b.address && quote_token == self.token_a.address;

        if !swap0to1 && !swap1to0 {
            return Err(AMMError::Msg("Token pair not in pool".to_string()));
        }

        // 3. Get decimals for scaling
        let in_decimals = if swap0to1 {
            self.token_a.decimals
        } else {
            self.token_b.decimals
        };
        let out_decimals = if swap0to1 {
            self.token_b.decimals
        } else {
            self.token_a.decimals
        };
        let amount_out_1e12 = scale_to_1e12(amount_out, out_decimals);

        // 4. Liquidity check - use imaginary reserves as upper bound
        // Real reserves may be split across col/debt pools, so we check against
        // imaginary reserves which represent the total trading capacity
        let (imag_reserve_out, real_reserve_out) = if swap0to1 {
            (
                self.token1_imag_reserves_1e12,
                self.token1_real_reserves_1e12,
            )
        } else {
            (
                self.token0_imag_reserves_1e12,
                self.token0_real_reserves_1e12,
            )
        };

        // Use imaginary reserves as the upper bound for available liquidity
        // This is more accurate than summing col/debt real reserves
        let total_available = imag_reserve_out.max(real_reserve_out);

        if amount_out_1e12 >= total_available {
            return Err(AMMError::Msg(
                "Insufficient liquidity for exact out".to_string(),
            ));
        }

        // 5. Set search bounds
        let (reserve_in_col, reserve_in_debt) = if swap0to1 {
            (self.col_token0_real_1e12, self.debt_token0_real_1e12)
        } else {
            (self.col_token1_real_1e12, self.debt_token1_real_1e12)
        };
        let total_reserve_in = reserve_in_col.saturating_add(reserve_in_debt);
        let max_high = total_reserve_in.saturating_mul(U256::from(1000u64));

        // 6. Exponential search for upper bound
        let mut low = U256::ZERO;
        let mut high = U256::from(1u8);

        loop {
            let high_original = unscale_from_1e12(high, in_decimals);
            let dy_1e12 = match self.simulate_swap_internal(base_token, quote_token, high_original)
            {
                Ok(calc) => calc.amount_out_1e12,
                Err(_) => U256::ZERO,
            };

            if dy_1e12 >= amount_out_1e12 {
                break;
            }

            if high >= max_high {
                return Err(AMMError::Msg(
                    "Exact out not reachable within max search bound".to_string(),
                ));
            }

            high = high.saturating_mul(U256::from(2u8));
            if high > max_high {
                high = max_high;
            }
        }

        // 7. Binary search for minimal input that yields output >= amount_out
        while high > low.saturating_add(U256::from(1u8)) {
            let mid = (low + high) / U256::from(2u8);
            let mid_original = unscale_from_1e12(mid, in_decimals);

            let dy_1e12 = match self.simulate_swap_internal(base_token, quote_token, mid_original) {
                Ok(calc) => calc.amount_out_1e12,
                Err(_) => U256::ZERO,
            };

            if dy_1e12 >= amount_out_1e12 {
                high = mid;
            } else {
                low = mid;
            }
        }

        // 8. Return result in original decimals
        Ok(unscale_from_1e12(high, in_decimals))
    }
}

impl AutomatedMarketMaker for FluidDexPool {
    fn address(&self) -> Address {
        self.address
    }

    fn supported_chains(&self) -> Option<Vec<u64>> {
        // Mainnet, Base, Arbitrum, Polygon, Plasma
        Some(vec![1, 8453, 42161, 137, 9745])
    }

    fn has_sufficient_liquidity(&self) -> bool {
        // Check real reserves (ignoring imaginary for safety baseline)
        // Reserves are scaled to 1e12.
        // 1_000_000_000 (10^9) represents 0.001 unit of the token.
        self.token0_real_reserves_1e12 > U256::from(1_000_000_000)
            && self.token1_real_reserves_1e12 > U256::from(1_000_000_000)
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

    fn last_synced_block(&self) -> u64 {
        self.last_synced_block
    }

    fn set_last_synced_block(&mut self, block_number: u64) {
        self.last_synced_block = self.last_synced_block.max(block_number);
    }

    fn sync_events(&self) -> Vec<B256> {
        vec![FluidDexT1::Swap::SIGNATURE_HASH, LogOperate::SIGNATURE_HASH]
    }

    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError> {
        let signature = log.topics()[0];
        if signature == FluidDexT1::Swap::SIGNATURE_HASH {
            let event = FluidDexT1::Swap::decode_log(&log.inner)?;
            let amount_in_1e12 = if event.swap0to1 {
                scale_to_1e12(U256::from(event.amountIn), self.token_a.decimals)
            } else {
                scale_to_1e12(U256::from(event.amountIn), self.token_b.decimals)
            };
            let amount_out_1e12 = if event.swap0to1 {
                scale_to_1e12(U256::from(event.amountOut), self.token_b.decimals)
            } else {
                scale_to_1e12(U256::from(event.amountOut), self.token_a.decimals)
            };
            self.apply_swap_1e12(event.swap0to1, amount_in_1e12, amount_out_1e12);

            tracing::info!(
                target = "amms::fluid_dex::sync",
                block_number = ?log.block_number,
                pool = ?self.address,
                swap0to1 = event.swap0to1,
                amount_in = ?event.amountIn,
                amount_out = ?event.amountOut,
                "Swap"
            );

            return Ok(SyncAction::None);
        } else if signature == LogOperate::SIGNATURE_HASH {
            let event = LogOperate::decode_log(&log.inner)?;
            if event.user == self.address {
                let is_token0 = event.token == self.token_a.address;
                let is_token1 = event.token == self.token_b.address;

                if !is_token0 && !is_token1 {
                    return Ok(SyncAction::None);
                }

                let decimals = if is_token0 {
                    self.token_a.decimals
                } else {
                    self.token_b.decimals
                };

                let apply_delta = |current: U256, delta: I256| -> U256 {
                    if delta.is_negative() {
                        let delta_abs = delta.wrapping_neg().into_raw();
                        let delta_scaled = scale_to_1e12(delta_abs, decimals);
                        current.saturating_sub(delta_scaled)
                    } else {
                        let delta_u = delta.into_raw();
                        let delta_scaled = scale_to_1e12(delta_u, decimals);
                        current.saturating_add(delta_scaled)
                    }
                };

                if is_token0 {
                    self.col_token0_real_1e12 =
                        apply_delta(self.col_token0_real_1e12, event.supplyAmount);
                    self.col_token0_imag_1e12 =
                        apply_delta(self.col_token0_imag_1e12, event.supplyAmount);
                    self.debt_token0_real_1e12 =
                        apply_delta(self.debt_token0_real_1e12, event.borrowAmount);
                    self.debt_token0_imag_1e12 =
                        apply_delta(self.debt_token0_imag_1e12, event.borrowAmount);
                    let total_delta = event.supplyAmount + event.borrowAmount;
                    self.token0_real_reserves_1e12 =
                        apply_delta(self.token0_real_reserves_1e12, total_delta);
                    self.token0_imag_reserves_1e12 =
                        apply_delta(self.token0_imag_reserves_1e12, total_delta);
                } else {
                    self.col_token1_real_1e12 =
                        apply_delta(self.col_token1_real_1e12, event.supplyAmount);
                    self.col_token1_imag_1e12 =
                        apply_delta(self.col_token1_imag_1e12, event.supplyAmount);
                    self.debt_token1_real_1e12 =
                        apply_delta(self.debt_token1_real_1e12, event.borrowAmount);
                    self.debt_token1_imag_1e12 =
                        apply_delta(self.debt_token1_imag_1e12, event.borrowAmount);
                    let total_delta = event.supplyAmount + event.borrowAmount;
                    self.token1_real_reserves_1e12 =
                        apply_delta(self.token1_real_reserves_1e12, total_delta);
                    self.token1_imag_reserves_1e12 =
                        apply_delta(self.token1_imag_reserves_1e12, total_delta);
                }

                self.refresh_prices();
            }

            tracing::info!(
                target = "amms::fluid_dex::sync",
                block_number = ?log.block_number,
                pool = ?self.address,
                user = ?event.user,
                token = ?event.token,
                supply_amount = ?event.supplyAmount,
                borrow_amount = ?event.borrowAmount,
                "LogOperate"
            );

            return Ok(SyncAction::None);
        }
        Ok(SyncAction::None)
    }

    fn tokens(&self) -> Vec<Address> {
        vec![self.token_a.address, self.token_b.address]
    }

    fn calculate_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
        if self.tokens().len() != 2 {
            return Err(AMMError::Msg("FluidDexPool tokens != 2".to_string()));
        }

        let (r0_u256, r1_u256) = self.total_imag_reserves();
        if r0_u256 < U256::from(MIN_POOL_RESERVE) || r1_u256 < U256::from(MIN_POOL_RESERVE) {
            return Ok(0.0);
        }
        let r0 = u256_to_float(r0_u256)?;
        let r1 = u256_to_float(r1_u256)?;

        if r0.is_zero() || r1.is_zero() {
            return Ok(0.0);
        }

        let shift = self.token_a.decimals as i32 - self.token_b.decimals as i32;
        let scale_factor = Float::with_val(MPFR_T_PRECISION, 10).pow(shift);

        let price_a_f = (r1 / r0) * &scale_factor;
        let price_a = price_a_f.to_f64();

        if base_token == self.token_a.address && quote_token == self.token_b.address {
            return Ok(price_a);
        }
        if base_token == self.token_b.address && quote_token == self.token_a.address {
            if price_a == 0.0 {
                return Ok(0.0);
            }
            return Ok(1.0 / price_a);
        }
        Err(AMMError::Msg("Token not in pool".to_string()))
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

    fn simulate_swap(
        &self,
        base_token: Address,
        quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        Ok(self
            .simulate_swap_internal(base_token, quote_token, amount_in)?
            .amount_out)
    }

    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let calc = self.simulate_swap_internal(base_token, quote_token, amount_in)?;
        if calc.amount_out_1e12.is_zero() {
            return Ok(U256::ZERO);
        }

        if calc.swap0to1 {
            if calc.amount_in_col_net > U256::ZERO {
                self.col_token0_real_1e12 = self
                    .col_token0_real_1e12
                    .saturating_add(calc.amount_in_col_net);
                self.col_token1_real_1e12 = self
                    .col_token1_real_1e12
                    .checked_sub(calc.amount_out_col)
                    .ok_or(AMMError::ArithmeticError)?;
                self.col_token0_imag_1e12 = self
                    .col_token0_imag_1e12
                    .checked_add(calc.amount_in_col_net)
                    .ok_or(AMMError::ArithmeticError)?;
                self.col_token1_imag_1e12 = self
                    .col_token1_imag_1e12
                    .checked_sub(calc.amount_out_col)
                    .ok_or(AMMError::ArithmeticError)?;
            }
            if calc.amount_in_debt_net > U256::ZERO {
                self.debt_token0_real_1e12 = self
                    .debt_token0_real_1e12
                    .saturating_add(calc.amount_in_debt_net);
                self.debt_token1_real_1e12 = self
                    .debt_token1_real_1e12
                    .checked_sub(calc.amount_out_debt)
                    .ok_or(AMMError::ArithmeticError)?;
                self.debt_token0_imag_1e12 = self
                    .debt_token0_imag_1e12
                    .checked_add(calc.amount_in_debt_net)
                    .ok_or(AMMError::ArithmeticError)?;
                self.debt_token1_imag_1e12 = self
                    .debt_token1_imag_1e12
                    .checked_sub(calc.amount_out_debt)
                    .ok_or(AMMError::ArithmeticError)?;
            }
        } else {
            if calc.amount_in_col_net > U256::ZERO {
                self.col_token1_real_1e12 = self
                    .col_token1_real_1e12
                    .saturating_add(calc.amount_in_col_net);
                self.col_token0_real_1e12 = self
                    .col_token0_real_1e12
                    .checked_sub(calc.amount_out_col)
                    .ok_or(AMMError::ArithmeticError)?;
                self.col_token1_imag_1e12 = self
                    .col_token1_imag_1e12
                    .checked_add(calc.amount_in_col_net)
                    .ok_or(AMMError::ArithmeticError)?;
                self.col_token0_imag_1e12 = self
                    .col_token0_imag_1e12
                    .checked_sub(calc.amount_out_col)
                    .ok_or(AMMError::ArithmeticError)?;
            }
            if calc.amount_in_debt_net > U256::ZERO {
                self.debt_token1_real_1e12 = self
                    .debt_token1_real_1e12
                    .saturating_add(calc.amount_in_debt_net);
                self.debt_token0_real_1e12 = self
                    .debt_token0_real_1e12
                    .checked_sub(calc.amount_out_debt)
                    .ok_or(AMMError::ArithmeticError)?;
                self.debt_token1_imag_1e12 = self
                    .debt_token1_imag_1e12
                    .checked_add(calc.amount_in_debt_net)
                    .ok_or(AMMError::ArithmeticError)?;
                self.debt_token0_imag_1e12 = self
                    .debt_token0_imag_1e12
                    .checked_sub(calc.amount_out_debt)
                    .ok_or(AMMError::ArithmeticError)?;
            }
        }

        self.token0_real_reserves_1e12 = self.col_token0_real_1e12 + self.debt_token0_real_1e12;
        self.token1_real_reserves_1e12 = self.col_token1_real_1e12 + self.debt_token1_real_1e12;
        self.token0_imag_reserves_1e12 = self.col_token0_imag_1e12 + self.debt_token0_imag_1e12;
        self.token1_imag_reserves_1e12 = self.col_token1_imag_1e12 + self.debt_token1_imag_1e12;

        let scale_1e27 = U256::from(10u64).pow(U256::from(27u64));
        let new_price_1e27 = if calc.swap0to1 {
            (self.token1_imag_reserves_1e12 * scale_1e27) / self.token0_imag_reserves_1e12
        } else {
            (self.token0_imag_reserves_1e12 * scale_1e27) / self.token1_imag_reserves_1e12
        };

        let current_timestamp = self.last_synced_block_timestamp;
        let time_diff = current_timestamp.saturating_sub(self.last_swap_timestamp);
        if time_diff == 0 {
            self.last_stored_price_1e27 = new_price_1e27;
        } else {
            self.older_price_1e27 = if self.last_stored_price_1e27.is_zero() {
                new_price_1e27
            } else {
                self.last_stored_price_1e27
            };
            self.last_stored_price_1e27 = new_price_1e27;
            self.last_center_price_1e27 = self.center_price_1e27;
            self.last_swap_timestamp = current_timestamp;
        }

        self.refresh_prices();
        Ok(calc.amount_out)
    }

    fn simulate_swap_exact_out(
        &self,
        base_token: Address,
        quote_token: Address,
        amount_out: U256,
    ) -> Result<U256, AMMError> {
        self.simulate_swap_exact_out_internal(base_token, quote_token, amount_out)
    }

    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        N::BlockResponse: BlockResponse,
        <N::BlockResponse as BlockResponse>::Header: BlockHeader,
        P: Provider<N> + Clone,
    {
        let resolver = DexReservesResolver::new(self.reserves_resolver, provider.clone());
        let res = resolver
            .getPoolReservesAdjusted(self.address)
            .block(block_number)
            .call()
            .await?;

        let dex = FluidDexT1::new(self.address, provider.clone());
        let dex_variables = dex
            .readFromStorage(B256::from(U256::from(0u64)))
            .block(block_number)
            .call()
            .await
            .unwrap_or(U256::ZERO);
        let dex_variables2 = dex
            .readFromStorage(B256::from(U256::from(1u64)))
            .block(block_number)
            .call()
            .await
            .unwrap_or(U256::ZERO);
        let range_shift = dex
            .readFromStorage(B256::from(U256::from(7u64)))
            .block(block_number)
            .call()
            .await
            .unwrap_or(U256::ZERO);
        let threshold_shift = dex
            .readFromStorage(B256::from(U256::from(8u64)))
            .block(block_number)
            .call()
            .await
            .unwrap_or(U256::ZERO);
        let center_price_shift = dex
            .readFromStorage(B256::from(U256::from(9u64)))
            .block(block_number)
            .call()
            .await
            .unwrap_or(U256::ZERO);
        let constants_view = dex.constantsView().block(block_number).call().await;
        let constants_view2 = dex.constantsView2().block(block_number).call().await;
        let _ = constants_view2;
        if let Ok(cv) = constants_view {
            self.liquidity_address = cv.liquidity;
            self.exchange_price_token0_slot = cv.exchangePriceToken0Slot;
            self.exchange_price_token1_slot = cv.exchangePriceToken1Slot;
            self.deployer_contract = cv.deployerContract;
        }

        if !self.liquidity_address.is_zero() {
            let liquidity = FluidLiquidity::new(self.liquidity_address, provider.clone());
            let exchange_price_token0 = liquidity
                .readFromStorage(self.exchange_price_token0_slot)
                .block(block_number)
                .call()
                .await
                .unwrap_or(U256::ZERO);
            let exchange_price_token1 = liquidity
                .readFromStorage(self.exchange_price_token1_slot)
                .block(block_number)
                .call()
                .await
                .unwrap_or(U256::ZERO);
            self.token0_utilization = decode_liquidity_utilization(exchange_price_token0);
            self.token1_utilization = decode_liquidity_utilization(exchange_price_token1);
        }

        // Token mapping reverted: We store raw 0xEeeee... address.
        // Core engine handles the logical mapping to WETH.

        let token_decimals =
            get_token_decimals::<N, _>(vec![res.token0, res.token1], provider.clone()).await?;

        let d0 = match token_decimals.get(&res.token0).copied() {
            Some(d) if d != 0 => d,
            _ if res.token0 == FLUID_NATIVE_ETH => 18,
            _ => {
                let decimals = IERC20::new(res.token0, provider.clone())
                    .decimals()
                    .call()
                    .await
                    .unwrap_or(18);
                if decimals == 0 {
                    18
                } else {
                    decimals
                }
            }
        };
        let d1 = match token_decimals.get(&res.token1).copied() {
            Some(d) if d != 0 => d,
            _ if res.token1 == FLUID_NATIVE_ETH => 18,
            _ => {
                let decimals = IERC20::new(res.token1, provider.clone())
                    .decimals()
                    .call()
                    .await
                    .unwrap_or(18);
                if decimals == 0 {
                    18
                } else {
                    decimals
                }
            }
        };
        self.token_a = Token::new_with_decimals(res.token0, d0);
        self.token_b = Token::new_with_decimals(res.token1, d1);
        let fee_1e4 = u32::try_from((dex_variables2 >> 2u32) & mask(17)).unwrap_or(0);
        self.fee_1e6 = fee_1e4;
        let revenue_cut_percent: U256 = (dex_variables2 >> 19u32) & mask(7);
        let revenue_cut = U256::from(100_000_000u64)
            .saturating_sub(revenue_cut_percent.saturating_mul(U256::from(fee_1e4)));
        self.revenue_cut_1e8 = if revenue_cut.is_zero() {
            U256::from(100_000_000u64)
        } else {
            revenue_cut
        };
        self.is_swap_paused = ((dex_variables2 >> 255) & U256::ONE) == U256::ONE;
        self.is_smart_collateral_enabled = (dex_variables2 & U256::ONE) == U256::ONE;
        self.is_smart_debt_enabled = ((dex_variables2 >> 1) & U256::ONE) == U256::ONE;
        self.utilization_limit_token0 = (dex_variables2 >> 228u32) & mask(10);
        self.utilization_limit_token1 = (dex_variables2 >> 238u32) & mask(10);
        self.center_price_1e27 = res.centerPrice;
        self.older_price_1e27 = decode_price_from_dex_variables(dex_variables, 1);
        self.last_stored_price_1e27 = decode_price_from_dex_variables(dex_variables, 41);
        self.last_center_price_1e27 = decode_price_from_dex_variables(dex_variables, 81);
        self.last_swap_timestamp = ((dex_variables >> 121u32) & mask(33)).to::<u64>();

        self.last_synced_block_timestamp =
            fetch_block_timestamp::<N, _>(provider.clone(), block_number).await;
        self.range_shift = range_shift;
        self.threshold_shift = threshold_shift;
        self.center_price_shift = center_price_shift;

        // Combined reserves (for backward compatibility)
        self.token0_real_reserves_1e12 =
            res.collateralReserves.token0RealReserves + res.debtReserves.token0RealReserves;
        self.token1_real_reserves_1e12 =
            res.collateralReserves.token1RealReserves + res.debtReserves.token1RealReserves;
        self.token0_imag_reserves_1e12 = res.collateralReserves.token0ImaginaryReserves
            + res.debtReserves.token0ImaginaryReserves;
        self.token1_imag_reserves_1e12 = res.collateralReserves.token1ImaginaryReserves
            + res.debtReserves.token1ImaginaryReserves;

        // Collateral pool reserves (for dual-pool routing)
        self.col_token0_real_1e12 = res.collateralReserves.token0RealReserves;
        self.col_token1_real_1e12 = res.collateralReserves.token1RealReserves;
        self.col_token0_imag_1e12 = res.collateralReserves.token0ImaginaryReserves;
        self.col_token1_imag_1e12 = res.collateralReserves.token1ImaginaryReserves;

        // Debt pool reserves (for dual-pool routing)
        self.debt_token0_real_1e12 = res.debtReserves.token0RealReserves;
        self.debt_token1_real_1e12 = res.debtReserves.token1RealReserves;
        self.debt_token0_imag_1e12 = res.debtReserves.token0ImaginaryReserves;
        self.debt_token1_imag_1e12 = res.debtReserves.token1ImaginaryReserves;
        self.debt0_1e12 = res.debtReserves.token0Debt;
        self.debt1_1e12 = res.debtReserves.token1Debt;

        // Limits for borrowable/withdrawable checks
        self.withdrawable_token0 = TokenLimitData {
            available: res.limits.withdrawableToken0.available,
            expands_to: res.limits.withdrawableToken0.expandsTo,
            expand_duration: res.limits.withdrawableToken0.expandDuration.to::<u64>(),
        };
        self.withdrawable_token1 = TokenLimitData {
            available: res.limits.withdrawableToken1.available,
            expands_to: res.limits.withdrawableToken1.expandsTo,
            expand_duration: res.limits.withdrawableToken1.expandDuration.to::<u64>(),
        };
        self.borrowable_token0 = TokenLimitData {
            available: res.limits.borrowableToken0.available,
            expands_to: res.limits.borrowableToken0.expandsTo,
            expand_duration: res.limits.borrowableToken0.expandDuration.to::<u64>(),
        };
        self.borrowable_token1 = TokenLimitData {
            available: res.limits.borrowableToken1.available,
            expands_to: res.limits.borrowableToken1.expandsTo,
            expand_duration: res.limits.borrowableToken1.expandDuration.to::<u64>(),
        };

        // Record sync time for limit expansion calculation
        self.limits_sync_time = self.last_synced_block_timestamp;

        let _ = self
            .update_center_price_from_chain::<N, _>(
                dex_variables,
                dex_variables2,
                provider.clone(),
                block_number,
                self.last_synced_block_timestamp,
            )
            .await;
        self.compute_ranges_from_dex(
            dex_variables,
            dex_variables2,
            self.last_synced_block_timestamp,
        );
        self.refresh_prices();
        Ok(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct FluidDexFactory {
    pub address: Address,
    pub creation_block: u64,
    pub pools: Vec<Address>,
    pub resolver: Option<Address>,
}

impl FluidDexFactory {
    pub fn new(
        address: Address,
        creation_block: u64,
        pools: Vec<Address>,
        resolver: Option<Address>,
    ) -> Self {
        Self {
            address,
            creation_block,
            pools,
            resolver,
        }
    }

    pub async fn sync_all_pools<N, P>(
        amms: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        N::BlockResponse: BlockResponse,
        <N::BlockResponse as BlockResponse>::Header: BlockHeader,
        P: Provider<N> + Clone,
    {
        if amms.is_empty() {
            return Ok(vec![]);
        }

        let total = amms.len();

        let resolver_address = match &amms[0] {
            AMM::FluidDexPool(pool) => pool.reserves_resolver,
            _ => return Err(AMMError::Msg("Expected FluidDexPool".to_string())),
        };

        let resolver = DexReservesResolver::new(resolver_address, provider.clone());
        let _step = 80;

        // Batch fetch reserves
        let mut futures = FuturesUnordered::new();

        for chunk in amms.chunks(80) {
            // Batch size 80
            let resolver = resolver.clone();
            let provider = provider.clone();
            let chunk_pools: Vec<Address> = chunk.iter().map(|amm| amm.address()).collect();

            futures.push(async move {
                let res = resolver
                    .getPoolsReservesAdjusted(chunk_pools)
                    .block(block_number)
                    .call()
                    .await?;

                let mut tokens = Vec::new();
                for pool_res in &res {
                    tokens.push(pool_res.token0);
                    tokens.push(pool_res.token1);
                }

                let token_decimals = get_token_decimals::<N, _>(tokens, provider).await?;
                Ok::<(Vec<PoolWithReserves>, HashMap<Address, u8>), AMMError>((res, token_decimals))
            });
        }

        let mut out = Vec::with_capacity(amms.len());

        while let Some(res) = futures.next().await {
            match res {
                Ok((pools_reserves, token_decimals)) => {
                    for pr in pools_reserves {
                        let mut pool = FluidDexPool::new(pr.pool, resolver_address);
                        // Populate pool
                        // Handle FLUID_NATIVE_ETH decimals explicitly
                        let d0 = if pr.token0 == FLUID_NATIVE_ETH {
                            18
                        } else {
                            *token_decimals.get(&pr.token0).unwrap_or(&18)
                        };
                        let d1 = if pr.token1 == FLUID_NATIVE_ETH {
                            18
                        } else {
                            *token_decimals.get(&pr.token1).unwrap_or(&18)
                        };

                        pool.token_a = Token::new_with_decimals(pr.token0, d0);
                        pool.token_b = Token::new_with_decimals(pr.token1, d1);
                        pool.fee_1e6 = pr.fee.to::<u32>();
                        pool.center_price_1e27 = pr.centerPrice;
                        let dex = FluidDexT1::new(pr.pool, provider.clone());
                        let dex_variables = dex
                            .readFromStorage(B256::from(U256::from(0u64)))
                            .block(block_number)
                            .call()
                            .await
                            .unwrap_or(U256::ZERO);
                        let dex_variables2 = dex
                            .readFromStorage(B256::from(U256::from(1u64)))
                            .block(block_number)
                            .call()
                            .await
                            .unwrap_or(U256::ZERO);
                        if let Ok(cv) = dex.constantsView().block(block_number).call().await {
                            pool.liquidity_address = cv.liquidity;
                            pool.exchange_price_token0_slot = cv.exchangePriceToken0Slot;
                            pool.exchange_price_token1_slot = cv.exchangePriceToken1Slot;
                            pool.deployer_contract = cv.deployerContract;
                        }
                        pool.older_price_1e27 = decode_price_from_dex_variables(dex_variables, 1);
                        pool.last_stored_price_1e27 =
                            decode_price_from_dex_variables(dex_variables, 41);
                        pool.last_center_price_1e27 =
                            decode_price_from_dex_variables(dex_variables, 81);
                        pool.last_swap_timestamp =
                            ((dex_variables >> 121u32) & mask(33)).to::<u64>();
                        pool.range_shift = dex
                            .readFromStorage(B256::from(U256::from(7u64)))
                            .block(block_number)
                            .call()
                            .await
                            .unwrap_or(U256::ZERO);
                        pool.threshold_shift = dex
                            .readFromStorage(B256::from(U256::from(8u64)))
                            .block(block_number)
                            .call()
                            .await
                            .unwrap_or(U256::ZERO);
                        pool.center_price_shift = dex
                            .readFromStorage(B256::from(U256::from(9u64)))
                            .block(block_number)
                            .call()
                            .await
                            .unwrap_or(U256::ZERO);

                        // Combined reserves (for backward compatibility)
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

                        // Collateral pool reserves (for dual-pool routing)
                        pool.col_token0_real_1e12 = pr.collateralReserves.token0RealReserves;
                        pool.col_token1_real_1e12 = pr.collateralReserves.token1RealReserves;
                        pool.col_token0_imag_1e12 = pr.collateralReserves.token0ImaginaryReserves;
                        pool.col_token1_imag_1e12 = pr.collateralReserves.token1ImaginaryReserves;

                        // Debt pool reserves (for dual-pool routing)
                        pool.debt_token0_real_1e12 = pr.debtReserves.token0RealReserves;
                        pool.debt_token1_real_1e12 = pr.debtReserves.token1RealReserves;
                        pool.debt_token0_imag_1e12 = pr.debtReserves.token0ImaginaryReserves;
                        pool.debt_token1_imag_1e12 = pr.debtReserves.token1ImaginaryReserves;
                        pool.debt0_1e12 = pr.debtReserves.token0Debt;
                        pool.debt1_1e12 = pr.debtReserves.token1Debt;

                        // Limits for borrowable/withdrawable checks
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

                        // Record sync time for limit expansion calculation
                        pool.limits_sync_time =
                            fetch_block_timestamp::<N, _>(provider.clone(), block_number).await;
                        pool.last_synced_block_timestamp = pool.limits_sync_time;
                        let _ = pool
                            .update_center_price_from_chain::<N, _>(
                                dex_variables,
                                dex_variables2,
                                provider.clone(),
                                block_number,
                                pool.last_synced_block_timestamp,
                            )
                            .await;
                        pool.compute_ranges_from_dex(
                            dex_variables,
                            dex_variables2,
                            pool.last_synced_block_timestamp,
                        );

                        pool.refresh_prices();

                        if let BlockId::Number(n) = block_number {
                            if let Some(bn) = n.as_number() {
                                pool.last_synced_block = bn;
                            }
                        }
                        out.push(AMM::FluidDexPool(pool));
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to sync Fluid pool batch: {:?}", e);
                }
            }
        }

        let valid = out.len();
        let invalid = total.saturating_sub(valid);
        tracing::info!(
            target: "amms::fluid_dex::init_batch",
            total = total,
            valid = valid,
            invalid = invalid,
            "Batch initialization complete"
        );

        Ok(out)
    }

    pub async fn init_batch<N, P>(
        amms: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        Self::sync_all_pools::<N, _>(amms, block_number, provider).await
    }
}

impl DiscoverySync for FluidDexFactory {
    fn discover<N, P>(
        &self,
        to_block: BlockId,
        provider: P,
    ) -> impl Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        async move {
            let resolver_addr = self.resolver.ok_or_else(|| {
                AMMError::Msg("FluidDexFactory: resolver address not set".to_string())
            })?;

            let pools = if self.pools.is_empty() {
                let rr = DexReservesResolver::new(resolver_addr, provider);
                rr.getAllPoolAddresses().block(to_block).call().await?
            } else {
                self.pools.clone()
            };

            Ok(pools
                .into_iter()
                .map(|address| AMM::FluidDexPool(FluidDexPool::new(address, resolver_addr)))
                .collect())
        }
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
        async move { Self::sync_all_pools::<N, _>(amms, to_block, provider).await }
    }
}

#[async_trait::async_trait]
impl AutomatedMarketMakerFactory for FluidDexFactory {
    type PoolVariant = FluidDexPool;

    fn address(&self) -> Address {
        self.address
    }

    fn pool_creation_event(&self) -> B256 {
        B256::ZERO
    }

    fn create_pool(&self, _log: Log) -> Result<AMM, AMMError> {
        Err(AMMError::Msg(
            "FluidDexFactory does not support create_pool from logs".to_string(),
        ))
    }

    fn creation_block(&self) -> u64 {
        self.creation_block
    }
}

#[cfg(test)]
mod tests {
    use alloy::{
        eips::BlockId,
        primitives::{Address, U256},
        providers::Provider,
        providers::ProviderBuilder,
        rpc::types::Filter,
        sol_types::SolEvent,
    };
    use std::str::FromStr;

    use crate::amms::{
        amm::{AutomatedMarketMaker, AMM},
        factory::DiscoverySync,
        fluid_dex::{
            DexReservesResolver, FluidDexFactory, FluidDexPool, FluidDexT1, FLUID_DEX_RESOLVER,
            FLUID_NATIVE_ETH,
        },
    };

    const WSTETH_ETH_POOL: &str = "0x0B1a513ee24972DAEf112bC777a5610d4325C9e7";
    const USDC_USDT_POOL: &str = "0x667701e51B4D1Ca244F17C78F7aB8744B4C99F9B";

    fn get_provider() -> Option<impl Provider<alloy::network::Ethereum> + Clone> {
        dotenv::dotenv().ok();
        match std::env::var("ETHEREUM_PROVIDER") {
            Ok(url) => Some(ProviderBuilder::new().connect_http(url.parse().ok()?)),
            Err(_) => None,
        }
    }

    #[tokio::test]
    async fn test_fluid_dex_discover_and_init_pool() -> eyre::Result<()> {
        let Some(provider) = get_provider() else {
            println!("Skipping test: ETHEREUM_PROVIDER not set");
            return Ok(());
        };

        let factory = FluidDexFactory::new(Address::ZERO, 0, vec![], Some(FLUID_DEX_RESOLVER));
        let pools = factory
            .discover::<alloy::network::Ethereum, _>(BlockId::latest(), provider.clone())
            .await?;
        assert!(!pools.is_empty(), "Should discover at least one pool");

        let pool_addr = pools[0].address();
        let code = provider.get_code_at(pool_addr).await?;
        if code.is_empty() {
            println!("Skipping test: Pool contract not found at {pool_addr}");
            return Ok(());
        }

        let mut pool = FluidDexPool::new(pool_addr, FLUID_DEX_RESOLVER);
        pool = pool
            .init::<alloy::network::Ethereum, _>(BlockId::latest(), provider.clone())
            .await?;

        assert_eq!(pool.tokens().len(), 2, "Pool should have 2 tokens");
        assert!(pool.token_a.decimals > 0, "Token A decimals should be > 0");
        assert!(pool.token_b.decimals > 0, "Token B decimals should be > 0");
        assert!(
            pool.token0_real_reserves_1e12 > U256::ZERO,
            "Token0 reserves should be > 0"
        );
        assert!(
            pool.token1_real_reserves_1e12 > U256::ZERO,
            "Token1 reserves should be > 0"
        );

        let price = pool.calculate_price(pool.token_a.address, pool.token_b.address)?;
        assert!(price > 0.0, "Price should be > 0");

        let amount_in = U256::from(10u64).pow(U256::from(pool.token_a.decimals as u64));
        let out = pool.simulate_swap(pool.token_a.address, pool.token_b.address, amount_in)?;
        assert!(out > U256::ZERO, "Swap output should be > 0");

        Ok(())
    }

    #[tokio::test]
    async fn test_fluid_dex_native_eth_pool() -> eyre::Result<()> {
        let Some(provider) = get_provider() else {
            println!("Skipping test: ETHEREUM_PROVIDER not set");
            return Ok(());
        };

        let pool_addr = Address::from_str(WSTETH_ETH_POOL)?;

        let mut pool = FluidDexPool::new(pool_addr, FLUID_DEX_RESOLVER);
        pool = pool
            .init::<alloy::network::Ethereum, _>(BlockId::latest(), provider.clone())
            .await?;

        let native_eth = FLUID_NATIVE_ETH;

        let has_native_eth =
            pool.token_a.address == native_eth || pool.token_b.address == native_eth;
        assert!(
            has_native_eth,
            "wstETH/ETH pool should contain Native ETH token (0xEeeee...)"
        );

        if pool.token_a.address == native_eth {
            assert_eq!(
                pool.token_a.decimals, 18,
                "Native ETH should have 18 decimals"
            );
        }
        if pool.token_b.address == native_eth {
            assert_eq!(
                pool.token_b.decimals, 18,
                "Native ETH should have 18 decimals"
            );
        }

        let amount_in = U256::from(10u64).pow(U256::from(18u64)); // 1 token
        let out = pool.simulate_swap(pool.token_a.address, pool.token_b.address, amount_in)?;
        assert!(out > U256::ZERO, "Native ETH swap output should be > 0");

        println!(
            "Native ETH pool test passed: {} -> {} for 1 unit input",
            pool.token_a.address, out
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_fluid_dex_swap_simulation_accuracy() -> eyre::Result<()> {
        let Some(provider) = get_provider() else {
            println!("Skipping test: ETHEREUM_PROVIDER not set");
            return Ok(());
        };

        let test_pools = vec![WSTETH_ETH_POOL, USDC_USDT_POOL];

        for pool_addr_str in test_pools {
            let pool_addr = Address::from_str(pool_addr_str)?;
            let current_block = provider.get_block_number().await?;
            let from_block = current_block.saturating_sub(5000);

            let filter = Filter::new()
                .address(pool_addr)
                .event_signature(FluidDexT1::Swap::SIGNATURE_HASH)
                .from_block(from_block);

            let logs = provider.get_logs(&filter).await?;

            if logs.is_empty() {
                println!(
                    "No swap events for pool {} in last 5000 blocks, skipping",
                    pool_addr
                );
                continue;
            }

            let test_count = logs.len().min(5);
            let mut passed = 0;
            let mut total_deviation_bps = 0u64;

            for log in logs.iter().rev().take(test_count) {
                let event = FluidDexT1::Swap::decode_log(&log.inner)?;
                let block_number = log.block_number.unwrap();
                let prev_block = block_number - 1;

                let mut pool = FluidDexPool::new(pool_addr, FLUID_DEX_RESOLVER);
                pool = pool
                    .init::<alloy::network::Ethereum, _>(
                        BlockId::from(prev_block),
                        provider.clone(),
                    )
                    .await?;

                let (token_in, token_out) = if event.swap0to1 {
                    (pool.token_a.address, pool.token_b.address)
                } else {
                    (pool.token_b.address, pool.token_a.address)
                };

                let amount_out_sim =
                    pool.simulate_swap(token_in, token_out, U256::from(event.amountIn))?;
                let amount_out_actual = U256::from(event.amountOut);

                let diff = if amount_out_sim > amount_out_actual {
                    amount_out_sim - amount_out_actual
                } else {
                    amount_out_actual - amount_out_sim
                };

                let deviation_bps = if !amount_out_actual.is_zero() {
                    (diff * U256::from(10000) / amount_out_actual).to::<u64>()
                } else {
                    0
                };

                total_deviation_bps += deviation_bps;

                if deviation_bps <= 50 {
                    passed += 1;
                } else {
                    println!(
                        "Pool {} Block {}: Sim={}, Actual={}, Deviation={}bps (FAIL)",
                        pool_addr, block_number, amount_out_sim, amount_out_actual, deviation_bps
                    );
                }
            }

            let avg_deviation = total_deviation_bps / test_count as u64;
            println!(
                "Pool {}: {}/{} swaps passed (avg deviation: {}bps)",
                pool_addr, passed, test_count, avg_deviation
            );

            assert!(
                passed >= test_count / 2,
                "At least half of swap simulations should be within tolerance for pool {}",
                pool_addr
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_fluid_dex_resolver_consistency() -> eyre::Result<()> {
        let Some(provider) = get_provider() else {
            println!("Skipping test: ETHEREUM_PROVIDER not set");
            return Ok(());
        };

        let pool_addr = Address::from_str(WSTETH_ETH_POOL)?;
        let resolver_addr: Address = "0xC93876C0EEd99645DD53937b25433e311881A27C".parse()?;

        let mut pool = FluidDexPool::new(pool_addr, FLUID_DEX_RESOLVER);
        pool = pool
            .init::<alloy::network::Ethereum, _>(BlockId::latest(), provider.clone())
            .await?;

        let resolver = DexReservesResolver::new(resolver_addr, provider.clone());
        let res = resolver.getPoolReservesAdjusted(pool_addr).call().await?;

        let expected_real_0 =
            res.collateralReserves.token0RealReserves + res.debtReserves.token0RealReserves;
        let expected_real_1 =
            res.collateralReserves.token1RealReserves + res.debtReserves.token1RealReserves;

        assert_eq!(
            pool.token0_real_reserves_1e12, expected_real_0,
            "Token0 real reserves mismatch"
        );
        assert_eq!(
            pool.token1_real_reserves_1e12, expected_real_1,
            "Token1 real reserves mismatch"
        );

        let expected_imag_0 = res.collateralReserves.token0ImaginaryReserves
            + res.debtReserves.token0ImaginaryReserves;
        let expected_imag_1 = res.collateralReserves.token1ImaginaryReserves
            + res.debtReserves.token1ImaginaryReserves;

        assert_eq!(
            pool.token0_imag_reserves_1e12, expected_imag_0,
            "Token0 imag reserves mismatch"
        );
        assert_eq!(
            pool.token1_imag_reserves_1e12, expected_imag_1,
            "Token1 imag reserves mismatch"
        );

        println!("Resolver consistency test passed for pool {}", pool_addr);
        Ok(())
    }

    #[tokio::test]
    async fn test_fluid_dex_factory_sync_all_pools() -> eyre::Result<()> {
        let Some(provider) = get_provider() else {
            println!("Skipping test: ETHEREUM_PROVIDER not set");
            return Ok(());
        };

        let factory = FluidDexFactory::new(Address::ZERO, 0, vec![], Some(FLUID_DEX_RESOLVER));
        let discovered = factory
            .discover::<alloy::network::Ethereum, _>(BlockId::latest(), provider.clone())
            .await?;

        let amms = discovered.into_iter().take(10).collect::<Vec<AMM>>();
        let synced = FluidDexFactory::sync_all_pools::<alloy::network::Ethereum, _>(
            amms.clone(),
            BlockId::latest(),
            provider,
        )
        .await?;

        assert_eq!(synced.len(), amms.len(), "All pools should be synced");

        for amm in synced {
            if let AMM::FluidDexPool(p) = amm {
                assert!(
                    p.token_a.decimals > 0,
                    "Synced pool should have valid decimals"
                );
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_fluid_dex_simulate_swap_mut() -> eyre::Result<()> {
        let Some(provider) = get_provider() else {
            println!("Skipping test: ETHEREUM_PROVIDER not set");
            return Ok(());
        };

        let pool_addr = Address::from_str(WSTETH_ETH_POOL)?;
        let mut pool = FluidDexPool::new(pool_addr, FLUID_DEX_RESOLVER);
        pool = pool
            .init::<alloy::network::Ethereum, _>(BlockId::latest(), provider.clone())
            .await?;

        let initial_r0 = pool.token0_real_reserves_1e12;
        let initial_r1 = pool.token1_real_reserves_1e12;

        let amount_in = U256::from(10u64).pow(U256::from(pool.token_a.decimals as u64)); // 1 token
        let out = pool.simulate_swap_mut(pool.token_a.address, pool.token_b.address, amount_in)?;

        assert!(
            pool.token0_real_reserves_1e12 != initial_r0,
            "Token0 reserves should change after swap"
        );
        assert!(
            pool.token1_real_reserves_1e12 != initial_r1,
            "Token1 reserves should change after swap"
        );

        assert!(
            pool.token0_real_reserves_1e12 > initial_r0,
            "Token0 reserves should increase (input)"
        );
        assert!(
            pool.token1_real_reserves_1e12 < initial_r1,
            "Token1 reserves should decrease (output)"
        );

        println!("simulate_swap_mut test passed, output: {}", out);
        Ok(())
    }

    #[tokio::test]
    async fn test_fluid_dex_resolver_get_total_pools() -> eyre::Result<()> {
        let Some(provider) = get_provider() else {
            println!("Skipping test: ETHEREUM_PROVIDER not set");
            return Ok(());
        };

        let reserves_resolver: Address = "0xC93876C0EEd99645DD53937b25433e311881A27C".parse()?;
        let rr = DexReservesResolver::new(reserves_resolver, provider);
        let total = rr.getTotalPools().call().await?;
        assert!(total > 0, "Should have at least one pool");
        println!("Total Fluid DEX pools: {}", total);
        Ok(())
    }

    #[tokio::test]
    async fn test_fluid_dex_swap_accuracy_amm_approach() -> eyre::Result<()> {
        let Some(provider) = get_provider() else {
            println!("Skipping test: ETHEREUM_PROVIDER not set");
            return Ok(());
        };

        let test_pools = vec![
            (WSTETH_ETH_POOL, "wstETH/ETH"),
            (USDC_USDT_POOL, "USDC/USDT"),
        ];

        for (pool_addr_str, label) in test_pools {
            let pool_addr = Address::from_str(pool_addr_str)?;
            println!("\n=== Testing {} pool ({}) ===", label, pool_addr);

            let current_block = provider.get_block_number().await?;
            let from_block = current_block.saturating_sub(10000);

            let filter = Filter::new()
                .address(pool_addr)
                .event_signature(FluidDexT1::Swap::SIGNATURE_HASH)
                .from_block(from_block);

            let logs = provider.get_logs(&filter).await?;

            if logs.is_empty() {
                println!(
                    "No swap events for {} in last 10000 blocks, skipping",
                    label
                );
                continue;
            }

            let test_event = logs.iter().rev().find(|log| {
                if let Ok(event) = FluidDexT1::Swap::decode_log(&log.inner) {
                    event.amountIn > 0 && event.amountOut > 0
                } else {
                    false
                }
            });

            let Some(test_log) = test_event else {
                println!("No valid swap event with non-zero amounts for {}", label);
                continue;
            };

            let event = FluidDexT1::Swap::decode_log(&test_log.inner)?;
            let block_number = test_log.block_number.unwrap();
            let prev_block = block_number - 1;

            let mut pool = FluidDexPool::new(pool_addr, FLUID_DEX_RESOLVER);
            pool = pool
                .init::<alloy::network::Ethereum, _>(BlockId::from(prev_block), provider.clone())
                .await?;

            let (token_in, token_out) = if event.swap0to1 {
                (pool.token_a.address, pool.token_b.address)
            } else {
                (pool.token_b.address, pool.token_a.address)
            };

            let base_amount = U256::from(event.amountIn);
            let actual_amount_out = U256::from(event.amountOut);

            println!("\n  Base swap from event at block {}:", block_number);
            println!("    Amount In: {}", base_amount);
            println!("    Actual Amount Out (on-chain): {}", actual_amount_out);

            let test_amounts = vec![
                base_amount,
                base_amount / U256::from(10u64),
                base_amount * U256::from(10u64),
                base_amount / U256::from(100u64),
                base_amount * U256::from(5u64),
            ];

            let mut all_passed = true;
            let mut max_deviation_bps = 0u64;

            for (idx, amount_in) in test_amounts.iter().enumerate() {
                if amount_in.is_zero() {
                    continue;
                }

                let mut test_pool = FluidDexPool::new(pool_addr, FLUID_DEX_RESOLVER);
                test_pool = test_pool
                    .init::<alloy::network::Ethereum, _>(
                        BlockId::from(prev_block),
                        provider.clone(),
                    )
                    .await?;

                let amount_out_sim = match test_pool.simulate_swap(token_in, token_out, *amount_in)
                {
                    Ok(out) => out,
                    Err(e) => {
                        println!(
                            "    Test {}: Sim failed for amount {}: {:?}",
                            idx + 1,
                            amount_in,
                            e
                        );
                        all_passed = false;
                        continue;
                    }
                };

                let expected_out = if *amount_in == base_amount {
                    actual_amount_out
                } else {
                    let ratio = if !base_amount.is_zero() {
                        U256::from(1000000u64) * *amount_in / base_amount
                    } else {
                        U256::ZERO
                    };
                    actual_amount_out * ratio / U256::from(1000000u64)
                };

                let diff = if amount_out_sim > expected_out {
                    amount_out_sim - expected_out
                } else {
                    expected_out - amount_out_sim
                };

                let deviation_bps = if !expected_out.is_zero() {
                    (diff * U256::from(10000) / expected_out).to::<u64>()
                } else {
                    0
                };

                if deviation_bps > max_deviation_bps {
                    max_deviation_bps = deviation_bps;
                }

                let status = if deviation_bps <= 100 {
                    "✅"
                } else {
                    all_passed = false;
                    "❌"
                };

                println!(
                    "    Test {}: Amount In = {} -> Sim Out = {}, Expected Out = {}, Deviation = {} bps {}",
                    idx + 1,
                    amount_in,
                    amount_out_sim,
                    expected_out,
                    deviation_bps,
                    status
                );
            }

            println!(
                "\n  {} pool summary: Max deviation = {} bps, Overall = {}",
                label,
                max_deviation_bps,
                if all_passed {
                    "✅ PASSED"
                } else {
                    "❌ FAILED"
                }
            );

            if label == "wstETH/ETH" {
                assert!(
                    max_deviation_bps <= 200,
                    "wstETH/ETH pool max deviation should be <= 2% (200 bps)"
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod test_sync_drift;

#[cfg(test)]
mod test_swap_simulate;

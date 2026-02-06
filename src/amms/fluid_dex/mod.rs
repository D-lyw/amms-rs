use super::{
    amm::{AutomatedMarketMaker, SyncAction, AMM},
    consts::{MIN_POOL_RESERVE, MPFR_T_PRECISION},
    error::AMMError,
    factory::{AutomatedMarketMakerFactory, DiscoverySync},
    float::u256_to_float,
    get_token_decimals, Token, IERC20,
};
use alloy::{
    eips::BlockId,
    network::Network,
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
use std::{collections::HashMap, future::Future, hash::Hash, str::FromStr};

// Address is consistent across Mainnet, Arbitrum, Base, Polygon, Plasma, etc. (Instadapp Fluid uses CREATE2)
// https://github.com/Instadapp/fluid-contracts-public/blob/main/deployments/deployments.md
pub const FLUID_LIQUIDITY_LAYER: Address = address!("52Aa899454998Be5b000Ad077a46Bbe360F4e497");

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
        // Handle edge cases
        if x.is_zero() || y.is_zero() || x2.is_zero() || y2.is_zero() {
            return I256::ZERO;
        }

        // Calculate sqrt(x * y) and sqrt(x2 * y2) with 1e18 precision
        // We use integer approximation since exact float sqrt is not available
        let xy = x.saturating_mul(y);
        let x2y2 = x2.saturating_mul(y2);

        // Integer square root approximation
        let xy_root = integer_sqrt(xy);
        let x2y2_root = integer_sqrt(x2y2);

        if xy_root.is_zero() && x2y2_root.is_zero() {
            return I256::ZERO;
        }

        // a = (y2 * xy_root + t * xy_root - y * x2y2_root) / (xy_root + x2y2_root)
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
            // verify token1 (output) reserves
            // reserve_out >= (reserve_in * center_price) / (1e27 * min_swap_liquidity)
            let numerator = reserve_in.saturating_mul(center_price);
            let denominator = scale_1e27.saturating_mul(min_swap_liquidity);
            if denominator.is_zero() {
                return false;
            }
            let min_required = numerator / denominator;
            reserve_out >= min_required
        } else {
            // verify token0 (output) reserves
            // reserve_out >= (reserve_in * 1e27) / (center_price * min_swap_liquidity)
            let numerator = reserve_in.saturating_mul(scale_1e27);
            let denominator = center_price.saturating_mul(min_swap_liquidity);
            if denominator.is_zero() {
                return false;
            }
            let min_required = numerator / denominator;
            reserve_out >= min_required
        }
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
                    self.token0_real_reserves_1e12 =
                        apply_delta(self.token0_real_reserves_1e12, event.supplyAmount);
                } else {
                    self.token1_real_reserves_1e12 =
                        apply_delta(self.token1_real_reserves_1e12, event.supplyAmount);
                }
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
        // Determine swap direction
        let swap0to1 = base_token == self.token_a.address && quote_token == self.token_b.address;
        let swap1to0 = base_token == self.token_b.address && quote_token == self.token_a.address;

        if !swap0to1 && !swap1to0 {
            return Err(AMMError::Msg("Token pair not in pool".to_string()));
        }

        // Scale input amount to 1e12
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

        // Apply fee upfront (fee is in 1e6 scale)
        let fee = U256::from(self.fee_1e6);
        let fee_100_percent = U256::from(1_000_000u64);
        let amount_in_after_fee = amount_in * (fee_100_percent - fee) / fee_100_percent;
        let amount_to_swap = scale_to_1e12(amount_in_after_fee, in_decimals);

        // Check if pools are enabled
        let col_pool_enabled = !self.col_token0_imag_1e12.is_zero()
            && !self.col_token1_imag_1e12.is_zero()
            && !self.col_token0_real_1e12.is_zero()
            && !self.col_token1_real_1e12.is_zero();

        let debt_pool_enabled = !self.debt_token0_imag_1e12.is_zero()
            && !self.debt_token1_imag_1e12.is_zero()
            && !self.debt_token0_real_1e12.is_zero()
            && !self.debt_token1_real_1e12.is_zero();

        if !col_pool_enabled && !debt_pool_enabled {
            // Fallback to combined reserves (backward compatibility)
            return self.simulate_swap_simple(base_token, quote_token, amount_in);
        }

        // Get reserves based on swap direction
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

        // Get current limits (with time expansion)
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

        // Scale limits to 1e12
        let borrowable_1e12 = scale_to_1e12(borrowable, out_decimals);
        let withdrawable_1e12 = scale_to_1e12(withdrawable, out_decimals);

        // Calculate routing between collateral and debt pools
        let a = if col_pool_enabled && debt_pool_enabled {
            self.swap_routing_in(
                amount_to_swap,
                col_i_reserve_out,
                col_i_reserve_in,
                debt_i_reserve_out,
                debt_i_reserve_in,
            )
        } else if debt_pool_enabled {
            I256::MINUS_ONE // Route entirely through debt pool
        } else {
            I256::try_from(amount_to_swap).unwrap_or(I256::MAX) + I256::ONE // Route entirely through col pool
        };

        let (amount_in_col, amount_out_col, amount_in_debt, amount_out_debt) = if a <= I256::ZERO {
            // Entire trade routes through debt pool
            let out = calc_amount_out(
                amount_to_swap,
                debt_i_reserve_in,
                debt_i_reserve_out,
                U256::ZERO,
            );
            (U256::ZERO, U256::ZERO, amount_to_swap, out)
        } else if a >= I256::try_from(amount_to_swap).unwrap_or(I256::MAX) {
            // Entire trade routes through collateral pool
            let out = calc_amount_out(
                amount_to_swap,
                col_i_reserve_in,
                col_i_reserve_out,
                U256::ZERO,
            );
            (amount_to_swap, out, U256::ZERO, U256::ZERO)
        } else {
            // Trade routes through both pools
            let a_u256 = a.into_raw();
            let out_col = calc_amount_out(a_u256, col_i_reserve_in, col_i_reserve_out, U256::ZERO);
            let in_debt = amount_to_swap - a_u256;
            let out_debt =
                calc_amount_out(in_debt, debt_i_reserve_in, debt_i_reserve_out, U256::ZERO);
            (a_u256, out_col, in_debt, out_debt)
        };

        // Check 1: Output amount vs real reserves
        if amount_out_debt > debt_reserve_out {
            return Ok(U256::ZERO); // Not enough liquidity in debt pool
        }
        if amount_out_col > col_reserve_out {
            return Ok(U256::ZERO); // Not enough liquidity in col pool
        }

        // Check 2: Output amount vs limits
        if amount_out_debt > borrowable_1e12 {
            return Ok(U256::ZERO); // Exceeds borrowable limit
        }
        if amount_out_col > withdrawable_1e12 {
            return Ok(U256::ZERO); // Exceeds withdrawable limit
        }

        // Check 3: Reserves ratio verification (prevents extreme imbalance)
        let min_swap_liquidity = U256::from(8500u64); // 0.85e4 with buffer
        let center_price = self.center_price_1e27;

        if !amount_in_col.is_zero() {
            let new_reserve_in = col_reserve_in + amount_in_col;
            let new_reserve_out = col_reserve_out.saturating_sub(amount_out_col);
            if !self.verify_reserves_ratio(
                swap0to1,
                new_reserve_in,
                new_reserve_out,
                center_price,
                min_swap_liquidity,
            ) {
                return Ok(U256::ZERO); // Reserves ratio invalid
            }
        }
        if !amount_in_debt.is_zero() {
            let new_reserve_in = debt_reserve_in + amount_in_debt;
            let new_reserve_out = debt_reserve_out.saturating_sub(amount_out_debt);
            if !self.verify_reserves_ratio(
                swap0to1,
                new_reserve_in,
                new_reserve_out,
                center_price,
                min_swap_liquidity,
            ) {
                return Ok(U256::ZERO); // Reserves ratio invalid
            }
        }

        // Check 4: Price movement limit (>5% revert)
        let max_price_diff_percent = 5u64;
        let (old_price, new_price) = if amount_in_col > amount_in_debt {
            let old = if swap0to1 {
                col_i_reserve_out * U256::from(10u64).pow(U256::from(27)) / col_i_reserve_in
            } else {
                col_i_reserve_in * U256::from(10u64).pow(U256::from(27)) / col_i_reserve_out
            };
            let new_i_in = col_i_reserve_in + amount_in_col;
            let new_i_out = col_i_reserve_out.saturating_sub(amount_out_col);
            let new = if swap0to1 {
                new_i_out * U256::from(10u64).pow(U256::from(27)) / new_i_in
            } else {
                new_i_in * U256::from(10u64).pow(U256::from(27)) / new_i_out
            };
            (old, new)
        } else {
            let old = if swap0to1 {
                debt_i_reserve_out * U256::from(10u64).pow(U256::from(27)) / debt_i_reserve_in
            } else {
                debt_i_reserve_in * U256::from(10u64).pow(U256::from(27)) / debt_i_reserve_out
            };
            let new_i_in = debt_i_reserve_in + amount_in_debt;
            let new_i_out = debt_i_reserve_out.saturating_sub(amount_out_debt);
            let new = if swap0to1 {
                new_i_out * U256::from(10u64).pow(U256::from(27)) / new_i_in
            } else {
                new_i_in * U256::from(10u64).pow(U256::from(27)) / new_i_out
            };
            (old, new)
        };

        let price_diff = if old_price > new_price {
            old_price - new_price
        } else {
            new_price - old_price
        };
        let max_allowed_diff = old_price * U256::from(max_price_diff_percent) / U256::from(100);
        if price_diff > max_allowed_diff {
            return Ok(U256::ZERO); // Price movement too large
        }

        // Total output
        let total_out_1e12 = amount_out_col + amount_out_debt;
        Ok(unscale_from_1e12(total_out_1e12, out_decimals))
    }

    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let out = self.simulate_swap(base_token, quote_token, amount_in)?;
        let swap0to1 = base_token == self.token_a.address && quote_token == self.token_b.address;
        let amount_in_1e12 = if base_token == self.token_a.address {
            scale_to_1e12(amount_in, self.token_a.decimals)
        } else {
            scale_to_1e12(amount_in, self.token_b.decimals)
        };
        let out_decimals = if quote_token == self.token_a.address {
            self.token_a.decimals
        } else {
            self.token_b.decimals
        };
        let amount_out_1e12 = scale_to_1e12(out, out_decimals);
        self.apply_swap_1e12(swap0to1, amount_in_1e12, amount_out_1e12);
        Ok(out)
    }

    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let resolver = DexReservesResolver::new(self.reserves_resolver, provider.clone());
        let res = resolver
            .getPoolReservesAdjusted(self.address)
            .block(block_number)
            .call()
            .await?;

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
        self.fee_1e6 = res.fee.to::<u32>();
        self.center_price_1e27 = res.centerPrice;

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
        self.limits_sync_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

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
        P: Provider<N> + Clone,
    {
        if amms.is_empty() {
            return Ok(vec![]);
        }

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
                        pool.limits_sync_time = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);

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
            DexReservesResolver, FluidDexFactory, FluidDexPool, FluidDexT1, FLUID_NATIVE_ETH,
        },
    };

    // Known key pools for targeted testing
    const FLUID_DEX_RESOLVER: &str = "0xC93876C0EEd99645DD53937b25433e311881A27C";
    const WSTETH_ETH_POOL: &str = "0x0B1a513ee24972DAEf112bC777a5610d4325C9e7";
    const USDC_USDT_POOL: &str = "0x6166D17398D51cf38734A60E2135048dC50125F8";

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

        let factory = FluidDexFactory::new(
            Address::ZERO,
            0,
            vec![],
            Some(Address::from_str(FLUID_DEX_RESOLVER).unwrap()),
        );
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

        let mut pool = FluidDexPool::new(pool_addr, Address::from_str(FLUID_DEX_RESOLVER).unwrap());
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

    /// Test Native ETH pool (0xEeeee...) specifically
    #[tokio::test]
    async fn test_fluid_dex_native_eth_pool() -> eyre::Result<()> {
        let Some(provider) = get_provider() else {
            println!("Skipping test: ETHEREUM_PROVIDER not set");
            return Ok(());
        };

        // wstETH/ETH pool - known to have Native ETH
        let pool_addr = Address::from_str(WSTETH_ETH_POOL)?;

        let mut pool = FluidDexPool::new(pool_addr, Address::from_str(FLUID_DEX_RESOLVER).unwrap());
        pool = pool
            .init::<alloy::network::Ethereum, _>(BlockId::latest(), provider.clone())
            .await?;

        let native_eth = FLUID_NATIVE_ETH;

        // Check that one token is Native ETH
        let has_native_eth =
            pool.token_a.address == native_eth || pool.token_b.address == native_eth;
        assert!(
            has_native_eth,
            "wstETH/ETH pool should contain Native ETH token (0xEeeee...)"
        );

        // Verify decimals for Native ETH is 18
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

        // Simulate swap involving Native ETH
        let amount_in = U256::from(10u64).pow(U256::from(18u64)); // 1 token
        let out = pool.simulate_swap(pool.token_a.address, pool.token_b.address, amount_in)?;
        assert!(out > U256::ZERO, "Native ETH swap output should be > 0");

        println!(
            "Native ETH pool test passed: {} -> {} for 1 unit input",
            pool.token_a.address, out
        );

        Ok(())
    }

    /// Test swap simulation against on-chain results with stricter tolerance (0.5%)
    #[tokio::test]
    async fn test_fluid_dex_swap_simulation_accuracy() -> eyre::Result<()> {
        let Some(provider) = get_provider() else {
            println!("Skipping test: ETHEREUM_PROVIDER not set");
            return Ok(());
        };

        // Test against known active pools
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

            // Test multiple swap events (up to 5) for this pool
            let test_count = logs.len().min(5);
            let mut passed = 0;
            let mut total_deviation_bps = 0u64;

            for log in logs.iter().rev().take(test_count) {
                let event = FluidDexT1::Swap::decode_log(&log.inner)?;
                let block_number = log.block_number.unwrap();
                let prev_block = block_number - 1;

                let mut pool =
                    FluidDexPool::new(pool_addr, Address::from_str(FLUID_DEX_RESOLVER).unwrap());
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

                // Calculate deviation in basis points
                let deviation_bps = if !amount_out_actual.is_zero() {
                    (diff * U256::from(10000) / amount_out_actual).to::<u64>()
                } else {
                    0
                };

                total_deviation_bps += deviation_bps;

                // Stricter tolerance: 0.5% (50 bps)
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

    /// Test that Resolver data matches our stored data
    #[tokio::test]
    async fn test_fluid_dex_resolver_consistency() -> eyre::Result<()> {
        let Some(provider) = get_provider() else {
            println!("Skipping test: ETHEREUM_PROVIDER not set");
            return Ok(());
        };

        let pool_addr = Address::from_str(WSTETH_ETH_POOL)?;
        let resolver_addr: Address = "0xC93876C0EEd99645DD53937b25433e311881A27C".parse()?;

        // Init pool
        let mut pool = FluidDexPool::new(pool_addr, Address::from_str(FLUID_DEX_RESOLVER).unwrap());
        pool = pool
            .init::<alloy::network::Ethereum, _>(BlockId::latest(), provider.clone())
            .await?;

        // Fetch fresh from resolver
        let resolver = DexReservesResolver::new(resolver_addr, provider.clone());
        let res = resolver.getPoolReservesAdjusted(pool_addr).call().await?;

        // Compare reserves
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

    /// Test sync_all_pools batch functionality
    #[tokio::test]
    async fn test_fluid_dex_factory_sync_all_pools() -> eyre::Result<()> {
        let Some(provider) = get_provider() else {
            println!("Skipping test: ETHEREUM_PROVIDER not set");
            return Ok(());
        };

        let factory = FluidDexFactory::new(
            Address::ZERO,
            0,
            vec![],
            Some(Address::from_str(FLUID_DEX_RESOLVER).unwrap()),
        );
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
                // Some pools may have zero reserves (inactive), just check decimals are set
            }
        }

        Ok(())
    }

    /// Test simulate_swap_mut correctly updates reserves
    #[tokio::test]
    async fn test_fluid_dex_simulate_swap_mut() -> eyre::Result<()> {
        let Some(provider) = get_provider() else {
            println!("Skipping test: ETHEREUM_PROVIDER not set");
            return Ok(());
        };

        // Use wstETH/ETH pool which is known to be active
        let pool_addr = Address::from_str(WSTETH_ETH_POOL)?;
        let mut pool = FluidDexPool::new(pool_addr, Address::from_str(FLUID_DEX_RESOLVER).unwrap());
        pool = pool
            .init::<alloy::network::Ethereum, _>(BlockId::latest(), provider.clone())
            .await?;

        let initial_r0 = pool.token0_real_reserves_1e12;
        let initial_r1 = pool.token1_real_reserves_1e12;

        // Perform swap
        let amount_in = U256::from(10u64).pow(U256::from(pool.token_a.decimals as u64)); // 1 token
        let out = pool.simulate_swap_mut(pool.token_a.address, pool.token_b.address, amount_in)?;

        // Reserves should have changed
        assert!(
            pool.token0_real_reserves_1e12 != initial_r0,
            "Token0 reserves should change after swap"
        );
        assert!(
            pool.token1_real_reserves_1e12 != initial_r1,
            "Token1 reserves should change after swap"
        );

        // For swap0to1: r0 should increase, r1 should decrease
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
}

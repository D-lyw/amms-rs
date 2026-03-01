//! Aerodrome V2 Pool Implementation
//!
//! This module provides the core pool implementation for Aerodrome V2 AMM,
//! supporting both volatile and stable pool types through the same struct.
//!
//! # Pool Types
//!
//! - **Volatile** (`stable = false`): Standard `x * y = k` constant product AMM
//! - **Stable** (`stable = true`): Stable swap using `x³y + y³x = k` with Newton-Raphson iteration
//!
//! Both pool types use the same contract (`Pool.sol`) but different swap calculations.

use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, U256},
    providers::Provider,
    rpc::types::{Filter, Log},
    sol,
    sol_types::{SolCall, SolEvent, SolValue},
};
use futures::{stream::FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::amms::{
    amm::{AutomatedMarketMaker, SyncAction, AMM},
    consts::{MPFR_T_PRECISION, MIN_POOL_RESERVE},
    error::AMMError,
    factory::{AutomatedMarketMakerFactory, DiscoverySync},
    Token,
};
use rug::Float;
use rug::ops::Pow;

// Import batch contract ABI
sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    IGetAerodromeV2PoolDataBatchRequest,
    "src/amms/abi/GetAerodromeV2PoolDataBatchRequest.json"
);

pub use IGetAerodromeV2PoolDataBatchRequest::IGetAerodromeV2PoolDataBatchRequestInstance;

sol! {
    #[allow(missing_docs)]
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IAerodromeV2Pool {
        event Sync(uint112 reserve0, uint112 reserve1);
        function token0() external view returns (address);
        function token1() external view returns (address);
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
        function metadata() external view returns (uint256 dec0, uint256 dec1, uint256 r0, uint256 r1, bool st, address t0, address t1);
        function stable() external view returns (bool);
        function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external returns (uint256, uint256);
    }

    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IAerodromeV2Factory {
        event PoolCreated(address indexed token0, address indexed token1, address pool, bool stable);
        function getFee(address pool, bool stable) external view returns (uint24);
    }
}

/// Aerodrome V2 Pool
///
/// This pool type supports both volatile and stable pools through the `stable` flag.
///
/// # Example
///
/// ```rust,no_run
/// use amms::aerodrome_v2::AerodromeV2Pool;
/// use alloy::primitives::address;
///
/// // Create a volatile pool
/// let volatile_pool = AerodromeV2Pool::new(address!("0x..."));
///
/// // Create a stable pool
/// let mut stable_pool = AerodromeV2Pool::new(address!("0x..."));
/// stable_pool.stable = true;
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AerodromeV2Pool {
    /// Pool address
    pub address: Address,
    /// Last synced block number
    #[serde(default)]
    pub last_synced_block: u64,
    /// Token A (token0)
    pub token_a: Token,
    /// Token B (token1)
    pub token_b: Token,
    /// Reserve of token0
    pub reserve_0: u128,
    /// Reserve of token1
    pub reserve_1: u128,
    /// Pool fee (fetch from factory dynamically)
    pub fee: u32,
    /// Stable flag - determines swap calculation method
    /// - false: Volatile pool (x * y = k)
    /// - true: Stable pool (x³y + y³x = k)
    pub stable: bool,
    /// Cached price of token A in terms of token B
    #[serde(default)]
    pub token_a_price: f64,
    /// Cached price of token B in terms of token A
    #[serde(default)]
    pub token_b_price: f64,
}

impl AerodromeV2Pool {
    /// Create a new Aerodrome V2 pool
    pub fn new(address: Address) -> Self {
        Self {
            address,
            ..Default::default()
        }
    }

    /// Create a new volatile pool
    pub fn new_volatile(address: Address) -> Self {
        Self {
            address,
            stable: false,
            ..Default::default()
        }
    }

    /// Create a new stable pool
    pub fn new_stable(address: Address) -> Self {
        Self {
            address,
            stable: true,
            ..Default::default()
        }
    }

    /// Calculates the amount received for a given `amount_in` `reserve_in` and `reserve_out`
    /// for volatile pools using the standard constant product formula.
    ///
    /// Formula: `output = (input * reserveOut) / (reserveIn + input)`
    pub fn get_amount_out_volatile(&self, amount_in: U256, reserve_in: U256, reserve_out: U256) -> U256 {
        if amount_in.is_zero() || reserve_in.is_zero() || reserve_out.is_zero() {
            return U256::ZERO;
        }

        // Fee is in hundredths of a bip (1e10 as base)
        let fee = U256::from(100000u64) - U256::from(self.fee);
        let amount_in_with_fee = amount_in * fee;
        let numerator = amount_in_with_fee * reserve_out;
        let denominator = reserve_in * U256::from(100000u64) + amount_in_with_fee;

        numerator / denominator
    }

    /// Calculates the amount received for a given `amount_in` for stable pools
    /// using Curve-style StableSwap formula.
    ///
    /// Formula: A·n^n·Σx_i + D = A·D·n^n + D^(n+1) / (n^n·Πx_i)
    ///
    /// For 2-token pools, this simplifies to finding y such that:
    /// Amp * (x + y) + D = Amp * D + D² / (x * y)
    ///
    /// Uses Newton-Raphson iteration to solve for the output amount.
    pub fn get_amount_out_stable(&self, amount_in: U256, reserve_in: U256, reserve_out: U256) -> U256 {
        if amount_in.is_zero() || reserve_in.is_zero() || reserve_out.is_zero() {
            return U256::ZERO;
        }

        // For Aerodrome V2 stable pools, use A = 100 (typical for 2-coin pools)
        let amp = U256::from(100u64);
        let precision = U256::from(1_000_000_000_000_000_000u64); // 1e18
        let a_precision = U256::from(100u64);

        let n_coins = U256::from(2u64);
        let ann = amp * n_coins; // A * n^n

        // Normalize reserves to 18 decimals for calculation
        // Reserve values may have different decimals, so we work with them directly
        let balances = [reserve_in, reserve_out];

        // Calculate D invariant
        let d = match self.get_d_stable(&balances, ann, precision, a_precision, n_coins) {
            Some(d) => d,
            None => return U256::ZERO,
        };

        // Apply fee to input amount
        let fee_multiplier = U256::from(100000u64) - U256::from(self.fee);
        let dx = amount_in * fee_multiplier / U256::from(100000u64);

        let x = reserve_in + dx;

        // Calculate new y using get_y
        let y = match self.get_y_stable(&balances, d, ann, x, precision, a_precision, n_coins) {
            Some(y) => y,
            None => return U256::ZERO,
        };

        // dy = old_y - new_y - 1 (rounding protection)
        if reserve_out <= y {
            return U256::ZERO;
        }
        let dy = reserve_out - y;
        if dy.is_zero() {
            return U256::ZERO;
        }
        let dy = dy - U256::from(1);

        dy
    }

    /// Calculate D invariant for StableSwap (Newton-Raphson)
    fn get_d_stable(
        &self,
        balances: &[U256; 2],
        ann: U256,
        precision: U256,
        a_precision: U256,
        n_coins: U256,
    ) -> Option<U256> {
        let s = balances[0] + balances[1];
        if s.is_zero() {
            return Some(U256::ZERO);
        }

        let mut d = s;

        for _ in 0..255 {
            // D_P = D^(n+1) / (n^n * prod(x_i))
            // For 2 coins: D_P = D³ / (4 * x0 * x1)
            let d_p = if balances[0].is_zero() || balances[1].is_zero() {
                return None;
            } else {
                let d_squared = d * d;
                let divisor = balances[0] * balances[1] * n_coins;
                if divisor.is_zero() {
                    return None;
                }
                d_squared * d / divisor
            };

            let d_prev = d;

            // d = (Ann * S / A_PRECISION + D_P * n) * D / ((Ann - A_PRECISION) * D / A_PRECISION + (n + 1) * D_P)
            let numerator = (ann * s / a_precision + d_p * n_coins) * d;
            let denominator = ((ann - a_precision) * d / a_precision) + (n_coins + U256::from(1)) * d_p;

            if denominator.is_zero() {
                return None;
            }
            d = numerator / denominator;

            // Convergence check
            let diff = if d > d_prev { d - d_prev } else { d_prev - d };
            if diff <= U256::from(1) {
                return Some(d);
            }
        }

        None
    }

    /// Calculate y (new balance of output token) for StableSwap
    fn get_y_stable(
        &self,
        balances: &[U256; 2],
        d: U256,
        ann: U256,
        x: U256,
        precision: U256,
        a_precision: U256,
        n_coins: U256,
    ) -> Option<U256> {
        // For 2-token pool swapping token 0 -> token 1:
        // c = D³ / (4 * x * n)
        // s = x (sum of all balances except output token)
        // y = (y² + c) / (2y + b - D) where b = s + D * A_PRECISION / Ann

        if ann.is_zero() {
            return None;
        }

        // c = D^(n+1) / (n^n * prod(x_k for k != j))
        // For 2 coins: c = D³ / (4 * x * 2) = D³ / (8x)
        // Actually: c = D * D / (n_coins * x) * D * A_PRECISION / (Ann * n_coins)
        // Simplified: c = D³ * A_PRECISION / (4 * Ann * x)
        let c = {
            let d_squared = d * d;
            let numerator = d_squared * d * a_precision;
            let divisor = x * ann * n_coins;
            if divisor.is_zero() {
                return None;
            }
            numerator / divisor
        };

        let s = x; // sum of balances except output token (which is just x for 2-token pool)
        let b = s + d * a_precision / ann;

        let mut y = d;

        for _ in 0..255 {
            let y_prev = y;

            // y = (y² + c) / (2y + b - d)
            let numerator = y * y + c;
            let denominator = y * U256::from(2) + b - d;

            if denominator.is_zero() {
                return None;
            }
            y = numerator / denominator;

            let diff = if y > y_prev { y - y_prev } else { y_prev - y };
            if diff <= U256::from(1) {
                return Some(y);
            }
        }

        None
    }

    /// Get amount out based on pool type (volatile or stable)
    pub fn get_amount_out(&self, amount_in: U256, reserve_in: U256, reserve_out: U256) -> U256 {
        if self.stable {
            self.get_amount_out_stable(amount_in, reserve_in, reserve_out)
        } else {
            self.get_amount_out_volatile(amount_in, reserve_in, reserve_out)
        }
    }

    /// Generate calldata for a swap operation on this pool.
    ///
    /// # Arguments
    ///
    /// * `amount_0_out` - Amount of token0 to receive
    /// * `amount_1_out` - Amount of token1 to receive
    /// * `to` - Recipient address
    /// * `calldata` - Additional data for callback (e.g., flash loan)
    ///
    /// # Returns
    ///
    /// Encoded calldata for the swap function call
    pub fn swap_calldata(
        &self,
        amount_0_out: U256,
        amount_1_out: U256,
        to: Address,
        calldata: Vec<u8>,
    ) -> alloy::primitives::Bytes {
        // Use alloy's SolType to encode the swap call
        // function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data)
        let call = IAerodromeV2Pool::swapCall {
            amount0Out: amount_0_out,
            amount1Out: amount_1_out,
            to,
            data: calldata.into(),
        };
        call.abi_encode().into()
    }
}

// ============================================================================
// AutomatedMarketMaker Trait Implementation
// ============================================================================

impl AutomatedMarketMaker for AerodromeV2Pool {
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
        vec![IAerodromeV2Pool::Sync::SIGNATURE_HASH]
    }

    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError> {
        let signature = log.topics()[0];
        if signature == IAerodromeV2Pool::Sync::SIGNATURE_HASH {
            let sync_event = IAerodromeV2Pool::Sync::decode_log(&log.inner)?;

            let (reserve_0, reserve_1) = (
                sync_event.reserve0.to::<u128>(),
                sync_event.reserve1.to::<u128>(),
            );

            tracing::info!(
                target = "amms::aerodrome_v2::sync",
                block_number = ?log.block_number,
                address = ?self.address,
                stable = self.stable,
                reserve_0, reserve_1,
                "Sync"
            );

            self.reserve_0 = reserve_0;
            self.reserve_1 = reserve_1;

            // Update cached prices
            if let Ok(p) = self.calculate_price(self.token_a.address, self.token_b.address) {
                self.token_a_price = p;
                if p != 0.0 {
                    self.token_b_price = 1.0 / p;
                } else {
                    self.token_b_price = 0.0;
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
        if self.token_a.address == base_token {
            Ok(self.get_amount_out(
                amount_in,
                U256::from(self.reserve_0),
                U256::from(self.reserve_1),
            ))
        } else {
            Ok(self.get_amount_out(
                amount_in,
                U256::from(self.reserve_1),
                U256::from(self.reserve_0),
            ))
        }
    }

    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        _quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        if self.token_a.address == base_token {
            let amount_out = self.get_amount_out(
                amount_in,
                U256::from(self.reserve_0),
                U256::from(self.reserve_1),
            );

            self.reserve_0 += amount_in.to::<u128>();
            self.reserve_1 -= amount_out.to::<u128>();

            Ok(amount_out)
        } else {
            let amount_out = self.get_amount_out(
                amount_in,
                U256::from(self.reserve_1),
                U256::from(self.reserve_0),
            );

            self.reserve_0 -= amount_out.to::<u128>();
            self.reserve_1 += amount_in.to::<u128>();

            Ok(amount_out)
        }
    }

    fn tokens(&self) -> Vec<Address> {
        vec![self.token_a.address, self.token_b.address]
    }

    fn calculate_price(&self, base_token: Address, _quote_token: Address) -> Result<f64, AMMError> {
        if self.reserve_0 < MIN_POOL_RESERVE || self.reserve_1 < MIN_POOL_RESERVE {
            return Ok(0.0);
        }

        let r0_str = self.reserve_0.to_string();
        let r0_val = Float::parse_radix(&r0_str, 10)
            .map_err(|e| AMMError::Msg(format!("Float parse error: {}", e)))?;
        let r0 = Float::with_val(MPFR_T_PRECISION, r0_val);

        let r1_str = self.reserve_1.to_string();
        let r1_val = Float::parse_radix(&r1_str, 10)
            .map_err(|e| AMMError::Msg(format!("Float parse error: {}", e)))?;
        let r1 = Float::with_val(MPFR_T_PRECISION, r1_val);

        let shift = self.token_a.decimals as i32 - self.token_b.decimals as i32;
        let scale_factor = Float::with_val(MPFR_T_PRECISION, 10).pow(shift);

        let price_a: Float = (r1 / r0) * scale_factor;
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
        // Validate both tokens are in the pool
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
            return Err(AMMError::Msg("base and quote tokens are the same".to_string()));
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
        self.token_a.has_sufficient_liquidity(self.reserve_0)
            && self.token_b.has_sufficient_liquidity(self.reserve_1)
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
        let pool = IAerodromeV2Pool::new(self.address, provider.clone());

        // Fetch pool metadata to get stable flag
        let metadata = pool.metadata().call().block(block_number).await?;
        self.stable = metadata.st;

        // Fetch tokens
        self.token_a = Token::new(pool.token0().call().await?, provider.clone()).await?;
        self.token_b = Token::new(pool.token1().call().await?, provider.clone()).await?;

        // Fetch reserves
        let reserves = pool.getReserves().call().await?;
        self.reserve_0 = reserves.reserve0.to::<u128>();
        self.reserve_1 = reserves.reserve1.to::<u128>();

        // Set default fee for Aerodrome V2
        // Volatile pools: 0.05% (500 in hundredths of a bip)
        // Stable pools: 0.01% (100 in hundredths of a bip)
        // Note: These can be overridden by governance, but these are the standard defaults
        self.fee = if self.stable { 100 } else { 500 };

        tracing::trace!(
            target = "amms::aerodrome_v2::init",
            stable = self.stable,
            fee = self.fee,
            "Set pool fee"
        );

        // Update cached prices
        if let Ok(p) = self.calculate_price(self.token_a.address, self.token_b.address) {
            self.token_a_price = p;
            if p != 0.0 {
                self.token_b_price = 1.0 / p;
            }
        }

        tracing::info!(
            target = "amms::aerodrome_v2::init",
            address = ?self.address,
            stable = self.stable,
            token_a = ?self.token_a.address,
            token_b = ?self.token_b.address,
            reserve_0 = self.reserve_0,
            reserve_1 = self.reserve_1,
            "Initialized Aerodrome V2 pool"
        );

        Ok(self)
    }
}

// ============================================================================
// Factory Implementation
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct AerodromeV2Factory {
    pub address: Address,
    pub creation_block: u64,
}

impl AerodromeV2Factory {
    pub fn new(address: Address, creation_block: u64) -> Self {
        Self {
            address,
            creation_block,
        }
    }
}

impl AutomatedMarketMakerFactory for AerodromeV2Factory {
    type PoolVariant = AerodromeV2Pool;

    fn address(&self) -> Address {
        self.address
    }

    fn creation_block(&self) -> u64 {
        self.creation_block
    }

    fn pool_creation_event(&self) -> alloy::primitives::FixedBytes<32> {
        IAerodromeV2Factory::PoolCreated::SIGNATURE_HASH
    }

    fn create_pool(&self, log: Log) -> Result<AMM, AMMError> {
        let event = IAerodromeV2Factory::PoolCreated::decode_log(&log.inner)?;
        Ok(AMM::AerodromeV2Pool(AerodromeV2Pool {
            address: event.pool,
            token_a: event.token0.into(),
            token_b: event.token1.into(),
            stable: event.stable,
            ..Default::default()
        }))
    }
}

impl DiscoverySync for AerodromeV2Factory {
    fn discover<N, P>(
        &self,
        _to_block: BlockId,
        provider: P,
    ) -> impl std::future::Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: alloy::network::Network,
        P: Provider<N> + Clone,
    {
        async move {
            use alloy::rpc::types::BlockNumberOrTag;
            let from_block: BlockNumberOrTag = self.creation_block.into();

            let filter = Filter::new()
                .address(self.address)
                .event_signature(IAerodromeV2Factory::PoolCreated::SIGNATURE_HASH)
                .from_block(from_block);

            let logs = provider.get_logs(&filter).await?;

            let pools: Vec<AMM> = logs
                .into_iter()
                .filter_map(|log| {
                    if let Ok(event) = IAerodromeV2Factory::PoolCreated::decode_log(&log.inner) {
                        Some(AMM::AerodromeV2Pool(AerodromeV2Pool {
                            address: event.pool,
                            token_a: event.token0.into(),
                            token_b: event.token1.into(),
                            stable: event.stable,
                            ..Default::default()
                        }))
                    } else {
                        None
                    }
                })
                .collect();

            tracing::info!(
                target = "amms::aerodrome_v2::discover",
                factory = ?self.address,
                pool_count = pools.len(),
                "Discovered Aerodrome V2 pools"
            );

            Ok(pools)
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
        async move {
            Self::sync_all_pools(amms, to_block, provider).await
        }
    }
}

impl AerodromeV2Factory {
    /// Batch initialize Aerodrome V2 pools using a batch contract call.
    ///
    /// This method fetches all necessary pool data (tokens, reserves, decimals, stable flag)
    /// in a single batched contract call for improved efficiency.
    pub async fn init_batch<N, P>(
        amms: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: alloy::network::Network,
        P: Provider<N> + Clone,
    {
        if amms.is_empty() {
            return Ok(amms);
        }

        let step = 120; // Max pools per batch call

        let mut futures = FuturesUnordered::new();
        let pool_addresses: Vec<Vec<Address>> = amms
            .chunks(step)
            .map(|chunk| chunk.iter().map(|amm| amm.address()).collect())
            .collect();

        for group in pool_addresses {
            let provider = provider.clone();

            futures.push(async move {
                let result = IGetAerodromeV2PoolDataBatchRequestInstance::deploy_builder(provider, group.clone())
                    .call_raw()
                    .block(block_number)
                    .await?;

                Ok::<(Vec<Address>, alloy::primitives::Bytes), AMMError>((group, result))
            });
        }

        let mut amms_map: HashMap<Address, AMM> = amms
            .into_iter()
            .map(|amm| (amm.address(), amm))
            .collect();

        while let Some(res) = futures.next().await {
            let (group, return_data) = res?;

            // Decode return data: (address tokenA, address tokenB, uint112 reserve0, uint112 reserve1, uint8 decimals0, uint8 decimals1, bool stable)[]
            let return_data =
                <Vec<(Address, Address, u128, u128, u32, u32, bool)> as SolValue>::abi_decode(&return_data)?;

            for (pool_data, pool_address) in return_data.iter().zip(group.iter()) {
                // If tokenA is zero, the pool data was not populated
                if pool_data.0.is_zero() {
                    continue;
                }

                if let Some(amm) = amms_map.get_mut(pool_address) {
                    let AMM::AerodromeV2Pool(pool) = amm else {
                        continue;
                    };

                    let (token_a, token_b, reserve_0, reserve_1, decimals_a, decimals_b, stable) = pool_data;

                    // Validate decimals (u32 from SolValue, convert to u8)
                    let decimals_a = *decimals_a as u8;
                    let decimals_b = *decimals_b as u8;
                    if decimals_a == 0 || decimals_b == 0 {
                        tracing::warn!(
                            target = "amms::aerodrome_v2::init_batch",
                            ?pool_address,
                            decimals_a,
                            decimals_b,
                            "Skipping pool with invalid decimals"
                        );
                        continue;
                    }

                    pool.token_a = Token::new_with_decimals(*token_a, decimals_a);
                    pool.token_b = Token::new_with_decimals(*token_b, decimals_b);
                    pool.reserve_0 = *reserve_0;
                    pool.reserve_1 = *reserve_1;
                    pool.stable = *stable;

                    // Set default fee if not set (0.3% = 3000 in hundredths of a bip)
                    if pool.fee == 0 {
                        pool.fee = 3000;
                    }

                    // Update cached prices
                    if let Ok(p) = pool.calculate_price(pool.token_a.address, pool.token_b.address) {
                        pool.token_a_price = p;
                        pool.token_b_price = if p != 0.0 { 1.0 / p } else { 0.0 };
                    }

                    tracing::trace!(
                        target = "amms::aerodrome_v2::init_batch",
                        ?pool_address,
                        stable,
                        ?token_a,
                        ?token_b,
                        reserve_0,
                        reserve_1,
                        "Initialized pool"
                    );
                }
            }
        }

        // Filter out pools with invalid data
        let (valid_amms, invalid_amms): (Vec<_>, Vec<_>) = amms_map
            .into_iter()
            .partition(|(_, amm)| !amm.tokens().iter().any(|t| t.is_zero()));

        if !invalid_amms.is_empty() {
            for (addr, _) in &invalid_amms {
                tracing::warn!(
                    target = "amms::aerodrome_v2::init_batch",
                    ?addr,
                    "Filtered out invalid pool"
                );
            }
        }

        tracing::info!(
            target = "amms::aerodrome_v2::init_batch",
            total = valid_amms.len() + invalid_amms.len(),
            valid = valid_amms.len(),
            invalid = invalid_amms.len(),
            "Batch initialization complete"
        );

        Ok(valid_amms.into_iter().map(|(_, amm)| amm).collect())
    }

    /// Batch sync Aerodrome V2 pools by fetching their current reserves.
    ///
    /// This method uses the batch contract to efficiently fetch reserves for multiple pools.
    pub async fn sync_all_pools<N, P>(
        amms: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: alloy::network::Network,
        P: Provider<N> + Clone,
    {
        if amms.is_empty() {
            return Ok(amms);
        }

        let step = 120;

        let mut futures = FuturesUnordered::new();
        let pool_addresses: Vec<Vec<Address>> = amms
            .chunks(step)
            .map(|chunk| chunk.iter().map(|amm| amm.address()).collect())
            .collect();

        for group in pool_addresses {
            let provider = provider.clone();

            futures.push(async move {
                let result = IGetAerodromeV2PoolDataBatchRequestInstance::deploy_builder(provider, group.clone())
                    .call_raw()
                    .block(block_number)
                    .await?;

                Ok::<(Vec<Address>, alloy::primitives::Bytes), AMMError>((group, result))
            });
        }

        let mut amms_map: HashMap<Address, AMM> = amms
            .into_iter()
            .map(|amm| (amm.address(), amm))
            .collect();

        while let Some(res) = futures.next().await {
            let (group, return_data) = res?;

            let return_data =
                <Vec<(Address, Address, u128, u128, u32, u32, bool)> as SolValue>::abi_decode(&return_data)?;

            for (pool_data, pool_address) in return_data.iter().zip(group.iter()) {
                if pool_data.0.is_zero() {
                    continue;
                }

                if let Some(amm) = amms_map.get_mut(pool_address) {
                    let AMM::AerodromeV2Pool(pool) = amm else {
                        continue;
                    };

                    let (_, _, reserve_0, reserve_1, _, _, stable) = pool_data;

                    pool.reserve_0 = *reserve_0;
                    pool.reserve_1 = *reserve_1;
                    pool.stable = *stable;

                    // Update cached prices
                    if let Ok(p) = pool.calculate_price(pool.token_a.address, pool.token_b.address) {
                        pool.token_a_price = p;
                        pool.token_b_price = if p != 0.0 { 1.0 / p } else { 0.0 };
                    }
                }
            }
        }

        Ok(amms_map.into_iter().map(|(_, amm)| amm).collect())
    }
}

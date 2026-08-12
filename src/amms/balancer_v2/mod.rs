use crate::amms::{
    amm::{AutomatedMarketMaker, SyncAction, AMM},
    consts::BONE,
    error::AMMError,
};
use alloy::primitives::address;
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, B256, U256},
    providers::Provider,
    rpc::types::Log,
    sol_types::SolEvent,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;

pub fn get_vault_address(_chain_id: u64) -> Option<Address> {
    // Balancer V2 Vault is deployed using Create2 at the same address on all supported chains.
    // Ref: https://docs-v2.balancer.fi/reference/contracts/deployment-addresses/
    Some(address!("BA12222222228d8Ba445958a75a0704d566BF2C8"))
}

/// Returns the list of chain IDs where Balancer V2 is deployed
pub fn get_supported_chains() -> Vec<u64> {
    vec![
        1,     // Ethereum
        137,   // Polygon
        42161, // Arbitrum
        10,    // Optimism
        8453,  // Base
        43114, // Avalanche
        100,   // Gnosis
        56,    // BSC
        250,   // Fantom (deprecated but historical data exists)
    ]
}

pub mod abi;
pub mod factory;
pub mod math;

use self::abi::{IRateProvider, IVault};
use alloy::sol;

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    GetBalancerV2RatesBatchRequest,
    "src/amms/abi/GetBalancerV2RatesBatchRequest.json"
);
use alloy::sol_types::SolType;

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BalancerV2Error {
    #[error("Token In Does Not Exist")]
    TokenInDoesNotExist,
    #[error("Token Out Does Not Exist")]
    TokenOutDoesNotExist,
    #[error("Initialization Error")]
    InitializationError,
    #[error("Add Overflow")]
    AddOverflow,
    #[error("Sub Underflow")]
    SubUnderflow,
    #[error("Mul Overflow")]
    MulOverflow,
    #[error("Div Zero")]
    DivZero,
    #[error("Div Internal")]
    DivInternal,
    #[error("Not Supported: {0}")]
    NotSupported(String),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum BalancerV2PoolType {
    Weighted,
    Stable,
    ComposableStable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenState {
    pub address: Address,
    pub balance: U256,
    pub decimals: u8,
    pub weight: Option<U256>,
    pub rate_provider: Option<Address>,
    pub rate: Option<U256>,
    pub scaling_factor: Option<U256>,
    pub index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AmpState {
    pub initial_value: U256,
    pub end_value: U256,
    pub start_time: U256,
    pub end_time: U256,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalancerV2Factory {
    pub address: Address,
    pub vault_address: Address,
    pub creation_block: u64,
    pub pool_type: BalancerV2PoolType,
}

impl BalancerV2Factory {
    pub fn new(
        address: Address,
        vault_address: Address,
        creation_block: u64,
        pool_type: BalancerV2PoolType,
    ) -> Self {
        Self {
            address,
            vault_address,
            creation_block,
            pool_type,
        }
    }

    pub fn address(&self) -> Address {
        self.address
    }

    pub fn pool_creation_event(&self) -> B256 {
        B256::from_str("0x83a48fbcfc991335314e74d0496aab6a1987e992ddc85dddbcc4d6dd6ef2e9fc")
            .unwrap()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalancerV2Pool {
    pub address: Address,
    pub last_synced_block: u64,
    pub pool_id: B256,
    pub pool_type: BalancerV2PoolType,
    pub vault_address: Address,
    pub tokens: HashMap<Address, TokenState>,
    pub token_list: Vec<Address>,
    pub swap_fee: U256,
    pub amp_state: Option<AmpState>,
    pub bpt_index: Option<usize>,
    #[serde(skip)]
    pub spot_prices: std::collections::HashMap<(Address, Address), f64>,
}

impl BalancerV2Pool {
    pub fn new(
        address: Address,
        vault_address: Address,
        pool_id: B256,
        pool_type: BalancerV2PoolType,
    ) -> Self {
        Self {
            address,
            last_synced_block: 0,
            pool_id,
            pool_type,
            vault_address,
            tokens: HashMap::new(),
            token_list: Vec::new(),
            swap_fee: U256::ZERO,
            amp_state: None,

            bpt_index: None,
            spot_prices: std::collections::HashMap::new(),
        }
    }

    pub fn get_current_amp(&self, block_timestamp: u64) -> Option<U256> {
        let state = self.amp_state.as_ref()?;
        let ts = U256::from(block_timestamp);

        if ts >= state.end_time {
            return Some(state.end_value);
        }
        if ts <= state.start_time {
            return Some(state.initial_value);
        }

        let total_duration = state.end_time - state.start_time;
        let elapsed = ts - state.start_time;

        if state.end_value >= state.initial_value {
            let delta = state.end_value - state.initial_value;
            Some(state.initial_value + (delta * elapsed) / total_duration)
        } else {
            let delta = state.initial_value - state.end_value;
            Some(state.initial_value - (delta * elapsed) / total_duration)
        }
    }

    fn ceil_div_u256(numerator: U256, denominator: U256) -> Result<U256, AMMError> {
        if denominator.is_zero() {
            return Err(AMMError::ArithmeticError);
        }
        let q = numerator / denominator;
        let r = numerator % denominator;
        if r.is_zero() {
            Ok(q)
        } else {
            q.checked_add(U256::from(1u8))
                .ok_or(AMMError::ArithmeticError)
        }
    }

    fn normalize_exact_out_amount_in(
        &self,
        base_token: Address,
        quote_token: Address,
        amount_out: U256,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        // NOTE:
        // Use bounded-search normalization (instead of +/-1 linear loops) to avoid pathological
        // long runtimes when the initial exact-out estimate is far from the true minimum.
        let one = U256::from(1u8);
        let mut hi = if amount_in.is_zero() { one } else { amount_in };

        // 1) Ensure upper bound: out(hi) >= target.
        let mut out_hi = self.simulate_swap(base_token, quote_token, hi)?;
        let mut expand_iters = 0u16;
        while out_hi < amount_out {
            hi = hi
                .checked_mul(U256::from(2u8))
                .ok_or(AMMError::ArithmeticError)?;
            out_hi = self.simulate_swap(base_token, quote_token, hi)?;
            expand_iters = expand_iters.saturating_add(1);
            if expand_iters > 256 {
                return Err(AMMError::Msg(
                    "normalize_exact_out failed to find upper bound".to_string(),
                ));
            }
        }

        // 2) Binary search minimal feasible input in [0, hi].
        let mut lo = U256::ZERO;
        while lo.checked_add(one).ok_or(AMMError::ArithmeticError)? < hi {
            let mid = lo
                .checked_add((hi - lo) / U256::from(2u8))
                .ok_or(AMMError::ArithmeticError)?;
            let out_mid = self.simulate_swap(base_token, quote_token, mid)?;
            if out_mid >= amount_out {
                hi = mid;
            } else {
                lo = mid;
            }
        }

        Ok(hi)
    }

    fn add_swap_fee_amount(&self, amount: U256) -> Result<U256, AMMError> {
        let fee_complement = BONE
            .checked_sub(self.swap_fee)
            .ok_or(AMMError::ArithmeticError)?;
        BalancerV2Pool::ceil_div_u256(
            amount.checked_mul(BONE).ok_or(AMMError::ArithmeticError)?,
            fee_complement,
        )
    }

    fn subtract_swap_fee_amount(&self, amount: U256) -> Result<U256, AMMError> {
        let fee_amount = BalancerV2Pool::ceil_div_u256(
            amount
                .checked_mul(self.swap_fee)
                .ok_or(AMMError::ArithmeticError)?,
            BONE,
        )?;
        amount
            .checked_sub(fee_amount)
            .ok_or(AMMError::ArithmeticError)
    }

    fn token_scaling_factor(state: &TokenState) -> Result<U256, AMMError> {
        // Prefer on-chain pool scaling factor when available. This is the exact value
        // used by pool math (already includes decimal/rate handling per pool flavor),
        // and avoids local re-derivation drift.
        if let Some(sf) = state.scaling_factor {
            return Ok(sf);
        }

        // NOTE(drift-gap): This currently derives scaling factor from decimals * `state.rate`.
        // For Balancer V2 pools with internal rate-cache semantics (e.g. Meta/Composable stable families),
        // chain `querySwap` may use cached pool rates that are conditionally refreshed by timestamp.
        // Until that cache state machine (current/old/duration/expires + refresh condition) is fully mirrored
        // in local state, tiny same-block quote differences can remain.
        //
        // IMPORTANT:
        // BasePool._computeScalingFactor(token) = 1e18 * 10^(18-decimals),
        // and ComposableStable scalingFactors[i] = _getScalingFactor(i).mulDown(rate)
        // => effective scaling factor = 10^(18-decimals) * rate.
        let dec_scale = U256::from(10).pow(U256::from(18u8.saturating_sub(state.decimals)));
        let rate = state.rate.unwrap_or(U256::from(10).pow(U256::from(18)));
        dec_scale.checked_mul(rate).ok_or(AMMError::ArithmeticError)
    }

    #[inline]
    fn token_scaling_factor_from_rate(state: &TokenState) -> Result<U256, AMMError> {
        let dec_scale = U256::from(10).pow(U256::from(18u8.saturating_sub(state.decimals)));
        let rate = state.rate.unwrap_or(U256::from(10).pow(U256::from(18)));
        dec_scale.checked_mul(rate).ok_or(AMMError::ArithmeticError)
    }

    #[inline]
    fn upscale_down(amount: U256, scaling_factor: U256) -> Result<U256, AMMError> {
        amount
            .checked_mul(scaling_factor)
            .ok_or(AMMError::ArithmeticError)?
            .checked_div(BONE)
            .ok_or(AMMError::ArithmeticError)
    }

    #[inline]
    fn downscale_down(amount: U256, scaling_factor: U256) -> Result<U256, AMMError> {
        amount
            .checked_mul(BONE)
            .ok_or(AMMError::ArithmeticError)?
            .checked_div(scaling_factor)
            .ok_or(AMMError::ArithmeticError)
    }

    #[inline]
    fn downscale_up(amount: U256, scaling_factor: U256) -> Result<U256, AMMError> {
        BalancerV2Pool::ceil_div_u256(
            amount.checked_mul(BONE).ok_or(AMMError::ArithmeticError)?,
            scaling_factor,
        )
    }

    pub async fn update_rates<N, P>(&mut self, provider: P) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        for token_state in self.tokens.values_mut() {
            if let Some(rate_provider_addr) = token_state.rate_provider {
                if rate_provider_addr == Address::ZERO {
                    continue;
                }

                let rate_provider = IRateProvider::new(rate_provider_addr, provider.clone());
                // Use a standard call
                match rate_provider.getRate().call().await {
                    Ok(result) => {
                        token_state.rate = Some(result);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to fetch rate for {:?}: {:?}",
                            rate_provider_addr,
                            e
                        );
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn batch_update_rates<N, P>(
        pools: &mut [BalancerV2Pool],
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        // NOTE(drift-gap): This task batch-pulls `rateProvider.getRate()`.
        // It does NOT yet synchronize pool-side token rate cache state (duration/expires/oldRate/currentRate),
        // so cache-based pools may briefly diverge from on-chain `querySwap` behavior.
        let mut rate_providers = Vec::new();
        let mut pool_token_indices = Vec::new(); // (pool_idx, token_addr)

        for (pool_idx, pool) in pools.iter().enumerate() {
            for (token_addr, state) in &pool.tokens {
                if let Some(rp) = state.rate_provider {
                    if rp != Address::ZERO {
                        rate_providers.push(rp);
                        pool_token_indices.push((pool_idx, *token_addr));
                    }
                }
            }
        }

        if rate_providers.is_empty() {
            return Ok(());
        }

        let deployer =
            GetBalancerV2RatesBatchRequest::deploy_builder(provider.clone(), rate_providers);

        let res = deployer
            .call_raw()
            .await
            .map_err(|_| AMMError::SyncError(Address::ZERO))?;

        // Decode result: uint256[]
        let rates =
            <alloy::sol_types::sol_data::Array<alloy::sol_types::sol_data::Uint<256>>>::abi_decode(
                &res,
            )
            .map_err(|_| AMMError::SyncError(Address::ZERO))?;

        // Update pools
        for (i, rate) in rates.into_iter().enumerate() {
            if i < pool_token_indices.len() {
                let (pool_idx, token_addr) = pool_token_indices[i];
                if let Some(pool) = pools.get_mut(pool_idx) {
                    if let Some(state) = pool.tokens.get_mut(&token_addr) {
                        state.rate = Some(rate);
                    }
                }
            }
        }

        Ok(())
    }
}

impl AutomatedMarketMaker for BalancerV2Pool {
    fn address(&self) -> Address {
        self.address
    }

    fn last_synced_block(&self) -> u64 {
        self.last_synced_block
    }

    fn set_last_synced_block(&mut self, block_number: u64) {
        self.last_synced_block = self.last_synced_block.max(block_number);
    }

    fn tokens(&self) -> Vec<Address> {
        self.token_list.clone()
    }

    fn has_sufficient_liquidity(&self) -> bool {
        // Simple heuristic: check if at least 2 tokens have meaningful balance
        let mut meaningful_tokens = 0;
        let mut normalized_balances: Vec<U256> = Vec::new();

        for token in self.tokens.values() {
            let reserve = token.balance;
            let decimals = token.decimals;

            // Replicate generic check from Token::has_sufficient_liquidity
            // Since we don't have symbol, we use decimal-based heuristics.
            let is_sufficient = if decimals >= 18 {
                // 0.0001 unit (e.g. 10^14 wei)
                reserve >= U256::from(10).pow(U256::from(decimals.saturating_sub(4)))
            } else if decimals >= 6 {
                // 100 units (e.g. 100 * 10^6 = 10^8)
                // Note: For USDC (6 decimals), 100 units = $100.
                let threshold =
                    U256::from(100).saturating_mul(U256::from(10).pow(U256::from(decimals)));
                reserve >= threshold
            } else {
                // Fallback
                reserve >= U256::from(100_000)
            };

            if is_sufficient {
                meaningful_tokens += 1;

                // Normalize balance to 18 decimals for ratio check
                let normalized = if decimals < 18 {
                    reserve * U256::from(10).pow(U256::from(18 - decimals))
                } else if decimals > 18 {
                    reserve / U256::from(10).pow(U256::from(decimals - 18))
                } else {
                    reserve
                };
                normalized_balances.push(normalized);
            }
        }

        if meaningful_tokens < 2 {
            return false;
        }

        // For Stable pools, check balance ratio imbalance
        // Stable pools assume pegged assets (1:1), so extreme imbalance means pool is unusable
        // Allow max 1000:1 ratio between normalized balances
        if matches!(
            self.pool_type,
            BalancerV2PoolType::Stable | BalancerV2PoolType::ComposableStable
        ) {
            let max_ratio = U256::from(1000);

            for i in 0..normalized_balances.len() {
                for j in (i + 1)..normalized_balances.len() {
                    let (larger, smaller) = if normalized_balances[i] > normalized_balances[j] {
                        (normalized_balances[i], normalized_balances[j])
                    } else {
                        (normalized_balances[j], normalized_balances[i])
                    };

                    if smaller.is_zero() {
                        return false; // One side is empty
                    }

                    let ratio = larger / smaller;
                    if ratio > max_ratio {
                        return false; // Too imbalanced
                    }
                }
            }
        }

        true
    }

    fn decimals(&self, token: Address) -> u8 {
        self.tokens.get(&token).map(|t| t.decimals).unwrap_or(0)
    }

    /// Balancer V2 is deployed on multiple EVM-compatible chains
    fn supported_chains(&self) -> Option<Vec<u64>> {
        Some(get_supported_chains())
    }

    fn spot_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
        if let Some(&price) = self.spot_prices.get(&(base_token, quote_token)) {
            // 价格有效性校验
            if price > 0.0 && price.is_finite() {
                return Ok(price);
            }
        }

        // 尝试反向查找 (quote -> base) 并取倒数，虽然 populate_pool_data 应该已经填充了双向价格
        if let Some(&inverse_price) = self.spot_prices.get(&(quote_token, base_token)) {
            if inverse_price > 0.0 && inverse_price.is_finite() {
                return Ok(1.0 / inverse_price);
            }
        }

        // 缓存未命中，回退到实时计算
        let price = self.calculate_price(base_token, quote_token)?;
        // 校验计算结果
        if price <= 0.0 || !price.is_finite() {
            return Err(AMMError::Msg("Invalid calculated spot price".to_string()));
        }
        Ok(price)
    }

    fn calculate_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
        let token_in_state = self
            .tokens
            .get(&base_token)
            .ok_or(BalancerV2Error::TokenInDoesNotExist)?;
        let token_out_state = self
            .tokens
            .get(&quote_token)
            .ok_or(BalancerV2Error::TokenOutDoesNotExist)?;

        // Scaling Factors
        let rate_in = token_in_state
            .rate
            .unwrap_or(U256::from(10).pow(U256::from(18)));
        let decimals_diff_in = 18u8.saturating_sub(token_in_state.decimals);
        let decimal_scaling_in = U256::from(10).pow(U256::from(decimals_diff_in));
        let scale_in_factor = |val: U256| -> Result<U256, AMMError> {
            val.checked_mul(decimal_scaling_in)
                .ok_or(BalancerV2Error::MulOverflow)?
                .checked_mul(rate_in)
                .ok_or(BalancerV2Error::MulOverflow)?
                .checked_div(U256::from(10).pow(U256::from(18)))
                .ok_or(BalancerV2Error::DivZero.into())
        };

        let rate_out = token_out_state
            .rate
            .unwrap_or(U256::from(10).pow(U256::from(18)));
        let decimals_diff_out = 18u8.saturating_sub(token_out_state.decimals);
        let decimal_scaling_out = U256::from(10).pow(U256::from(decimals_diff_out));
        // We don't need a closure for out if we just need the factor for price adjustment
        // But we need to scale balance_out for the math function.
        let scale_out_factor = |val: U256| -> Result<U256, AMMError> {
            val.checked_mul(decimal_scaling_out)
                .ok_or(BalancerV2Error::MulOverflow)?
                .checked_mul(rate_out)
                .ok_or(BalancerV2Error::MulOverflow)?
                .checked_div(U256::from(10).pow(U256::from(18)))
                .ok_or(BalancerV2Error::DivZero.into())
        };

        match self.pool_type {
            BalancerV2PoolType::Weighted => {
                let w_in = token_in_state
                    .weight
                    .ok_or(BalancerV2Error::InitializationError)?;
                let w_out = token_out_state
                    .weight
                    .ok_or(BalancerV2Error::InitializationError)?;

                let scaled_balance_in = scale_in_factor(token_in_state.balance)?;
                let scaled_balance_out = scale_out_factor(token_out_state.balance)?;

                let price_norm = math::weighted_math::calculate_spot_price(
                    scaled_balance_in,
                    w_in,
                    scaled_balance_out,
                    w_out,
                )?;

                // NOTE: No rate adjustment needed here!
                // scale_in_factor/scale_out_factor already apply rate:
                //   scaled = balance * decimal_scaling * rate / 1e18
                // Applying rate ratio again would cause double-rate-adjustment.

                Ok(price_norm)
            }
            BalancerV2PoolType::Stable | BalancerV2PoolType::ComposableStable => {
                let amp = self
                    .amp_state
                    .as_ref()
                    .map(|s| s.end_value)
                    .ok_or(BalancerV2Error::InitializationError)?;
                let mut scaled_balances = Vec::new();
                let mut index_in = 0;
                let mut index_out = 0;
                let mut found_in = false;
                let mut found_out = false;

                // We need to iterate over all tokens to get balances in order
                for (i, token_addr) in self.token_list.iter().enumerate() {
                    // Check if BPT needs to be skipped (only for Composable?)
                    if let Some(bpt_idx) = self.bpt_index {
                        if i == bpt_idx {
                            continue;
                        }
                    }

                    let token_state = self
                        .tokens
                        .get(token_addr)
                        .ok_or(BalancerV2Error::TokenInDoesNotExist)?;

                    // Scale balance
                    let decimal_scaling =
                        U256::from(10).pow(U256::from(18u8.saturating_sub(token_state.decimals)));
                    let rate = token_state
                        .rate
                        .unwrap_or(U256::from(10).pow(U256::from(18)));

                    let scaled = token_state
                        .balance
                        .checked_mul(decimal_scaling)
                        .ok_or(BalancerV2Error::MulOverflow)?
                        .checked_mul(rate)
                        .ok_or(BalancerV2Error::MulOverflow)?
                        .checked_div(U256::from(10).pow(U256::from(18)))
                        .ok_or(BalancerV2Error::DivZero)?;

                    scaled_balances.push(scaled);

                    if *token_addr == base_token {
                        index_in = scaled_balances.len() - 1;
                        found_in = true;
                    }
                    if *token_addr == quote_token {
                        index_out = scaled_balances.len() - 1;
                        found_out = true;
                    }
                }

                if !found_in || !found_out {
                    return Err(BalancerV2Error::TokenInDoesNotExist.into());
                }

                let price_norm = math::stable_math::calculate_spot_price(
                    amp,
                    &scaled_balances,
                    index_in,
                    index_out,
                )?;

                // NOTE: No rate adjustment needed here!
                // The scaling loop already applies rate:
                //   scaled = balance * decimal_scaling * rate / 1e18
                // Applying rate ratio again would cause double-rate-adjustment.

                Ok(price_norm)
            }
        }
    }

    fn simulate_swap(
        &self,
        base_token: Address,
        quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let token_in_state = self
            .tokens
            .get(&base_token)
            .ok_or(BalancerV2Error::TokenInDoesNotExist)?;
        let token_out_state = self
            .tokens
            .get(&quote_token)
            .ok_or(BalancerV2Error::TokenOutDoesNotExist)?;

        match self.pool_type {
            BalancerV2PoolType::Weighted => {
                let w_in = token_in_state
                    .weight
                    .ok_or(BalancerV2Error::InitializationError)?;
                let w_out = token_out_state
                    .weight
                    .ok_or(BalancerV2Error::InitializationError)?;

                let scaling_in = BalancerV2Pool::token_scaling_factor_from_rate(token_in_state)?;
                let scaling_out = BalancerV2Pool::token_scaling_factor_from_rate(token_out_state)?;

                let scaled_balance_in =
                    BalancerV2Pool::upscale_down(token_in_state.balance, scaling_in)?;
                let scaled_balance_out =
                    BalancerV2Pool::upscale_down(token_out_state.balance, scaling_out)?;

                // Weighted GIVEN_IN: apply fee in scaled domain to preserve sub-token precision
                // for non-18-decimal assets (closer to on-chain math behavior).
                let scaled_amount_in_raw = BalancerV2Pool::upscale_down(amount_in, scaling_in)?;
                let scaled_amount_in = self.subtract_swap_fee_amount(scaled_amount_in_raw)?;

                // Calculate Output (18 decimals)
                let scaled_amount_out = math::weighted_math::calculate_out_given_in(
                    scaled_balance_in,
                    w_in,
                    scaled_balance_out,
                    w_out,
                    scaled_amount_in,
                )?;

                // BaseMinimalSwapInfoPool GIVEN_IN: token out is downscaled with divDown.
                let amount_out = BalancerV2Pool::downscale_down(scaled_amount_out, scaling_out)?;

                Ok(amount_out)
            }
            BalancerV2PoolType::Stable | BalancerV2PoolType::ComposableStable => {
                let amp = self
                    .amp_state
                    .as_ref()
                    .map(|s| s.end_value)
                    .ok_or(BalancerV2Error::InitializationError)?;
                let one_e18 = U256::from(10).pow(U256::from(18));

                let mut scaled_balances = Vec::new();
                let mut index_in = 0;
                let mut index_out = 0;
                let mut found_in = false;
                let mut found_out = false;

                for (i, token_addr) in self.token_list.iter().enumerate() {
                    if let Some(bpt_idx) = self.bpt_index {
                        if i == bpt_idx {
                            if *token_addr == base_token || *token_addr == quote_token {
                                return Err(BalancerV2Error::NotSupported(
                                    "Swaps involving BPT not supported yet".to_string(),
                                )
                                .into());
                            }
                            continue;
                        }
                    }

                    let token_state = self.tokens.get(token_addr).unwrap();
                    let scaling_factor = BalancerV2Pool::token_scaling_factor(token_state)?;

                    // Upscale balance with scaling factor (mulDown).
                    let scaled_balance = token_state
                        .balance
                        .checked_mul(scaling_factor)
                        .ok_or(BalancerV2Error::MulOverflow)?
                        .checked_div(one_e18)
                        .ok_or(BalancerV2Error::DivZero)?;

                    scaled_balances.push(scaled_balance);

                    if *token_addr == base_token {
                        index_in = scaled_balances.len() - 1;
                        found_in = true;
                    }
                    if *token_addr == quote_token {
                        index_out = scaled_balances.len() - 1;
                        found_out = true;
                    }
                }

                if !found_in || !found_out {
                    return Err(BalancerV2Error::TokenInDoesNotExist.into());
                }

                // GIVEN_IN mirrors BaseGeneralPool:
                // 1) subtract fee in raw domain, 2) upscale, 3) stable math, 4) downscaleDown
                let amount_in_after_fee = self.subtract_swap_fee_amount(amount_in)?;

                // Upscale Amount In (mulDown by scaling factor)
                let token_in_state = self.tokens.get(&base_token).unwrap();
                let scaling_in = BalancerV2Pool::token_scaling_factor(token_in_state)?;

                let scaled_amount_in = amount_in_after_fee
                    .checked_mul(scaling_in)
                    .ok_or(BalancerV2Error::MulOverflow)?
                    .checked_div(one_e18)
                    .ok_or(BalancerV2Error::DivZero)?;

                let scaled_amount_out = math::stable_math::calculate_out_given_in(
                    amp,
                    &scaled_balances,
                    index_in,
                    index_out,
                    scaled_amount_in,
                )?;

                // Downscale Amount Out (divDown by scaling factor)
                let token_out_state = self.tokens.get(&quote_token).unwrap();
                let scaling_out = BalancerV2Pool::token_scaling_factor(token_out_state)?;

                let amount_out = scaled_amount_out
                    .checked_mul(one_e18)
                    .ok_or(BalancerV2Error::MulOverflow)?
                    .checked_div(scaling_out)
                    .ok_or(BalancerV2Error::DivZero)?;
                Ok(amount_out)
            }
        }
    }

    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let amount_out = self.simulate_swap(base_token, quote_token, amount_in)?;

        if let Some(t) = self.tokens.get_mut(&base_token) {
            t.balance = t
                .balance
                .checked_add(amount_in)
                .ok_or(AMMError::Msg("BalancerV2 balance add overflow".into()))?;
        }
        if let Some(t) = self.tokens.get_mut(&quote_token) {
            t.balance = t
                .balance
                .checked_sub(amount_out)
                .ok_or(AMMError::Msg("BalancerV2 balance sub underflow".into()))?;
        }

        // 刷新 spot price 缓存（balances 已更新为 swap 后状态）
        self.update_spot_prices();

        Ok(amount_out)
    }

    fn simulate_swap_exact_out(
        &self,
        base_token: Address,
        quote_token: Address,
        amount_out: U256,
    ) -> Result<U256, AMMError> {
        if amount_out.is_zero() {
            return Ok(U256::ZERO);
        }

        let token_in_state = self
            .tokens
            .get(&base_token)
            .ok_or(BalancerV2Error::TokenInDoesNotExist)?;
        let token_out_state = self
            .tokens
            .get(&quote_token)
            .ok_or(BalancerV2Error::TokenOutDoesNotExist)?;
        if amount_out >= token_out_state.balance {
            return Err(AMMError::Msg(
                "insufficient liquidity for BalancerV2 exact out".to_string(),
            ));
        }

        match self.pool_type {
            BalancerV2PoolType::Weighted => {
                let w_in = token_in_state
                    .weight
                    .ok_or(BalancerV2Error::InitializationError)?;
                let w_out = token_out_state
                    .weight
                    .ok_or(BalancerV2Error::InitializationError)?;

                let scaling_in = BalancerV2Pool::token_scaling_factor_from_rate(token_in_state)?;
                let scaling_out = BalancerV2Pool::token_scaling_factor_from_rate(token_out_state)?;

                let scaled_balance_in =
                    BalancerV2Pool::upscale_down(token_in_state.balance, scaling_in)?;
                let scaled_balance_out =
                    BalancerV2Pool::upscale_down(token_out_state.balance, scaling_out)?;

                // BaseMinimalSwapInfoPool GIVEN_OUT: target out is upscaled with mulDown.
                let scaled_amount_out = BalancerV2Pool::upscale_down(amount_out, scaling_out)?;

                let scaled_amount_in = math::weighted_math::calculate_in_given_out(
                    scaled_balance_in,
                    w_in,
                    scaled_balance_out,
                    w_out,
                    scaled_amount_out,
                )?;

                // Weighted GIVEN_OUT: add fee in scaled domain first, then downscale up.
                let scaled_in_with_fee = self.add_swap_fee_amount(scaled_amount_in)?;
                BalancerV2Pool::downscale_up(scaled_in_with_fee, scaling_in)
            }
            BalancerV2PoolType::Stable | BalancerV2PoolType::ComposableStable => {
                let amp = self
                    .amp_state
                    .as_ref()
                    .map(|s| s.end_value)
                    .ok_or(BalancerV2Error::InitializationError)?;

                let one_e18 = U256::from(10).pow(U256::from(18));
                let mut scaled_balances = Vec::new();
                let mut index_in = 0usize;
                let mut index_out = 0usize;
                let mut found_in = false;
                let mut found_out = false;

                for (i, token_addr) in self.token_list.iter().enumerate() {
                    if let Some(bpt_idx) = self.bpt_index {
                        if i == bpt_idx {
                            if *token_addr == base_token || *token_addr == quote_token {
                                return Err(BalancerV2Error::NotSupported(
                                    "Swaps involving BPT not supported yet".to_string(),
                                )
                                .into());
                            }
                            continue;
                        }
                    }

                    let state = self
                        .tokens
                        .get(token_addr)
                        .ok_or(BalancerV2Error::InitializationError)?;
                    let scaling_factor = BalancerV2Pool::token_scaling_factor(state)?;
                    let scaled = state
                        .balance
                        .checked_mul(scaling_factor)
                        .ok_or(BalancerV2Error::MulOverflow)?
                        .checked_div(one_e18)
                        .ok_or(BalancerV2Error::DivZero)?;
                    scaled_balances.push(scaled);

                    if *token_addr == base_token {
                        index_in = scaled_balances.len() - 1;
                        found_in = true;
                    }
                    if *token_addr == quote_token {
                        index_out = scaled_balances.len() - 1;
                        found_out = true;
                    }
                }

                if !found_in || !found_out {
                    return Err(BalancerV2Error::TokenInDoesNotExist.into());
                }

                let scaling_out = BalancerV2Pool::token_scaling_factor(token_out_state)?;
                let scaled_amount_out = amount_out
                    .checked_mul(scaling_out)
                    .ok_or(BalancerV2Error::MulOverflow)?
                    .checked_div(one_e18)
                    .ok_or(BalancerV2Error::DivZero)?;

                let scaled_amount_in = math::stable_math::calculate_in_given_out(
                    amp,
                    &scaled_balances,
                    index_in,
                    index_out,
                    scaled_amount_out,
                )?;

                let scaling_in = BalancerV2Pool::token_scaling_factor(token_in_state)?;

                let numerator = scaled_amount_in
                    .checked_mul(one_e18)
                    .ok_or(BalancerV2Error::MulOverflow)?;
                let denominator = scaling_in;
                let raw_in_without_fee = BalancerV2Pool::ceil_div_u256(numerator, denominator)?;
                let raw_in = self.add_swap_fee_amount(raw_in_without_fee)?;
                self.normalize_exact_out_amount_in(base_token, quote_token, amount_out, raw_in)
            }
        }
    }

    fn sync_events(&self) -> Vec<B256> {
        // NOTE(drift-gap): We currently sync Vault-side balance events only.
        // Pool-side rate-cache update events are not yet part of this stream.
        vec![
            IVault::Swap::SIGNATURE_HASH,
            IVault::PoolBalanceChanged::SIGNATURE_HASH,
            IVault::PoolBalanceManaged::SIGNATURE_HASH,
        ]
    }

    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError> {
        let topic0 = log.topics()[0];

        if topic0 == IVault::Swap::SIGNATURE_HASH {
            let event = IVault::Swap::decode_log(&log.inner)?;
            if event.poolId != self.pool_id {
                return Ok(SyncAction::None);
            }

            if let Some(t) = self.tokens.get_mut(&event.tokenIn) {
                t.balance += event.amountIn;
            }
            if let Some(t) = self.tokens.get_mut(&event.tokenOut) {
                t.balance -= event.amountOut;
            }

            tracing::info!(
                target = "amms::balancer_v2::sync",
                block_number = ?log.block_number,
                pool_id = ?self.pool_id,
                token_in = ?event.tokenIn,
                token_out = ?event.tokenOut,
                amount_in = ?event.amountIn,
                amount_out = ?event.amountOut,
                "Swap"
            );
        } else if topic0 == IVault::PoolBalanceChanged::SIGNATURE_HASH {
            let event = IVault::PoolBalanceChanged::decode_log(&log.inner)?;
            if event.poolId != self.pool_id {
                return Ok(SyncAction::None);
            }

            for (i, &token) in event.tokens.iter().enumerate() {
                if let Some(t) = self.tokens.get_mut(&token) {
                    let delta = event.deltas[i];
                    if delta.is_positive() {
                        t.balance += U256::try_from(delta).unwrap();
                    } else {
                        let abs_delta = U256::try_from(-delta).unwrap();
                        t.balance -= abs_delta;
                    }
                }
            }

            tracing::info!(
                target = "amms::balancer_v2::sync",
                block_number = ?log.block_number,
                pool_id = ?self.pool_id,
                liquidity_provider = ?event.liquidityProvider,
                "PoolBalanceChanged"
            );
        } else if topic0 == IVault::PoolBalanceManaged::SIGNATURE_HASH {
            // Asset Manager moved funds between cash and managed balances
            let event = IVault::PoolBalanceManaged::decode_log(&log.inner)?;
            if event.poolId != self.pool_id {
                return Ok(SyncAction::None);
            }

            // cashDelta represents change in the Vault's cash balance for this token
            // Positive = funds deposited back to Vault (available for swaps)
            // Negative = funds withdrawn from Vault (not available for swaps)
            if let Some(t) = self.tokens.get_mut(&event.token) {
                let cash_delta = event.cashDelta;
                if cash_delta.is_positive() {
                    t.balance += U256::try_from(cash_delta).unwrap();
                } else if cash_delta.is_negative() {
                    let abs_delta = U256::try_from(-cash_delta).unwrap();
                    t.balance = t.balance.saturating_sub(abs_delta);
                }
            }

            tracing::info!(
                target = "amms::balancer_v2::sync",
                block_number = ?log.block_number,
                pool_id = ?self.pool_id,
                asset_manager = ?event.assetManager,
                token = ?event.token,
                cash_delta = ?event.cashDelta,
                managed_delta = ?event.managedDelta,
                "PoolBalanceManaged"
            );
        }

        self.update_spot_prices();
        Ok(SyncAction::None)
    }

    async fn init<N, P>(self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        Self: Sized,
        N: Network,
        P: Provider<N> + Clone,
    {
        // Must call inherent init logic but we need access to factory logic.
        // The inherent init method implementation is already provided in the inherent impl block
        // wait, inherent impl block has `init` which calls `BalancerV2Factory::init_batch`.
        // We can just call that inherent method? No, generic bounds conflict.
        // Actually, the inherent `init` method (lines 790-802) has the EXACT same signature.
        // So we can just delegate or move it.
        // To avoid duplication, let's call the inherent one if possible, but rust generic methods are tricky.
        // Better yet: MOVE the inherent `init` method (lines 790-802) into this trait impl block.
        // But wait, the inherent method calls `BalancerV2Factory::init_batch`.

        let amm = AMM::BalancerV2Pool(self);
        let amms = vec![amm];
        let synced_amms = BalancerV2Factory::init_batch(amms, block_number, provider).await?;

        if let Some(AMM::BalancerV2Pool(pool)) = synced_amms.into_iter().next() {
            Ok(pool)
        } else {
            Err(AMMError::Msg(
                "Failed to initialize BalancerV2Pool".to_string(),
            ))
        }
    }
}

impl BalancerV2Pool {
    pub(crate) fn update_spot_prices(&mut self) {
        let tokens = self.token_list.clone();
        if tokens.len() < 2 {
            return;
        }

        for i in 0..tokens.len() {
            for j in 0..tokens.len() {
                if i == j {
                    continue;
                }
                let base = tokens[i];
                let quote = tokens[j];

                // Avoid using BPT in swap if applicable
                if let Some(bpt_idx) = self.bpt_index {
                    if i == bpt_idx || j == bpt_idx {
                        continue;
                    }
                }

                // Get decimals for 1 unit
                let decimals = if let Some(t) = self.tokens.get(&base) {
                    t.decimals
                } else {
                    continue;
                };

                let amount_in = U256::from(10).pow(U256::from(decimals));

                if let Ok(amount_out) = self.simulate_swap(base, quote, amount_in) {
                    let decimals_quote = if let Some(t) = self.tokens.get(&quote) {
                        t.decimals
                    } else {
                        18
                    };

                    let price = amount_out.to_string().parse::<f64>().unwrap_or(0.0)
                        / 10f64.powi(decimals_quote as i32);
                    self.spot_prices.insert((base, quote), price);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::abi::{FundManagement, IBalancerQueries, SingleSwap};
    use super::*;
    use alloy::primitives::address;
    use alloy::providers::{Provider, ProviderBuilder};
    use alloy::rpc::client::ClientBuilder;
    use alloy::transports::layers::{RetryBackoffLayer, ThrottleLayer};
    use std::sync::Arc;
    use tokio::time::{timeout, Duration};

    async fn assert_exact_out_case<P: Provider + Clone>(
        amm: &BalancerV2Pool,
        provider: &Arc<P>,
        block: BlockId,
        pool_id: B256,
        token_in: Address,
        token_out: Address,
        amount_out: U256,
    ) -> eyre::Result<()> {
        let balancer_queries = IBalancerQueries::new(
            address!("E39B5e3B6D74016b2F6A9673D7d7493B6DF549d5"),
            provider.clone(),
        );
        let local_amount_in = amm.simulate_swap_exact_out(token_in, token_out, amount_out)?;
        let funds = FundManagement {
            sender: address!("000000000000000000000000000000000000dead"),
            fromInternalBalance: false,
            recipient: address!("000000000000000000000000000000000000dead"),
            toInternalBalance: false,
        };
        let single_swap = SingleSwap {
            poolId: pool_id,
            kind: 1, // GIVEN_OUT
            assetIn: token_in,
            assetOut: token_out,
            amount: amount_out,
            userData: alloy::primitives::Bytes::new(),
        };

        println!(
            "[exact-out][start] block={:?} pool={} token_in={} token_out={} target_out={}",
            block, amm.address, token_in, token_out, amount_out
        );

        let onchain_amount_in = timeout(
            Duration::from_secs(45),
            balancer_queries
                .querySwap(single_swap, funds)
                .block(block)
                .call(),
        )
        .await
        .map_err(|_| {
            eyre::eyre!(
                "querySwap timed out (>45s) pool={} token_in={} token_out={} target_out={} block={:?}",
                amm.address,
                token_in,
                token_out,
                amount_out,
                block
            )
        })??;

        let diff = if local_amount_in > onchain_amount_in {
            local_amount_in - onchain_amount_in
        } else {
            onchain_amount_in - local_amount_in
        };

        let local_out = amm.simulate_swap(token_in, token_out, local_amount_in)?;
        let (local_prev, local_prev_out) = if local_amount_in > U256::ZERO {
            let prev = local_amount_in - U256::from(1u8);
            let prev_out = amm.simulate_swap(token_in, token_out, prev)?;
            (prev, prev_out)
        } else {
            (U256::ZERO, U256::ZERO)
        };
        let local_out_at_onchain_in = amm.simulate_swap(token_in, token_out, onchain_amount_in)?;
        let rate_in = amm
            .tokens
            .get(&token_in)
            .and_then(|t| t.rate)
            .unwrap_or(U256::from(10).pow(U256::from(18)));
        let rate_out = amm
            .tokens
            .get(&token_out)
            .and_then(|t| t.rate)
            .unwrap_or(U256::from(10).pow(U256::from(18)));
        let amp = amm
            .amp_state
            .as_ref()
            .map(|s| s.end_value)
            .unwrap_or(U256::ZERO);

        println!(
            "[exact-out][cmp] block={:?} pool={} pool_type={:?} token_in={} token_out={} target_out={} local_in={} onchain_in={} diff={} local_out_at_local_in={} local_prev_in={} local_prev_out={} local_out_at_onchain_in={} rate_in={} rate_out={} swap_fee={} amp={}",
            block,
            amm.address,
            amm.pool_type,
            token_in,
            token_out,
            amount_out,
            local_amount_in,
            onchain_amount_in,
            diff,
            local_out,
            local_prev,
            local_prev_out,
            local_out_at_onchain_in,
            rate_in,
            rate_out,
            amm.swap_fee,
            amp
        );

        // NOTE: Weighted pools use LogExpMath approximations with MAX_POW_RELATIVE_ERROR.
        // On-chain `powUp/powDown` intentionally introduce a tiny relative error (~1e-14),
        // which can show up in exact-out comparisons even when local math matches the contract.
        // This is expected for Weighted exact-out and is not related to rate-cache drift.
        let tolerance = std::cmp::max(onchain_amount_in / U256::from(100_000u64), U256::from(1u8));
        assert!(
            diff <= tolerance,
            "exact-out mismatch pool={} token_in={} token_out={} target_out={} local_in={} onchain_in={} diff={} tolerance={}",
            amm.address,
            token_in,
            token_out,
            amount_out,
            local_amount_in,
            onchain_amount_in,
            diff,
            tolerance
        );

        // Minimality sanity in local model.
        // Weighted exact-out path is compared directly against querySwap(GIVEN_OUT), and given-in/out are not
        // perfectly inverse under rounding, so we only enforce this strictly for stable families.
        if !matches!(amm.pool_type, BalancerV2PoolType::Weighted) {
            assert!(local_out >= amount_out);
            if local_amount_in > U256::ZERO {
                assert!(local_prev_out < amount_out);
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_balancer_v2_mainnet_sync_and_swap() -> eyre::Result<()> {
        dotenv::dotenv().ok();
        let rpc_endpoint = match std::env::var("ETHEREUM_PROVIDER") {
            Ok(url) => url,
            Err(_) => {
                println!("Skipping test: ETHEREUM_PROVIDER not set");
                return Ok(());
            }
        };

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(50))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = Arc::new(ProviderBuilder::new().connect_client(client));
        let block = BlockId::from(provider.get_block_number().await?);

        // BAL-WETH 80/20 Weighted Pool
        let pool_address = address!("5c6Ee304399DBdB9C8Ef030aB642B10820DB8F56");
        let vault_address = address!("BA12222222228d8Ba445958a75a0704d566BF2C8");
        let pool_id =
            B256::from_str("5c6ee304399dbdb9c8ef030ab642b10820db8f56000200000000000000000014")?;

        // Create pool
        let pool = BalancerV2Pool::new(
            pool_address,
            vault_address,
            pool_id,
            BalancerV2PoolType::Weighted,
        );

        println!("Initializing pool...");
        let amm = pool.init(block, provider.clone()).await?;
        println!("Pool initialized!");

        println!("Pool Address: {}", amm.address);
        println!("Pool ID: {}", amm.pool_id);
        println!("Swap Fee: {}", amm.swap_fee);
        println!("Tokens:");
        for (addr, state) in &amm.tokens {
            println!(
                "  Token {}: Balance {}, Weight {:?}, Decimals {}",
                addr, state.balance, state.weight, state.decimals
            );
        }

        // Simulate swap
        // Swap 1 WETH for BAL
        // BAL: 0xba100000625a3754423978a60c9317c58a424e3D
        // WETH: 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let bal = address!("ba100000625a3754423978a60c9317c58a424e3D");
        let amount_in = U256::from(1_000_000_000_000_000_000u128); // 1 WETH

        println!("Simulating swap 1 WETH -> BAL");
        let amount_out = amm.simulate_swap(weth, bal, amount_in)?;
        println!("Local Amount out: {}", amount_out);

        assert!(
            amount_out > U256::ZERO,
            "Amount out should be greater than zero"
        );

        // On-chain verification
        let balancer_queries_addr = address!("E39B5e3B6D74016b2F6A9673D7d7493B6DF549d5");

        let balancer_queries = IBalancerQueries::new(balancer_queries_addr, provider.clone());

        let single_swap = SingleSwap {
            poolId: pool_id,
            kind: 0, // GIVEN_IN
            assetIn: weth,
            assetOut: bal,
            amount: amount_in,
            userData: alloy::primitives::Bytes::new(),
        };

        let funds = FundManagement {
            sender: address!("000000000000000000000000000000000000dead"), // Non-zero address
            fromInternalBalance: false,
            recipient: address!("000000000000000000000000000000000000dead"),
            toInternalBalance: false,
        };

        let onchain_amount_out = balancer_queries
            .querySwap(single_swap, funds)
            .block(block)
            .call()
            .await?;

        println!("On-chain Amount out: {}", onchain_amount_out);

        // Check if the difference is within acceptable range (e.g. 1%)
        // Weighted math can have small precision differences
        let diff = if amount_out > onchain_amount_out {
            amount_out - onchain_amount_out
        } else {
            onchain_amount_out - amount_out
        };

        let tolerance = onchain_amount_out / U256::from(1000);
        assert!(
            diff <= tolerance,
            "Local result {} differs too much from on-chain result {}",
            amount_out,
            onchain_amount_out
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_balancer_v2_wbtc_weth_weighted_sync_and_swap() -> eyre::Result<()> {
        use alloy::sol;
        sol! {
            #[sol(rpc)]
            interface IGetPoolId {
                function getPoolId() external view returns (bytes32);
            }
        }

        dotenv::dotenv().ok();
        let rpc_endpoint = match std::env::var("ETHEREUM_PROVIDER") {
            Ok(url) => url,
            Err(_) => {
                println!("Skipping test: ETHEREUM_PROVIDER not set");
                return Ok(());
            }
        };

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(50))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = Arc::new(ProviderBuilder::new().connect_client(client));
        let block = BlockId::from(provider.get_block_number().await?);

        // WBTC/WETH 50/50 Weighted Pool
        // Pool Address: 0xA6F548DF93DE924d73be7D25dC02554c6bD66dB5
        let pool_address = address!("A6F548DF93DE924d73be7D25dC02554c6bD66dB5");
        let vault_address = address!("BA12222222228d8Ba445958a75a0704d566BF2C8");

        let pool_contract = IGetPoolId::new(pool_address, provider.clone());
        let pool_id = pool_contract.getPoolId().call().await?;

        println!("Fetched Pool ID: {}", pool_id);

        let pool = BalancerV2Pool::new(
            pool_address,
            vault_address,
            pool_id,
            BalancerV2PoolType::Weighted,
        );
        let amm = pool.init(block, provider.clone()).await?;
        println!("Pool initialized!");
        // Swap 0.1 WBTC -> WETH
        // WBTC: 0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599
        // WETH: 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2
        let wbtc = address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let amount_in = U256::from(10_000_000u128); // 0.1 WBTC (8 decimals)

        println!("Simulating swap 0.1 WBTC -> WETH");
        let amount_out = amm.simulate_swap(wbtc, weth, amount_in)?;
        println!("Local Amount out: {}", amount_out);

        assert!(
            amount_out > U256::ZERO,
            "Amount out should be greater than zero"
        );

        // On-chain verification
        let balancer_queries_addr = address!("E39B5e3B6D74016b2F6A9673D7d7493B6DF549d5");
        let balancer_queries = IBalancerQueries::new(balancer_queries_addr, provider.clone());

        let single_swap = SingleSwap {
            poolId: pool_id,
            kind: 0, // GIVEN_IN
            assetIn: wbtc,
            assetOut: weth,
            amount: amount_in,
            userData: alloy::primitives::Bytes::new(),
        };

        let funds = FundManagement {
            sender: address!("000000000000000000000000000000000000dead"),
            fromInternalBalance: false,
            recipient: address!("000000000000000000000000000000000000dead"),
            toInternalBalance: false,
        };

        let onchain_amount_out = balancer_queries
            .querySwap(single_swap, funds)
            .block(block)
            .call()
            .await?;
        println!("On-chain Amount out: {}", onchain_amount_out);

        let diff = if amount_out > onchain_amount_out {
            amount_out - onchain_amount_out
        } else {
            onchain_amount_out - amount_out
        };

        let tolerance = onchain_amount_out / U256::from(1000);
        assert!(
            diff <= tolerance,
            "Local result {} differs too much from on-chain result {}",
            amount_out,
            onchain_amount_out
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_balancer_v2_vlr_weth_weighted_sync_and_swap() -> eyre::Result<()> {
        use alloy::sol;
        sol! {
            #[sol(rpc)]
            interface IGetPoolId {
                function getPoolId() external view returns (bytes32);
            }
        }

        dotenv::dotenv().ok();
        let rpc_endpoint = match std::env::var("ETHEREUM_PROVIDER") {
            Ok(url) => url,
            Err(_) => {
                println!("Skipping test: ETHEREUM_PROVIDER not set");
                return Ok(());
            }
        };

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(50))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = Arc::new(ProviderBuilder::new().connect_client(client));
        let block = BlockId::from(provider.get_block_number().await?);

        // VLR/WETH 80/20 Weighted Pool
        // VLR: 0x4e107a0000DB66f0E9Fd2039288Bf811dD1f9c74
        let pool_address = address!("0x4446d101e91d042b5d08b62fde126e307f1acd57");
        let vault_address = address!("BA12222222228d8Ba445958a75a0704d566BF2C8");

        let pool_contract = IGetPoolId::new(pool_address, provider.clone());
        let pool_id = pool_contract.getPoolId().call().await?;

        let pool = BalancerV2Pool::new(
            pool_address,
            vault_address,
            pool_id,
            BalancerV2PoolType::Weighted,
        );
        let amm = pool.init(block, provider.clone()).await?;
        println!("Pool initialized!");

        // Swap 100 VLR -> WETH
        let vlr = address!("4e107a0000DB66f0E9Fd2039288Bf811dD1f9c74");
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let amount_in = U256::from(100_000_000_000_000_000_000u128); // 100 VLR

        println!("Simulating swap 100 VLR -> WETH");
        let amount_out = amm.simulate_swap(vlr, weth, amount_in)?;
        println!("Local Amount out: {}", amount_out);

        assert!(
            amount_out > U256::ZERO,
            "Amount out should be greater than zero"
        );

        // On-chain verification
        let balancer_queries_addr = address!("E39B5e3B6D74016b2F6A9673D7d7493B6DF549d5");
        let balancer_queries = IBalancerQueries::new(balancer_queries_addr, provider.clone());

        let single_swap = SingleSwap {
            poolId: pool_id,
            kind: 0, // GIVEN_IN
            assetIn: vlr,
            assetOut: weth,
            amount: amount_in,
            userData: alloy::primitives::Bytes::new(),
        };

        let funds = FundManagement {
            sender: address!("000000000000000000000000000000000000dead"),
            fromInternalBalance: false,
            recipient: address!("000000000000000000000000000000000000dead"),
            toInternalBalance: false,
        };

        let onchain_amount_out = balancer_queries
            .querySwap(single_swap, funds)
            .block(block)
            .call()
            .await?;
        println!("On-chain Amount out: {}", onchain_amount_out);

        let diff = if amount_out > onchain_amount_out {
            amount_out - onchain_amount_out
        } else {
            onchain_amount_out - amount_out
        };

        let tolerance = onchain_amount_out / U256::from(1000);
        assert!(
            diff <= tolerance,
            "Local result {} differs too much from on-chain result {}",
            amount_out,
            onchain_amount_out
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_balancer_v2_composable_stable_sync_and_swap() -> eyre::Result<()> {
        use alloy::sol;
        sol! {
            #[sol(rpc)]
            interface IGetPoolId {
                function getPoolId() external view returns (bytes32);
            }
        }

        dotenv::dotenv().ok();
        let rpc_endpoint = match std::env::var("ETHEREUM_PROVIDER") {
            Ok(url) => url,
            Err(_) => {
                println!("Skipping test: ETHEREUM_PROVIDER not set");
                return Ok(());
            }
        };

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(50))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = Arc::new(ProviderBuilder::new().connect_client(client));

        // wstETH-WETH Composable Stable Pool
        let pool_address = address!("32296969ef14eb0c6d29669c550d4a0449130230");
        let vault_address = address!("BA12222222228d8Ba445958a75a0704d566BF2C8");

        let pool_contract = IGetPoolId::new(pool_address, provider.clone());
        let pool_id = pool_contract.getPoolId().call().await?;

        println!("Fetched Pool ID: {}", pool_id);

        // Create pool
        let pool = BalancerV2Pool::new(
            pool_address,
            vault_address,
            pool_id,
            BalancerV2PoolType::ComposableStable,
        );

        println!("Initializing pool...");
        let amm = pool.init(BlockId::latest(), provider.clone()).await?;
        println!("Pool initialized!");

        println!("Pool Address: {}", amm.address);
        println!("Pool ID: {}", amm.pool_id);
        println!("Swap Fee: {}", amm.swap_fee);
        println!("Tokens:");
        for (addr, state) in &amm.tokens {
            println!(
                "  Token {}: Balance {}, Rate Provider {:?}, Rate {:?}, Decimals {}",
                addr, state.balance, state.rate_provider, state.rate, state.decimals
            );
        }

        // Simulate swap
        // Swap 1 WETH for wstETH
        // WETH: 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2
        // wstETH: 0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let wsteth = address!("7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0");
        let amount_in = U256::from(1_000_000_000_000_000_000u128); // 1 WETH

        println!("Simulating swap 1 WETH -> wstETH");
        let amount_out = amm.simulate_swap(weth, wsteth, amount_in)?;
        println!("Local Amount out: {}", amount_out);

        assert!(
            amount_out > U256::ZERO,
            "Amount out should be greater than zero"
        );

        // On-chain verification
        let balancer_queries_addr = address!("0xE39B5e3B6D74016b2F6A9673D7d7493B6DF549d5");

        let balancer_queries = IBalancerQueries::new(balancer_queries_addr, provider.clone());

        let single_swap = SingleSwap {
            poolId: pool_id,
            kind: 0, // GIVEN_IN
            assetIn: weth,
            assetOut: wsteth,
            amount: amount_in,
            userData: alloy::primitives::Bytes::new(),
        };

        let funds = FundManagement {
            sender: address!("000000000000000000000000000000000000dead"),
            fromInternalBalance: false,
            recipient: address!("000000000000000000000000000000000000dead"),
            toInternalBalance: false,
        };

        let onchain_amount_out = balancer_queries
            .querySwap(single_swap, funds)
            .call()
            .await?;
        println!("On-chain Amount out: {}", onchain_amount_out);

        let diff = if amount_out > onchain_amount_out {
            amount_out - onchain_amount_out
        } else {
            onchain_amount_out - amount_out
        };

        let tolerance = onchain_amount_out / U256::from(1000);
        assert!(
            diff <= tolerance,
            "Local result {} differs too much from on-chain result {}",
            amount_out,
            onchain_amount_out
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_balancer_v2_weighted_exact_out_matches_query() -> eyre::Result<()> {
        dotenv::dotenv().ok();
        let rpc_endpoint = match std::env::var("ETHEREUM_PROVIDER") {
            Ok(url) => url,
            Err(_) => {
                println!("Skipping test: ETHEREUM_PROVIDER not set");
                return Ok(());
            }
        };

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(50))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);
        let provider = Arc::new(ProviderBuilder::new().connect_client(client));
        let block = BlockId::from(provider.get_block_number().await?);

        // BAL-WETH 80/20 Weighted Pool
        let pool_address = address!("5c6Ee304399DBdB9C8Ef030aB642B10820DB8F56");
        let vault_address = address!("BA12222222228d8Ba445958a75a0704d566BF2C8");
        let pool_id =
            B256::from_str("5c6ee304399dbdb9c8ef030ab642b10820db8f56000200000000000000000014")?;

        let pool = BalancerV2Pool::new(
            pool_address,
            vault_address,
            pool_id,
            BalancerV2PoolType::Weighted,
        );
        let amm = pool.init(block, provider.clone()).await?;

        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let bal = address!("ba100000625a3754423978a60c9317c58a424e3D");
        let amount_out = U256::from(10_000_000_000_000_000_000u128); // 10 BAL

        assert_exact_out_case(&amm, &provider, block, pool_id, weth, bal, amount_out).await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_balancer_v2_composable_stable_exact_out_matches_query() -> eyre::Result<()> {
        use alloy::sol;
        sol! {
            #[sol(rpc)]
            interface IGetPoolId {
                function getPoolId() external view returns (bytes32);
            }
        }

        dotenv::dotenv().ok();
        let rpc_endpoint = match std::env::var("ETHEREUM_PROVIDER") {
            Ok(url) => url,
            Err(_) => {
                println!("Skipping test: ETHEREUM_PROVIDER not set");
                return Ok(());
            }
        };

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(50))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);
        let provider = Arc::new(ProviderBuilder::new().connect_client(client));
        let block = BlockId::from(provider.get_block_number().await?);

        let pool_address = address!("32296969ef14eb0c6d29669c550d4a0449130230");
        let vault_address = address!("BA12222222228d8Ba445958a75a0704d566BF2C8");
        let pool_contract = IGetPoolId::new(pool_address, provider.clone());
        let pool_id = pool_contract.getPoolId().call().await?;

        let pool = BalancerV2Pool::new(
            pool_address,
            vault_address,
            pool_id,
            BalancerV2PoolType::ComposableStable,
        );
        let amm = pool.init(block, provider.clone()).await?;

        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let wsteth = address!("7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0");
        let wsteth_balance = amm
            .tokens
            .get(&wsteth)
            .ok_or_else(|| eyre::eyre!("missing wsteth token state"))?
            .balance;
        // Keep target safely below current liquidity to reduce block-dependent failures.
        let amount_out = std::cmp::max(wsteth_balance / U256::from(20u8), U256::from(1u8)); // 5%

        assert_exact_out_case(&amm, &provider, block, pool_id, weth, wsteth, amount_out).await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_balancer_v2_weighted_exact_out_matrix() -> eyre::Result<()> {
        dotenv::dotenv().ok();
        let rpc_endpoint = match std::env::var("ETHEREUM_PROVIDER") {
            Ok(url) => url,
            Err(_) => {
                println!("Skipping test: ETHEREUM_PROVIDER not set");
                return Ok(());
            }
        };

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(50))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);
        let provider = Arc::new(ProviderBuilder::new().connect_client(client));
        let block = BlockId::from(provider.get_block_number().await?);

        let pool_address = address!("5c6Ee304399DBdB9C8Ef030aB642B10820DB8F56");
        let vault_address = address!("BA12222222228d8Ba445958a75a0704d566BF2C8");
        let pool_id =
            B256::from_str("5c6ee304399dbdb9c8ef030ab642b10820db8f56000200000000000000000014")?;
        let amm = BalancerV2Pool::new(
            pool_address,
            vault_address,
            pool_id,
            BalancerV2PoolType::Weighted,
        )
        .init(block, provider.clone())
        .await?;

        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let bal = address!("ba100000625a3754423978a60c9317c58a424e3D");

        let bal_targets = [
            U256::from(1_000_000_000_000_000_000u128),  // 1 BAL
            U256::from(5_000_000_000_000_000_000u128),  // 5 BAL
            U256::from(10_000_000_000_000_000_000u128), // 10 BAL
        ];
        for target in bal_targets {
            assert_exact_out_case(&amm, &provider, block, pool_id, weth, bal, target).await?;
        }

        let weth_targets = [
            U256::from(10_000_000_000_000_000u128),  // 0.01 WETH
            U256::from(50_000_000_000_000_000u128),  // 0.05 WETH
            U256::from(100_000_000_000_000_000u128), // 0.1 WETH
        ];
        for target in weth_targets {
            assert_exact_out_case(&amm, &provider, block, pool_id, bal, weth, target).await?;
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_balancer_v2_composable_stable_exact_out_matrix() -> eyre::Result<()> {
        use alloy::sol;
        sol! {
            #[sol(rpc)]
            interface IGetPoolId {
                function getPoolId() external view returns (bytes32);
            }
        }

        dotenv::dotenv().ok();
        let rpc_endpoint = match std::env::var("ETHEREUM_PROVIDER") {
            Ok(url) => url,
            Err(_) => {
                println!("Skipping test: ETHEREUM_PROVIDER not set");
                return Ok(());
            }
        };

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(50))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);
        let provider = Arc::new(ProviderBuilder::new().connect_client(client));
        let block = BlockId::from(provider.get_block_number().await?);

        let pool_address = address!("32296969ef14eb0c6d29669c550d4a0449130230");
        let vault_address = address!("BA12222222228d8Ba445958a75a0704d566BF2C8");
        let pool_contract = IGetPoolId::new(pool_address, provider.clone());
        let pool_id = pool_contract.getPoolId().call().await?;
        let amm = BalancerV2Pool::new(
            pool_address,
            vault_address,
            pool_id,
            BalancerV2PoolType::ComposableStable,
        )
        .init(block, provider.clone())
        .await?;

        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let wsteth = address!("7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0");
        let mk_targets = |balance: U256| -> [U256; 3] {
            // Keep targets well below current liquidity to avoid block-dependent false failures.
            // max_target ~= 1/5 of current out-token balance.
            let t1 = std::cmp::max(balance / U256::from(50u8), U256::from(1u8)); // 2%
            let t2 = std::cmp::max(balance / U256::from(20u8), U256::from(1u8)); // 5%
            let t3 = std::cmp::max(balance / U256::from(10u8), U256::from(1u8)); // 10%
            [t1, t2, t3]
        };

        let wsteth_balance = amm
            .tokens
            .get(&wsteth)
            .ok_or_else(|| eyre::eyre!("missing wsteth token state"))?
            .balance;
        for target in mk_targets(wsteth_balance) {
            assert_exact_out_case(&amm, &provider, block, pool_id, weth, wsteth, target).await?;
        }

        let weth_balance = amm
            .tokens
            .get(&weth)
            .ok_or_else(|| eyre::eyre!("missing weth token state"))?
            .balance;
        for target in mk_targets(weth_balance) {
            assert_exact_out_case(&amm, &provider, block, pool_id, wsteth, weth, target).await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod test_sync_drift;

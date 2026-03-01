use crate::amms::{
    amm::{AutomatedMarketMaker, SyncAction},
    balancer_v2::{
        math::{stable_math, weighted_math},
        BalancerV2Error,
    },
    error::AMMError,
    float::u256_to_float,
};
use alloy::primitives::address;
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, B256, U256},
    providers::Provider,
    rpc::types::Log,
    sol,
    sol_types::{SolEvent, SolValue},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;

pub fn get_vault_address(_chain_id: u64) -> Option<Address> {
    // Balancer V3 Vault is deployed using Create2 at the same address on all supported chains.
    // Ref: https://docs.balancer.fi/developer-reference/contracts/deployment-addresses/
    Some(address!("ba1333333333a1BA1108E8412f11850A5C319bA9"))
}
use thiserror::Error;

pub mod factory;
pub use factory::BalancerV3Factory;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BalancerV3Error {
    #[error("Token In Does Not Exist")]
    TokenInDoesNotExist,
    #[error("Token Out Does Not Exist")]
    TokenOutDoesNotExist,
    #[error("Initialization Error")]
    InitializationError,
    #[error("Not Supported: {0}")]
    NotSupported(String),
    #[error("Math Error: {0}")]
    MathError(String),
}

impl From<BalancerV2Error> for BalancerV3Error {
    fn from(e: BalancerV2Error) -> Self {
        BalancerV3Error::MathError(e.to_string())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum BalancerV3PoolType {
    #[default]
    Weighted,
    Stable,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BalancerV3Pool {
    pub address: Address,
    pub last_synced_block: u64,
    pub vault_address: Address,
    pub pool_type: BalancerV3PoolType,
    pub tokens: HashMap<Address, V3TokenState>,
    pub token_list: Vec<Address>,
    pub swap_fee: U256,
    // Pool specific data
    pub amp: Option<U256>,          // For Stable pools
    pub weights: Option<Vec<U256>>, // For Weighted pools
    #[serde(skip)]
    pub spot_prices: std::collections::HashMap<(Address, Address), f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V3TokenState {
    pub address: Address,
    pub decimals: u8,
    pub index: usize,         // Index in the pool
    pub balance: U256,        // Raw balance in Vault
    pub scaling_factor: U256, // 10^(18-decimals) usually
    pub rate: U256,           // Rate provider rate, default 1e18
    pub rate_provider: Option<Address>,
}

sol! {
    struct PoolData {
        address poolAddress;
        uint8 poolType;
        address[] tokens;
        uint8[] decimals;
        uint256[] balances;
        uint256[] weights;
        uint256 amp;
        uint256 swapFee;
        address[] rateProviders;
        uint256[] rates;
    }
}

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    IGetBalancerV3RatesBatchRequest,
    "src/amms/abi/GetBalancerV3RatesBatchRequest.json"
);

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    IGetBalancerV3PoolDataBatchRequest,
    "src/amms/abi/GetBalancerV3PoolDataBatchRequest.json"
);

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IBalancerV3Router {
        function querySwapSingleTokenExactIn(
            address pool,
            address tokenIn,
            address tokenOut,
            uint256 exactAmountIn,
            address sender,
            bytes memory userData
        ) external returns (uint256 amountOut);
    }

    // Balancer V3 Pool Contract
    #[sol(rpc)]
    interface IBalancerV3PoolContract {
        function getTokens() external view returns (address[] memory);
        function getRateProviders() external view returns (address[] memory);
        function totalSupply() external view returns (uint256);
        function getSwapFeePercentage() external view returns (uint256);
        // Some pools might use static fee
        function getStaticSwapFeePercentage() external view returns (uint256);
        // Stable Pool
        function getAmplificationParameter() external view returns (uint256 value, bool isUpdating, uint256 precision);
        // Weighted Pool
        function getNormalizedWeights() external view returns (uint256[] memory);
    }

    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IVaultV3 {
        event Swap(
            address indexed pool,
            address indexed tokenIn,
            address indexed tokenOut,
            uint256 amountIn,
            uint256 amountOut,
            uint256 swapFeeAmount,
            uint256 protocolSwapFeeAmount
        );

        event PoolBalanceChanged(
            address indexed pool,
            address indexed liquidityProvider,
            address[] tokens,
            int256[] deltas,
            uint256[] protocolFeeAmounts
        );
    }

    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    struct TokenInfo {
        uint8 tokenType;
        address rateProvider;
        bool paysYieldFees;
    }

    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IVaultExplorer {
        function getPoolTokenInfo(address pool) external view returns (
            address[] memory tokens,
            TokenInfo[] memory tokenInfo,
            uint256[] memory balancesRaw,
            uint256[] memory lastLiveBalances
        );
    }

    #[sol(rpc)]
    interface IERC20 {
        function decimals() external view returns (uint8);
    }

    #[sol(rpc)]
    interface IRateProvider {
        function getRate() external view returns (uint256);
    }
);

impl BalancerV3Pool {
    pub fn new(address: Address, vault_address: Address, pool_type: BalancerV3PoolType) -> Self {
        Self {
            address,
            last_synced_block: 0,
            vault_address,
            pool_type,
            tokens: HashMap::new(),
            token_list: Vec::new(),
            swap_fee: U256::ZERO,
            amp: None,
            weights: None,
            spot_prices: HashMap::new(),
        }
    }

    pub(crate) fn update_spot_prices(&mut self) {
        if self.token_list.len() < 2 {
            return;
        }

        for i in 0..self.token_list.len() {
            for j in 0..self.token_list.len() {
                if i == j {
                    continue;
                }
                let base = self.token_list[i];
                let quote = self.token_list[j];

                if let Ok(price) = self.calculate_price(base, quote) {
                    self.spot_prices.insert((base, quote), price);
                }
            }
        }
    }

    pub async fn batch_update_rates<N, P>(pools: &mut [Self], provider: P) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        // ... (implementation same as before, omitted for brevity if not changed, but I'll keep it if I'm replacing the whole block or careful with SearchReplace)
        // Actually I should just replace the init function mostly.
        // But I need to insert the interfaces.

        // I will keep the original implementation of batch_update_rates and other methods.
        // I will only use SearchReplace for the sol! block and init function.
        if pools.is_empty() {
            return Ok(());
        }

        // Collect all unique rate providers
        let mut providers = Vec::new();
        for pool in pools.iter() {
            for token_state in pool.tokens.values() {
                if let Some(rp) = token_state.rate_provider {
                    providers.push(rp);
                }
            }
        }

        // Dedup
        providers.sort();
        providers.dedup();

        if providers.is_empty() {
            return Ok(());
        }

        // Batch call
        let deployer = IGetBalancerV3RatesBatchRequest::deploy_builder(provider, providers.clone());
        let res = deployer.call_raw().await?;
        let rates = <Vec<U256> as SolValue>::abi_decode(&res)?;

        // Map back to pools
        let rates_map: HashMap<Address, U256> =
            providers.into_iter().zip(rates.into_iter()).collect();

        for pool in pools.iter_mut() {
            for token_state in pool.tokens.values_mut() {
                if let Some(rp) = token_state.rate_provider {
                    if let Some(rate) = rates_map.get(&rp) {
                        token_state.rate = *rate;
                    }
                }
            }
        }

        for pool in pools.iter_mut() {
            pool.update_spot_prices();
        }

        Ok(())
    }

    /// Batch update swap fees for multiple pools.
    /// This is necessary because swap fees can be updated by governance and are not emitted as events.
    pub async fn batch_update_swap_fees<N, P>(
        pools: &mut [Self],
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        use futures::{stream::FuturesUnordered, StreamExt};

        if pools.is_empty() {
            return Ok(());
        }

        // Create concurrent requests for each pool's swap fee
        let mut futures = FuturesUnordered::new();

        for pool in pools.iter() {
            let pool_address = pool.address;
            let provider = provider.clone();

            futures.push(async move {
                let pool_contract = IBalancerV3PoolContract::new(pool_address, provider.clone());

                // Try getSwapFeePercentage first
                if let Ok(fee) = pool_contract.getSwapFeePercentage().call().await {
                    return (pool_address, Some(fee));
                }

                // Fallback to getStaticSwapFeePercentage
                if let Ok(fee) = pool_contract.getStaticSwapFeePercentage().call().await {
                    return (pool_address, Some(fee));
                }

                (pool_address, None)
            });
        }

        // Collect results
        let mut fee_map: HashMap<Address, U256> = HashMap::new();
        while let Some((addr, fee_opt)) = futures.next().await {
            if let Some(fee) = fee_opt {
                fee_map.insert(addr, fee);
            }
        }

        // Apply updates
        let mut updated_count = 0;
        for pool in pools.iter_mut() {
            if let Some(&fee) = fee_map.get(&pool.address) {
                if pool.swap_fee != fee {
                    pool.swap_fee = fee;
                    updated_count += 1;
                }
            }
        }

        if updated_count > 0 {
            tracing::info!("Updated swap_fee for {} Balancer V3 pools", updated_count);
        }

        Ok(())
    }

    // Scale up using Rate Provider logic
    // Scale up using Rate Provider logic
    // Balancer V3 standardized scaling: Amount * ScalingFactor * Rate / 1e18
    fn scale_up(&self, amount: U256, token_state: &V3TokenState) -> Result<U256, BalancerV3Error> {
        // 1. Scale decimals (ScalingFactor = 10^(18-decimals))
        let decimal_scaled =
            amount
                .checked_mul(token_state.scaling_factor)
                .ok_or(BalancerV3Error::MathError(
                    "Mul overflow (decimals)".to_string(),
                ))?;

        // 2. Apply Rate (Rate is 18 decimals)
        // Scaled = (DecimalScaled * Rate) / 1e18
        let final_scaled = decimal_scaled
            .checked_mul(token_state.rate)
            .ok_or(BalancerV3Error::MathError(
                "Mul overflow (rate)".to_string(),
            ))?
            .checked_div(U256::from(1000000000000000000u128)) // 1e18
            .ok_or(BalancerV3Error::MathError("Div zero".to_string()))?;

        Ok(final_scaled)
    }

    // Scale down using Rate Provider logic
    // Reverse: (AmountScaled * 1e18) / Rate / ScalingFactor
    fn scale_down(
        &self,
        amount_scaled: U256,
        token_state: &V3TokenState,
    ) -> Result<U256, BalancerV3Error> {
        // 1. Reverse Rate
        // DecimalScaled = (AmountScaled * 1e18) / Rate
        let decimal_scaled = amount_scaled
            .checked_mul(U256::from(1000000000000000000u128))
            .ok_or(BalancerV3Error::MathError(
                "Mul overflow (scale down)".to_string(),
            ))?
            .checked_div(token_state.rate)
            .ok_or(BalancerV3Error::MathError("Div zero (rate)".to_string()))?;

        // 2. Reverse Decimals
        // Raw = DecimalScaled / ScalingFactor
        let raw = decimal_scaled
            .checked_div(token_state.scaling_factor)
            .ok_or(BalancerV3Error::MathError(
                "Div zero (scaling factor)".to_string(),
            ))?;

        Ok(raw)
    }
}

impl AutomatedMarketMaker for BalancerV3Pool {
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
            IVaultV3::Swap::SIGNATURE_HASH,
            IVaultV3::PoolBalanceChanged::SIGNATURE_HASH,
        ]
    }

    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError> {
        if log.topics().is_empty() {
            return Ok(SyncAction::None);
        }

        let topic0 = log.topics()[0];

        if topic0 == IVaultV3::Swap::SIGNATURE_HASH {
            let swap = IVaultV3::Swap::decode_raw_log(log.topics(), log.data().data.as_ref())?;
            if swap.pool != self.address {
                return Ok(SyncAction::None);
            }

            // Update balances
            if let Some(token_in) = self.tokens.get_mut(&swap.tokenIn) {
                // Protocol fees are taken from the input amount for exactIn swaps (or output for exactOut, but denominated in In usually).
                // The amountIn log includes the fee, so we must subtract it to get the net amount added to the pool.
                let net_amount_in = swap.amountIn.saturating_sub(swap.protocolSwapFeeAmount);
                token_in.balance = token_in
                    .balance
                    .checked_add(net_amount_in)
                    .unwrap_or(token_in.balance);
            }

            if let Some(token_out) = self.tokens.get_mut(&swap.tokenOut) {
                token_out.balance = token_out
                    .balance
                    .checked_sub(swap.amountOut)
                    .unwrap_or(token_out.balance);
            }

            tracing::info!(
                target = "amms::balancer_v3::sync",
                block_number = ?log.block_number,
                pool = ?self.address,
                token_in = ?swap.tokenIn,
                token_out = ?swap.tokenOut,
                amount_in = ?swap.amountIn,
                amount_out = ?swap.amountOut,
                "Swap"
            );
        } else if topic0 == IVaultV3::PoolBalanceChanged::SIGNATURE_HASH {
            let pbc = IVaultV3::PoolBalanceChanged::decode_raw_log(
                log.topics(),
                log.data().data.as_ref(),
            )?;
            if pbc.pool != self.address {
                return Ok(SyncAction::None);
            }

            for (i, token_addr) in pbc.tokens.iter().enumerate() {
                if let Some(token_state) = self.tokens.get_mut(token_addr) {
                    let delta = pbc.deltas[i];
                    let protocol_fee = pbc.protocolFeeAmounts[i];

                    if delta.is_positive() {
                        // Delta is positive: Vault balance increases (user deposits / pays).
                        // Protocol fee is taken from this amount.
                        // Balance += (Delta - ProtocolFee)
                        let abs_delta = delta.into_raw();
                        let net_delta = abs_delta.saturating_sub(protocol_fee);

                        token_state.balance = token_state
                            .balance
                            .checked_add(net_delta)
                            .unwrap_or(token_state.balance);
                    } else {
                        // Delta is negative: Vault balance decreases (user withdraws).
                        // Protocol fee (if any) is ALSO removed from the pool balance.
                        // Typically protocol fees accumulate and are withdrawn.
                        // If checking out PoolBalanceChanged, it represents net changes to vault cache.
                        // Logic: Balance_New = Balance_Old + Delta - ProtocolFee
                        // If Delta < 0: Balance_New = Balance_Old - Abs(Delta) - ProtocolFee

                        let abs_delta = delta.abs().into_raw();
                        let total_deduction = abs_delta.saturating_add(protocol_fee);

                        token_state.balance = token_state
                            .balance
                            .checked_sub(total_deduction)
                            .unwrap_or(token_state.balance);
                    }
                }
            }

            tracing::info!(
                target = "amms::balancer_v3::sync",
                block_number = ?log.block_number,
                pool = ?self.address,
                liquidity_provider = ?pbc.liquidityProvider,
                "PoolBalanceChanged"
            );
        }

        self.update_spot_prices();
        Ok(SyncAction::None)
    }

    fn tokens(&self) -> Vec<Address> {
        self.token_list.clone()
    }

    fn has_sufficient_liquidity(&self) -> bool {
        // Simple heuristic: check if at least 2 tokens have meaningful balance
        let mut meaningful_tokens = 0;
        for token_state in self.tokens.values() {
            let reserve = token_state.balance;
            let decimals = token_state.decimals;

            let is_sufficient = if decimals >= 18 {
                // 0.0001 unit (e.g. 10^14 wei)
                reserve >= U256::from(10).pow(U256::from(decimals.saturating_sub(4)))
            } else if decimals >= 6 {
                // 100 units (e.g. 100 * 10^6 = 10^8)
                let threshold =
                    U256::from(100).saturating_mul(U256::from(10).pow(U256::from(decimals)));
                reserve >= threshold
            } else {
                // Fallback
                reserve >= U256::from(100_000)
            };

            if is_sufficient {
                meaningful_tokens += 1;
            }
        }
        meaningful_tokens >= 2
    }

    fn decimals(&self, token: Address) -> u8 {
        self.tokens.get(&token).map(|t| t.decimals).unwrap_or(0)
    }

    fn spot_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
        // Try cache first
        if let Some(&price) = self.spot_prices.get(&(base_token, quote_token)) {
            // 价格有效性校验
            if price > 0.0 && price.is_finite() {
                return Ok(price);
            }
        }

        // Fallback - 计算价格
        // 注意：计算价格会自动处理 rate 和 decimal，之前已经修复了 double-rate 问题
        let price = self.calculate_price(base_token, quote_token)?;
        // 校验计算结果
        if price <= 0.0 || !price.is_finite() {
            return Err(AMMError::Msg("Invalid calculated spot price".to_string()));
        }
        return Ok(price);
    }

    fn calculate_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
        let token_in_state = self
            .tokens
            .get(&base_token)
            .ok_or(BalancerV3Error::TokenInDoesNotExist)?;
        let token_out_state = self
            .tokens
            .get(&quote_token)
            .ok_or(BalancerV3Error::TokenOutDoesNotExist)?;

        match self.pool_type {
            BalancerV3PoolType::Weighted => {
                let weights = self
                    .weights
                    .as_ref()
                    .ok_or(BalancerV3Error::InitializationError)?;
                let w_in = weights
                    .get(token_in_state.index)
                    .ok_or(BalancerV3Error::InitializationError)?;
                let w_out = weights
                    .get(token_out_state.index)
                    .ok_or(BalancerV3Error::InitializationError)?;

                // Spot Price = (Bo / Wo) / (Bi / Wi)
                // This gives: how many token_out you get for 1 token_in
                let bi = self.scale_up(token_in_state.balance, token_in_state)?;
                let bo = self.scale_up(token_out_state.balance, token_out_state)?;

                let bi_f = u256_to_float(bi)?;
                let bo_f = u256_to_float(bo)?;
                let wi_f = u256_to_float(*w_in)?;
                let wo_f = u256_to_float(*w_out)?;

                if bi_f.is_zero() || wo_f.is_zero() {
                    return Ok(0.0);
                }

                // Correct formula: (Bo * Wi) / (Bi * Wo)
                // Correct formula: (Bo * Wi) / (Bi * Wo)
                let price_norm = (bo_f / wo_f) / (bi_f / wi_f);

                // NOTE: No rate adjustment needed here!
                // scale_up() already applies rate: Amount * ScalingFactor * Rate / 1e18
                // Applying rate ratio again would cause double-rate-adjustment.

                Ok(price_norm.to_f64())
            }
            BalancerV3PoolType::Stable => {
                let amount_in = U256::from(10).pow(U256::from(token_in_state.decimals));
                let amount_out = self.simulate_swap(base_token, quote_token, amount_in)?;

                let amount_out_f = u256_to_float(amount_out)?;
                // simulate_swap takes Raw In and returns Raw Out.
                // Price = Raw Out / Raw In
                // amount_in was 1 unit (1 * 10^decimals)
                // P = amount_out / amount_in

                let amount_in_f = u256_to_float(amount_in)?;
                let price = amount_out_f / amount_in_f;
                Ok(price.to_f64())
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
            .ok_or(BalancerV3Error::TokenInDoesNotExist)?;
        let token_out_state = self
            .tokens
            .get(&quote_token)
            .ok_or(BalancerV3Error::TokenOutDoesNotExist)?;

        // Scale Inputs
        let balance_in_scaled = self.scale_up(token_in_state.balance, token_in_state)?;
        let balance_out_scaled = self.scale_up(token_out_state.balance, token_out_state)?;
        let amount_in_scaled = self.scale_up(amount_in, token_in_state)?;

        match self.pool_type {
            BalancerV3PoolType::Weighted => {
                let weights = self
                    .weights
                    .as_ref()
                    .ok_or(BalancerV3Error::InitializationError)?;
                let w_in = weights
                    .get(token_in_state.index)
                    .ok_or(BalancerV3Error::InitializationError)?;
                let w_out = weights
                    .get(token_out_state.index)
                    .ok_or(BalancerV3Error::InitializationError)?;

                let amount_out_scaled = weighted_math::calculate_out_given_in(
                    balance_in_scaled,
                    *w_in,
                    balance_out_scaled,
                    *w_out,
                    amount_in_scaled,
                    self.swap_fee,
                )
                .map_err(BalancerV3Error::from)?;

                let amount_out = self.scale_down(amount_out_scaled, token_out_state)?;
                Ok(amount_out)
            }
            BalancerV3PoolType::Stable => {
                let amp = self.amp.ok_or(BalancerV3Error::InitializationError)?;

                // For stable pools, we need all balances scaled
                let mut scaled_balances = Vec::with_capacity(self.token_list.len());
                let mut index_in = 0;
                let mut index_out = 0;

                for (i, token_addr) in self.token_list.iter().enumerate() {
                    let state = self
                        .tokens
                        .get(token_addr)
                        .ok_or(BalancerV3Error::InitializationError)?;
                    let scaled = self.scale_up(state.balance, state)?;
                    scaled_balances.push(scaled);

                    if *token_addr == base_token {
                        index_in = i;
                    }
                    if *token_addr == quote_token {
                        index_out = i;
                    }
                }

                let amount_out_scaled = stable_math::calculate_out_given_in(
                    amp,
                    &scaled_balances,
                    index_in,
                    index_out,
                    amount_in_scaled,
                    self.swap_fee,
                )
                .map_err(BalancerV3Error::from)?;

                let amount_out = self.scale_down(amount_out_scaled, token_out_state)?;
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

        // Update balances
        let token_in_state = self
            .tokens
            .get_mut(&base_token)
            .ok_or(BalancerV3Error::TokenInDoesNotExist)?;
        token_in_state.balance = token_in_state
            .balance
            .checked_add(amount_in)
            .ok_or(AMMError::Msg("BalancerV3 balance overflow".into()))?;

        let token_out_state = self
            .tokens
            .get_mut(&quote_token)
            .ok_or(BalancerV3Error::TokenOutDoesNotExist)?;
        token_out_state.balance = token_out_state
            .balance
            .checked_sub(amount_out)
            .ok_or(AMMError::Msg("BalancerV3 balance underflow".into()))?;

        Ok(amount_out)
    }

    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        Self: Sized,
        N: Network,
        P: Provider<N> + Clone,
    {
        // 1. Get Pool Specifics First (Weights/Amp/SwapFee)
        // This helps confirm connection and pool type before fetching balances
        let pool_contract = IBalancerV3PoolContract::new(self.address, provider.clone());

        // Get Swap Fee
        if let Ok(fee) = pool_contract
            .getSwapFeePercentage()
            .block(block_number)
            .call()
            .await
        {
            self.swap_fee = fee;
        }

        if self.swap_fee.is_zero() {
            // Try fallback (Static Fee)
            if let Ok(fee) = pool_contract
                .getStaticSwapFeePercentage()
                .block(block_number)
                .call()
                .await
            {
                self.swap_fee = fee;
            }
        }

        if self.pool_type == BalancerV3PoolType::Stable {
            // Try getAmplificationParameter
            let amp_res = pool_contract
                .getAmplificationParameter()
                .block(block_number)
                .call()
                .await;
            if let Ok(amp_data) = amp_res {
                self.amp = Some(amp_data.value);
            }
        } else if self.pool_type == BalancerV3PoolType::Weighted {
            let weights_res = pool_contract
                .getNormalizedWeights()
                .block(block_number)
                .call()
                .await;
            if let Ok(weights) = weights_res {
                self.weights = Some(weights);
            }
        }

        // 2. Get Tokens and Balances
        // Use VaultExplorer to fetch pool token info
        let vault_explorer_address =
            Address::from_str("0xFc2986feAB34713E659da84F3B1FA32c1da95832")
                .unwrap_or(Address::ZERO);
        let vault_explorer = IVaultExplorer::new(vault_explorer_address, provider.clone());

        // Check if VaultExplorer exists
        let code = provider
            .get_code_at(vault_explorer_address)
            .block_id(block_number)
            .await
            .unwrap_or_default();

        let mut explorer_failed = code.is_empty();
        let mut tokens_data = None;

        if !explorer_failed {
            let pool_tokens_res = vault_explorer
                .getPoolTokenInfo(self.address)
                .block(block_number)
                .call()
                .await;
            match pool_tokens_res {
                Ok(data) => {
                    tokens_data = Some((
                        data.tokens,
                        data.balancesRaw,
                        data.tokenInfo,
                        data.lastLiveBalances,
                    ));
                }
                Err(_) => {
                    // Fail silently or log at debug level if needed, but for now just mark flag
                    explorer_failed = true;
                }
            }
        }

        let (tokens, balances, token_infos, last_live_balances) = if let Some(data) = tokens_data {
            data
        } else {
            // Fallback: Try fetching tokens from Pool directly
            // Note: Pool contract typically only returns tokens, not balances (unless we query each token)
            // But weighted pools expose getNormalizedWeights which we already tried.
            // getTokens() is standard.
            match pool_contract.getTokens().block(block_number).call().await {
                Ok(t) => (t, vec![], vec![], vec![]),
                Err(_) => {
                    return Err(AMMError::BalancerV3Error(
                        BalancerV3Error::InitializationError,
                    ));
                }
            }
        };

        self.token_list = tokens.clone();

        // 4. Populate Token State
        for (i, token_addr) in tokens.iter().enumerate() {
            let balance = if i < balances.len() {
                balances[i]
            } else {
                U256::ZERO
            };
            let _live_balance = if i < last_live_balances.len() {
                last_live_balances[i]
            } else {
                U256::ZERO
            };

            // Use raw balance (not scaled) - live_balance is already scaled to 18 decimals
            // which would cause double-scaling in simulate_swap if we used it here.
            // So we stick to `balance` (which is raw) and ignore `live_balance` for storage.

            // If balance is zero, we might have hit fallback logic where balances are empty.
            if balance.is_zero() && explorer_failed {
                // Nothing much to do without working VaultExplorer for now
            }

            // Get Decimals
            let token_contract = IERC20::new(*token_addr, provider.clone());
            let decimals = token_contract.decimals().block(block_number).call().await?;
            // .map_err(|e| AMMError::from(e))?; // If needed to convert error

            // Calculate Scaling Factor
            let scaling_factor = if decimals <= 18 {
                U256::from(10).pow(U256::from(18 - decimals))
            } else {
                U256::from(1)
            };

            // Rate and Rate Provider
            let mut rate = U256::from(1000000000000000000u128); // Default 1e18
            let mut rate_provider_addr = None;

            if i < token_infos.len() {
                let info = &token_infos[i];
                if info.rateProvider != Address::ZERO {
                    rate_provider_addr = Some(info.rateProvider);
                    // Fetch rate
                    let rp_contract = IRateProvider::new(info.rateProvider, provider.clone());
                    if let Ok(res) = rp_contract.getRate().block(block_number).call().await {
                        rate = res;
                    }
                }
            }
            // If fallback was used, try fetching rate providers from pool if Stable
            else if self.pool_type == BalancerV3PoolType::Stable {
                // Try legacy/pool method if struct info is missing
                match pool_contract
                    .getRateProviders()
                    .block(block_number)
                    .call()
                    .await
                {
                    Ok(rps) => {
                        if i < rps.len() && rps[i] != Address::ZERO {
                            rate_provider_addr = Some(rps[i]);
                            let rp_contract = IRateProvider::new(rps[i], provider.clone());
                            if let Ok(res) = rp_contract.getRate().block(block_number).call().await
                            {
                                rate = res;
                            }
                        }
                    }
                    Err(_) => {}
                }
            }

            let token_state = V3TokenState {
                address: *token_addr,
                decimals,
                index: i,
                balance,
                scaling_factor,
                rate,
                rate_provider: rate_provider_addr,
            };

            self.tokens.insert(*token_addr, token_state);
        }

        self.update_spot_prices();
        Ok(self)
    }
}

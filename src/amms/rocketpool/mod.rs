//! Rocket Pool rETH converter implementation.
//!
//! Models the protocol conversion path `rETH <-> ETH` through the
//! `RocketDepositPool` contract (deposit) and `RocketTokenRETH` (redemption).
//!
//! # Data sourcing
//!
//! All raw state is fetched via a single **Multicall3** batch call covering
//! three contracts:
//!
//! | Field | Source | Contract |
//! |---|---|---|
//! | `total_eth_balance` | `getTotalETHBalance()` | RocketNetworkBalances |
//! | `reth_supply` | `totalSupply()` | RocketTokenRETH (ERC20) |
//! | `deposit_pool_balance` | `getBalance()` | RocketDepositPool |
//! | `excess_balance` | `getExcessBalance()` | RocketDepositPool |
//! | `maximum_deposit_amount` | `getMaximumDepositAmount()` | RocketDepositPool |
//!
//! Derived fields (computed locally, **not** fetched from chain):
//! - `total_collateral = total_eth_balance - excess_balance`
//! - `exchange_rate = (total_eth_balance - excess_balance) * WAD / reth_supply`
//!
//! This avoids depending on `RocketTokenRETH.getExchangeRate()` /
//! `getTotalCollateral()` which may be unavailable after contract upgrades.
//! The formulas exactly match the on-chain contract implementation.

use crate::amms::{
    amm::{AutomatedMarketMaker, SyncAction, AMM},
    error::AMMError,
    float::q64_to_float,
};
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{address, Address, B256, U256},
    providers::Provider,
    rpc::types::Log,
    sol,
    sol_types::{SolCall, SolEvent},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::info;

const WAD: u128 = 1_000_000_000_000_000_000u128;
pub const NATIVE_ETH_PLACEHOLDER: Address =
    address!("EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");

/// Multicall3 is deployed at the same address on all major chains.
const MULTICALL3_ADDRESS: Address =
    address!("cA11bde05977b3631167028862bE2a173976CA11");

sol! {
    /// Minimal rETH token interface — only ERC20 standard methods.
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IRocketTokenRETH {
        function decimals() external view returns (uint8);
        function totalSupply() external view returns (uint256);
    }

    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IRocketNetworkBalances {
        function getTotalETHBalance() external view returns (uint256);
    }

    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IRocketDepositPool {
        function getBalance() external view returns (uint256);
        function getExcessBalance() external view returns (uint256);
        function getMaximumDepositAmount() external view returns (uint256);

        event DepositReceived(address indexed from, uint256 amount, uint256 time);
        event DepositRecycled(address indexed from, uint256 amount, uint256 time);
        event DepositAssigned(address indexed minipool, uint256 amount, uint256 time);
        event ExcessWithdrawn(address indexed to, uint256 amount, uint256 time);
        event FundsRequested(address indexed receiver, uint256 validatorId, uint256 amount, bool expressQueue, uint256 time);
        event FundsAssigned(address indexed receiver, uint256 amount, uint256 time);
        event QueueExited(address indexed nodeAddress, uint256 time);
        event CreditWithdrawn(address indexed nodeAddress, uint256 amount, uint256 time);
    }

    /// Multicall3 interface for batched eth_calls.
    #[sol(rpc)]
    interface IMulticall3 {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }
        struct Result {
            bool success;
            bytes returnData;
        }
        function aggregate3(Call3[] calldata calls) external payable returns (Result[] memory returnData);
    }
}

// ── SolCall function signatures for Multicall3 ──────────────────────────

sol! {
    function getTotalETHBalance() external view returns (uint256);
    function totalSupply() external view returns (uint256);
    function getExcessBalance() external view returns (uint256);
    function getMaximumDepositAmount() external view returns (uint256);
    function getBalance() external view returns (uint256);
}

#[derive(Error, Debug)]
pub enum RocketPoolError {
    #[error("Insufficient collateral for redemption")]
    InsufficientCollateral,
    #[error("Unsupported conversion direction")]
    UnsupportedDirection,
    #[error("Division by zero")]
    DivisionByZero,
    #[error("Maximum deposit amount exceeded")]
    DepositCapacityExceeded,
}

/// Rocket Pool converter state.
///
/// The converter's `address` is the **RocketDepositPool** contract — the
/// entry-point for deposits and source of capacity events.  `token_0` is
/// rETH and `token_1` is the native ETH placeholder.
///
/// All data is fetched from chain via a single Multicall3 batch and then
/// exchange_rate / total_collateral are computed locally.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RocketPoolConverter {
    pub last_synced_block: u64,
    /// RocketDepositPool contract address (also the event routing key).
    pub address: Address,
    pub token_0: Address,
    pub token_1: Address,
    pub token_0_decimals: u8,
    pub token_1_decimals: u8,
    /// RocketNetworkBalances contract address (for `getTotalETHBalance`).
    pub network_balances_address: Address,
    /// Computed: `(total_eth_balance - excess_balance) * WAD / reth_supply`.
    pub exchange_rate: U256,
    /// Total ETH balance from RocketNetworkBalances.
    pub total_eth_balance: U256,
    /// rETH token totalSupply() (ERC20).
    pub reth_supply: U256,
    /// Computed: `total_eth_balance - excess_balance`.
    pub total_collateral: U256,
    /// Deposit-side remaining capacity from RocketDepositPool.
    pub maximum_deposit_amount: U256,
    /// Current ETH held by the deposit pool (RocketDepositPool).
    pub deposit_pool_balance: U256,
    /// Current excess ETH (RocketDepositPool).
    pub excess_balance: U256,
    pub token_0_price: f64,
    pub token_1_price: f64,
}

impl AutomatedMarketMaker for RocketPoolConverter {
    fn address(&self) -> Address {
        self.address
    }

    fn supported_chains(&self) -> Option<Vec<u64>> {
        Some(vec![1])
    }

    fn last_synced_block(&self) -> u64 {
        self.last_synced_block
    }

    fn set_last_synced_block(&mut self, block_number: u64) {
        self.last_synced_block = self.last_synced_block.max(block_number);
    }

    fn sync_events(&self) -> Vec<B256> {
        // Events emitted by RocketDepositPool that signal state changes.
        // rETH Transfer events are NOT routed (different address) — the
        // periodic sync task handles redemptions.
        vec![
            IRocketDepositPool::DepositReceived::SIGNATURE_HASH,
            IRocketDepositPool::DepositRecycled::SIGNATURE_HASH,
            IRocketDepositPool::DepositAssigned::SIGNATURE_HASH,
            IRocketDepositPool::ExcessWithdrawn::SIGNATURE_HASH,
            IRocketDepositPool::FundsRequested::SIGNATURE_HASH,
            IRocketDepositPool::FundsAssigned::SIGNATURE_HASH,
            IRocketDepositPool::QueueExited::SIGNATURE_HASH,
            IRocketDepositPool::CreditWithdrawn::SIGNATURE_HASH,
        ]
    }

    fn sync(&mut self, _log: &Log) -> Result<SyncAction, AMMError> {
        // Event tells us state changed but not to what — schedule full update.
        Ok(SyncAction::AsyncUpdate)
    }

    fn tokens(&self) -> Vec<Address> {
        vec![self.token_0, self.token_1]
    }

    fn has_sufficient_liquidity(&self) -> bool {
        let min_threshold = U256::from(100_000_000_000_000_000u128); // 0.1 ETH
        self.total_collateral >= min_threshold || self.maximum_deposit_amount >= min_threshold
    }

    fn decimals(&self, token: Address) -> u8 {
        if token == self.token_0 {
            self.token_0_decimals
        } else if token == self.token_1 {
            self.token_1_decimals
        } else {
            0
        }
    }

    fn calculate_price(&self, base_token: Address, _quote_token: Address) -> Result<f64, AMMError> {
        q64_to_float(self.calculate_price_64_x_64(base_token)?)
    }

    fn spot_price(&self, base_token: Address, _quote_token: Address) -> Result<f64, AMMError> {
        let price = if base_token == self.token_0 {
            self.token_0_price
        } else if base_token == self.token_1 {
            self.token_1_price
        } else {
            return Err(AMMError::TokenNotFound(base_token));
        };

        if price <= 0.0 || !price.is_finite() {
            return Err(AMMError::Msg("Invalid cached spot price".to_string()));
        }

        Ok(price)
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

        if base_token == self.token_0 {
            let amount_out = self.reth_to_eth(amount_in)?;
            if amount_out > self.total_collateral {
                return Err(RocketPoolError::InsufficientCollateral.into());
            }
            return Ok(amount_out);
        }

        if base_token == self.token_1 {
            if amount_in > self.maximum_deposit_amount {
                return Err(RocketPoolError::DepositCapacityExceeded.into());
            }
            return self.eth_to_reth(amount_in);
        }

        Err(RocketPoolError::UnsupportedDirection.into())
    }

    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let amount_out = self.simulate_swap(base_token, quote_token, amount_in)?;
        if base_token == self.token_0 {
            self.total_eth_balance = self
                .total_eth_balance
                .checked_sub(amount_out)
                .ok_or(AMMError::ArithmeticError)?;
            self.reth_supply = self
                .reth_supply
                .checked_sub(amount_in)
                .ok_or(AMMError::ArithmeticError)?;
            self.total_collateral = self
                .total_collateral
                .checked_sub(amount_out)
                .ok_or(AMMError::ArithmeticError)?;
            self.deposit_pool_balance = self.deposit_pool_balance.saturating_sub(amount_out);
            self.excess_balance = self.excess_balance.saturating_sub(amount_out);
            self.maximum_deposit_amount = self
                .maximum_deposit_amount
                .checked_add(amount_out)
                .ok_or(AMMError::ArithmeticError)?;
        } else if base_token == self.token_1 {
            self.total_eth_balance = self
                .total_eth_balance
                .checked_add(amount_in)
                .ok_or(AMMError::ArithmeticError)?;
            self.reth_supply = self
                .reth_supply
                .checked_add(amount_out)
                .ok_or(AMMError::ArithmeticError)?;
            self.total_collateral = self
                .total_collateral
                .checked_add(amount_in)
                .ok_or(AMMError::ArithmeticError)?;
            self.deposit_pool_balance = self
                .deposit_pool_balance
                .checked_add(amount_in)
                .ok_or(AMMError::ArithmeticError)?;
            self.excess_balance = self
                .excess_balance
                .checked_add(amount_in)
                .ok_or(AMMError::ArithmeticError)?;
            self.maximum_deposit_amount = self.maximum_deposit_amount.saturating_sub(amount_in);
        }
        // Recompute exchange_rate from the directly-mutated total_collateral
        // and reth_supply (NOT from total_eth - excess, which would be
        // incorrect when excess and total_eth are both decremented).
        self.exchange_rate = if self.reth_supply.is_zero() || self.total_collateral.is_zero() {
            U256::from(WAD)
        } else {
            U256::from(WAD)
                .checked_mul(self.total_collateral)
                .unwrap_or(U256::from(WAD))
                / self.reth_supply
        };
        self.refresh_prices()?;
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

        if base_token == self.token_0 {
            if amount_out > self.total_collateral {
                return Err(RocketPoolError::InsufficientCollateral.into());
            }
            return self.reth_to_eth_input(amount_out);
        }

        if base_token == self.token_1 {
            let amount_in = self.eth_to_reth_input(amount_out)?;
            if amount_in > self.maximum_deposit_amount {
                return Err(RocketPoolError::DepositCapacityExceeded.into());
            }
            return Ok(amount_in);
        }

        Err(RocketPoolError::UnsupportedDirection.into())
    }

    /// Fetch all state via a single Multicall3 batch, then derive
    /// exchange_rate and total_collateral locally.
    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let decimals = IRocketTokenRETH::new(self.token_0, provider.clone())
            .decimals()
            .block(block_number)
            .call()
            .await?;

        let (total_eth, reth_supply, excess, max_deposit, balance) =
            Self::fetch_all(block_number, provider, self.token_0, self.network_balances_address, self.address).await?;

        self.token_1 = NATIVE_ETH_PLACEHOLDER;
        self.token_0_decimals = decimals;
        self.token_1_decimals = 18;
        self.total_eth_balance = total_eth;
        self.reth_supply = reth_supply;
        self.excess_balance = excess;
        self.maximum_deposit_amount = max_deposit;
        self.deposit_pool_balance = balance;
        self.recompute_state();
        self.refresh_prices()?;

        info!(
            target: "amms::rocketpool::init",
            address = ?self.address,
            exchange_rate = ?self.exchange_rate,
            total_collateral = ?self.total_collateral,
            total_eth_balance = ?self.total_eth_balance,
            reth_supply = ?self.reth_supply,
            maximum_deposit_amount = ?self.maximum_deposit_amount,
            "Rocket Pool converter initialized"
        );

        Ok(self)
    }

    /// Refresh all state via Multicall3.
    async fn update<N, P>(&mut self, provider: P) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let (total_eth, reth_supply, excess, max_deposit, balance) =
            Self::fetch_all(BlockId::default(), provider, self.token_0, self.network_balances_address, self.address).await?;

        self.total_eth_balance = total_eth;
        self.reth_supply = reth_supply;
        self.excess_balance = excess;
        self.maximum_deposit_amount = max_deposit;
        self.deposit_pool_balance = balance;
        self.recompute_state();
        self.refresh_prices()?;
        Ok(())
    }
}

impl RocketPoolConverter {
    /// Creates a new Rocket Pool converter.
    ///
    /// # Parameters
    ///
    /// * `deposit_pool` — `RocketDepositPool` contract address (also the event
    ///   routing key; returned by [`address()`](AutomatedMarketMaker::address)).
    /// * `reth` — `RocketTokenRETH` token address (`token_0`).
    /// * `network_balances` — `RocketNetworkBalances` contract address (used
    ///   to fetch `getTotalETHBalance()` via Multicall3).
    ///
    /// For mainnet use the pre-defined constants in [`addresses`]:
    ///
    /// ```ignore
    /// use amms::amms::rocketpool::{RocketPoolConverter, addresses};
    /// let converter = RocketPoolConverter::new(
    ///     addresses::ROCKET_DEPOSIT_POOL,
    ///     addresses::RETH,
    ///     addresses::ROCKET_NETWORK_BALANCES,
    /// );
    /// ```
    pub fn new(deposit_pool: Address, reth: Address, network_balances: Address) -> Self {
        Self {
            address: deposit_pool,
            token_0: reth,
            token_1: NATIVE_ETH_PLACEHOLDER,
            token_0_decimals: 18,
            token_1_decimals: 18,
            network_balances_address: network_balances,
            exchange_rate: U256::from(WAD),
            ..Default::default()
        }
    }

    // ------------------------------------------------------------------
    //  Multicall3 batch fetcher
    // ------------------------------------------------------------------

    /// Fetch the 5 raw data points in a single RPC call.
    ///
    /// # Parameters
    ///
    /// * `reth` — `RocketTokenRETH` address (for `totalSupply`).
    /// * `network_balances` — `RocketNetworkBalances` address (for `getTotalETHBalance`).
    /// * `deposit_pool` — `RocketDepositPool` address (for deposit-pool view calls).
    async fn fetch_all<N, P>(
        block_number: BlockId,
        provider: P,
        reth: Address,
        network_balances: Address,
        deposit_pool: Address,
    ) -> Result<(U256, U256, U256, U256, U256), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let multicall = IMulticall3::new(MULTICALL3_ADDRESS, provider);

        let calls = vec![
            IMulticall3::Call3 {
                target: network_balances,
                allowFailure: false,
                callData: getTotalETHBalanceCall {}.abi_encode().into(),
            },
            IMulticall3::Call3 {
                target: reth,
                allowFailure: false,
                callData: totalSupplyCall {}.abi_encode().into(),
            },
            IMulticall3::Call3 {
                target: deposit_pool,
                allowFailure: false,
                callData: getExcessBalanceCall {}.abi_encode().into(),
            },
            IMulticall3::Call3 {
                target: deposit_pool,
                allowFailure: false,
                callData: getMaximumDepositAmountCall {}.abi_encode().into(),
            },
            IMulticall3::Call3 {
                target: deposit_pool,
                allowFailure: false,
                callData: getBalanceCall {}.abi_encode().into(),
            },
        ];

        let results = multicall
            .aggregate3(calls)
            .block(block_number)
            .call()
            .await?;

        // results is Vec<Result>, each Result has {success, returnData}
        let total_eth = <getTotalETHBalanceCall as SolCall>::abi_decode_returns(
            &results[0].returnData,
        )?;
        let supply = <totalSupplyCall as SolCall>::abi_decode_returns(&results[1].returnData)?;
        let excess = <getExcessBalanceCall as SolCall>::abi_decode_returns(
            &results[2].returnData,
        )?;
        let max_deposit = <getMaximumDepositAmountCall as SolCall>::abi_decode_returns(
            &results[3].returnData,
        )?;
        let balance = <getBalanceCall as SolCall>::abi_decode_returns(&results[4].returnData)?;

        Ok((total_eth, supply, excess, max_deposit, balance))
    }

    // ------------------------------------------------------------------
    //  Derived state (local computation)
    // ------------------------------------------------------------------

    /// Recompute `total_collateral` and `exchange_rate` from raw fields.
    fn recompute_state(&mut self) {
        // total_collateral = total_eth_balance - excess_balance
        self.total_collateral = self.total_eth_balance.saturating_sub(self.excess_balance);

        // exchange_rate = total_collateral * WAD / reth_supply
        self.exchange_rate = if self.reth_supply.is_zero() || self.total_collateral.is_zero() {
            U256::from(WAD)
        } else {
            U256::from(WAD)
                .checked_mul(self.total_collateral)
                .unwrap_or(U256::from(WAD))
                / self.reth_supply
        };
    }

    // ------------------------------------------------------------------
    //  Price helpers
    // ------------------------------------------------------------------

    pub fn calculate_price_64_x_64(&self, base_token: Address) -> Result<u128, AMMError> {
        if self.exchange_rate.is_zero() {
            return Err(RocketPoolError::DivisionByZero.into());
        }

        let price_q64: U256 = if base_token == self.token_0 {
            (self.exchange_rate << 64) / U256::from(WAD)
        } else if base_token == self.token_1 {
            (U256::from(WAD) << 64) / self.exchange_rate
        } else {
            return Err(AMMError::TokenNotFound(base_token));
        };

        Ok(price_q64.to())
    }

    // ------------------------------------------------------------------
    //  Conversion math
    // ------------------------------------------------------------------

    pub fn reth_to_eth(&self, amount_in: U256) -> Result<U256, AMMError> {
        if self.exchange_rate.is_zero() {
            return Err(RocketPoolError::DivisionByZero.into());
        }
        Ok(amount_in
            .checked_mul(self.exchange_rate)
            .ok_or(AMMError::ArithmeticError)?
            / U256::from(WAD))
    }

    pub fn eth_to_reth(&self, amount_in: U256) -> Result<U256, AMMError> {
        if self.exchange_rate.is_zero() {
            return Err(RocketPoolError::DivisionByZero.into());
        }
        Ok(amount_in
            .checked_mul(U256::from(WAD))
            .ok_or(AMMError::ArithmeticError)?
            / self.exchange_rate)
    }

    pub fn eth_to_reth_input(&self, amount_out: U256) -> Result<U256, AMMError> {
        if self.exchange_rate.is_zero() {
            return Err(RocketPoolError::DivisionByZero.into());
        }
        let numerator = amount_out
            .checked_mul(self.exchange_rate)
            .ok_or(AMMError::ArithmeticError)?;
        let wad = U256::from(WAD);
        let mut amount_in = numerator / wad;
        if numerator % wad != U256::ZERO {
            amount_in = amount_in
                .checked_add(U256::from(1u64))
                .ok_or(AMMError::ArithmeticError)?;
        }
        Ok(amount_in)
    }

    pub fn reth_to_eth_input(&self, amount_out: U256) -> Result<U256, AMMError> {
        if self.exchange_rate.is_zero() {
            return Err(RocketPoolError::DivisionByZero.into());
        }
        let numerator = amount_out
            .checked_mul(U256::from(WAD))
            .ok_or(AMMError::ArithmeticError)?;
        let mut amount_in = numerator / self.exchange_rate;
        if numerator % self.exchange_rate != U256::ZERO {
            amount_in = amount_in
                .checked_add(U256::from(1u64))
                .ok_or(AMMError::ArithmeticError)?;
        }
        Ok(amount_in)
    }

    pub fn refresh_prices(&mut self) -> Result<(), AMMError> {
        self.token_0_price = self.calculate_price(self.token_0, self.token_1)?;
        self.token_1_price = self.calculate_price(self.token_1, self.token_0)?;
        Ok(())
    }

    // ------------------------------------------------------------------
    //  Batch init
    // ------------------------------------------------------------------

    pub async fn init_batch<N, P>(
        amms: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let total = amms.len();
        let mut initialized = Vec::with_capacity(total);

        for amm in amms {
            match amm {
                AMM::RocketPoolConverter(converter) => {
                    let addr = converter.address;
                    match converter.init(block_number, provider.clone()).await {
                        Ok(init_converter) => {
                            initialized.push(AMM::RocketPoolConverter(init_converter));
                        }
                        Err(e) => {
                            info!(
                                target: "amms::rocketpool::init_batch",
                                address = ?addr,
                                error = ?e,
                                "Failed to initialize Rocket Pool converter"
                            );
                        }
                    }
                }
                _ => {
                    info!(
                        target: "amms::rocketpool::init_batch",
                        "Non-RocketPool converter in batch, skipping"
                    );
                }
            }
        }

        let valid = initialized.len();
        let invalid = total - valid;
        info!(
            target: "amms::rocketpool::init_batch",
            total,
            valid,
            invalid,
            "Batch initialization complete"
        );

        Ok(initialized)
    }

    // ------------------------------------------------------------------
    //  Helpers
    // ------------------------------------------------------------------

    pub fn has_token(&self, token: Address) -> bool {
        self.token_0 == token || self.token_1 == token
    }

    pub fn get_other_token(&self, token: Address) -> Option<Address> {
        if token == self.token_0 {
            Some(self.token_1)
        } else if token == self.token_1 {
            Some(self.token_0)
        } else {
            None
        }
    }
}

/// Known Rocket Pool addresses on Ethereum mainnet.
pub mod addresses {
    use alloy::primitives::{address, Address};

    pub const RETH: Address = address!("ae78736Cd615f374D3085123A210448E74Fc6393");
    pub const ROCKET_DEPOSIT_POOL: Address =
        address!("ce15294273cfb9d9b628f4d61636623decdf4fdc");
    pub const ROCKET_NETWORK_BALANCES: Address =
        address!("07FCaBCbe4ff0d80c2b1eb42855C0131b6cba2F4");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_converter() -> RocketPoolConverter {
        RocketPoolConverter::new(
            addresses::ROCKET_DEPOSIT_POOL,
            addresses::RETH,
            addresses::ROCKET_NETWORK_BALANCES,
        )
    }

    // ------------------------------------------------------------------
    //  reth_to_eth  /  eth_to_reth
    // ------------------------------------------------------------------

    #[test]
    fn test_reth_to_eth_simulation() {
        let mut converter = test_converter();
        converter.total_eth_balance = U256::from(115u128) * U256::from(WAD);
        converter.reth_supply = U256::from(100u128) * U256::from(WAD);
        converter.excess_balance = U256::from(15u128) * U256::from(WAD);
        converter.total_collateral = U256::from(100u128) * U256::from(WAD); // 115-15
        converter.maximum_deposit_amount = U256::from(100u128) * U256::from(WAD);
        converter.exchange_rate = U256::from(1_150_000_000_000_000_000u128);
        converter.refresh_prices().unwrap();

        let amount_in = U256::from(WAD);
        let amount_out = converter
            .simulate_swap(converter.token_0, converter.token_1, amount_in)
            .unwrap();
        // 1 rETH * 1.15 / 1.0 = 1.15 ETH
        assert_eq!(amount_out, U256::from(1_150_000_000_000_000_000u128));
    }

    #[test]
    fn test_eth_to_reth_simulation() {
        let mut converter = test_converter();
        converter.total_eth_balance = U256::from(115u128) * U256::from(WAD);
        converter.reth_supply = U256::from(100u128) * U256::from(WAD);
        converter.excess_balance = U256::from(15u128) * U256::from(WAD);
        converter.total_collateral = U256::from(100u128) * U256::from(WAD);
        converter.maximum_deposit_amount = U256::from(100u128) * U256::from(WAD);
        converter.exchange_rate = U256::from(1_150_000_000_000_000_000u128);
        converter.refresh_prices().unwrap();

        let amount_in = U256::from(WAD);
        let amount_out = converter
            .simulate_swap(converter.token_1, converter.token_0, amount_in)
            .unwrap();
        // 1 ETH / 1.15 = 0.869565217391304347 rETH
        assert_eq!(amount_out, U256::from(869_565_217_391_304_347u128));
    }

    #[test]
    fn test_insufficient_collateral() {
        let mut converter = test_converter();
        converter.total_eth_balance = U256::from(115u128) * U256::from(WAD);
        converter.reth_supply = U256::from(100u128) * U256::from(WAD);
        converter.excess_balance = U256::from(114u128) * U256::from(WAD);
        converter.total_collateral = U256::from(WAD); // 115-114 = 1 ETH
        converter.maximum_deposit_amount = U256::from(100u128) * U256::from(WAD);
        converter.exchange_rate = U256::from(1_150_000_000_000_000_000u128);
        converter.refresh_prices().unwrap();

        let amount_in = U256::from(200u128) * U256::from(WAD);
        let err = converter
            .simulate_swap(converter.token_0, converter.token_1, amount_in)
            .unwrap_err();
        assert!(matches!(
            err,
            AMMError::RocketPoolError(RocketPoolError::InsufficientCollateral)
        ));
    }

    #[test]
    fn test_deposit_capacity_exceeded() {
        let mut converter = test_converter();
        converter.total_eth_balance = U256::from(115u128) * U256::from(WAD);
        converter.reth_supply = U256::from(100u128) * U256::from(WAD);
        converter.excess_balance = U256::from(15u128) * U256::from(WAD);
        converter.total_collateral = U256::from(100u128) * U256::from(WAD);
        converter.maximum_deposit_amount = U256::from(WAD);
        converter.exchange_rate = U256::from(1_150_000_000_000_000_000u128);
        converter.refresh_prices().unwrap();

        let amount_in = U256::from(2u128) * U256::from(WAD);
        let err = converter
            .simulate_swap(converter.token_1, converter.token_0, amount_in)
            .unwrap_err();
        assert!(matches!(
            err,
            AMMError::RocketPoolError(RocketPoolError::DepositCapacityExceeded)
        ));
    }

    // ------------------------------------------------------------------
    //  Exact-out
    // ------------------------------------------------------------------

    #[test]
    fn test_exact_out_reth_to_eth_rounds_up() {
        let mut converter = test_converter();
        converter.total_eth_balance = U256::from(11u128) * U256::from(WAD);
        converter.reth_supply = U256::from(10u128) * U256::from(WAD);
        converter.excess_balance = U256::ZERO;
        converter.total_collateral = U256::from(100u128) * U256::from(WAD);
        converter.maximum_deposit_amount = U256::from(100u128) * U256::from(WAD);
        converter.exchange_rate = U256::from(1_100_000_000_000_000_000u128);
        converter.refresh_prices().unwrap();

        let eth_out = U256::from(WAD);
        let reth_in = converter
            .simulate_swap_exact_out(converter.token_0, converter.token_1, eth_out)
            .unwrap();
        assert_eq!(reth_in, U256::from(909_090_909_090_909_091u128));

        let eth_received = converter
            .simulate_swap(converter.token_0, converter.token_1, reth_in)
            .unwrap();
        assert!(eth_received >= eth_out);
    }

    #[test]
    fn test_exact_out_eth_to_reth_rounds_up() {
        let mut converter = test_converter();
        converter.total_eth_balance = U256::from(115u128) * U256::from(WAD);
        converter.reth_supply = U256::from(100u128) * U256::from(WAD);
        converter.excess_balance = U256::from(15u128) * U256::from(WAD);
        converter.total_collateral = U256::from(100u128) * U256::from(WAD);
        converter.maximum_deposit_amount = U256::from(100u128) * U256::from(WAD);
        converter.exchange_rate = U256::from(1_150_000_000_000_000_000u128);
        converter.refresh_prices().unwrap();

        let target_out = U256::from(WAD);
        let amount_in = converter
            .simulate_swap_exact_out(converter.token_1, converter.token_0, target_out)
            .unwrap();
        assert_eq!(amount_in, U256::from(1_150_000_000_000_000_000u128));

        let reth_received = converter
            .simulate_swap(converter.token_1, converter.token_0, amount_in)
            .unwrap();
        assert!(reth_received >= target_out);
    }

    // ------------------------------------------------------------------
    //  recompute_state
    // ------------------------------------------------------------------

    #[test]
    fn test_recompute_state() {
        let mut converter = test_converter();
        converter.total_eth_balance = U256::from(115u128) * U256::from(WAD);
        converter.reth_supply = U256::from(100u128) * U256::from(WAD);
        converter.excess_balance = U256::from(15u128) * U256::from(WAD);
        converter.recompute_state();

        // total_collateral = 115 - 15 = 100
        assert_eq!(
            converter.total_collateral,
            U256::from(100u128) * U256::from(WAD)
        );
        // exchange_rate = 100 * WAD / 100 = 1.0  (not 1.15!)
        // Wait — with these values total_collateral = 100, reth_supply = 100 → rate = 1.0
        assert_eq!(converter.exchange_rate, U256::from(WAD));
    }

    // ------------------------------------------------------------------
    //  Edge cases
    // ------------------------------------------------------------------

    #[test]
    fn test_zero_amount() {
        let mut converter = test_converter();
        converter.total_eth_balance = U256::from(115u128) * U256::from(WAD);
        converter.reth_supply = U256::from(100u128) * U256::from(WAD);
        converter.excess_balance = U256::from(15u128) * U256::from(WAD);
        converter.total_collateral = U256::from(100u128) * U256::from(WAD);
        converter.maximum_deposit_amount = U256::from(100u128) * U256::from(WAD);
        converter.exchange_rate = U256::from(1_150_000_000_000_000_000u128);
        converter.refresh_prices().unwrap();

        assert!(converter
            .simulate_swap(converter.token_0, converter.token_1, U256::ZERO)
            .unwrap()
            .is_zero());
        assert!(converter
            .simulate_swap(converter.token_1, converter.token_0, U256::ZERO)
            .unwrap()
            .is_zero());
        assert!(converter
            .simulate_swap_exact_out(converter.token_0, converter.token_1, U256::ZERO)
            .unwrap()
            .is_zero());
    }

    #[test]
    fn test_unsupported_direction() {
        let converter = test_converter();
        let fake = address!("0000000000000000000000000000000000000001");
        let err = converter
            .simulate_swap(fake, converter.token_0, U256::from(WAD))
            .unwrap_err();
        assert!(matches!(
            err,
            AMMError::RocketPoolError(RocketPoolError::UnsupportedDirection)
        ));
    }

    #[test]
    fn test_division_by_zero() {
        let mut converter = test_converter();
        converter.exchange_rate = U256::ZERO;
        let err = converter
            .simulate_swap(converter.token_0, converter.token_1, U256::from(WAD))
            .unwrap_err();
        assert!(matches!(
            err,
            AMMError::RocketPoolError(RocketPoolError::DivisionByZero)
        ));
    }

    // ------------------------------------------------------------------
    //  simulate_swap_mut  state transitions
    // ------------------------------------------------------------------

    #[test]
    fn test_simulate_swap_mut_state_transition() {
        let mut converter = test_converter();
        converter.total_eth_balance = U256::from(115u128) * U256::from(WAD);
        converter.reth_supply = U256::from(100u128) * U256::from(WAD);
        converter.excess_balance = U256::ZERO;
        converter.total_collateral = U256::from(115u128) * U256::from(WAD);
        converter.maximum_deposit_amount = U256::from(100u128) * U256::from(WAD);
        converter.deposit_pool_balance = U256::from(50u128) * U256::from(WAD);
        converter.exchange_rate = U256::from(1_150_000_000_000_000_000u128);
        converter.refresh_prices().unwrap();

        // rETH → ETH
        let amount_in = U256::from(10u128) * U256::from(WAD);
        let amount_out = converter
            .simulate_swap_mut(converter.token_0, converter.token_1, amount_in)
            .unwrap();
        assert_eq!(amount_out, U256::from(11_500_000_000_000_000_000u128));

        assert_eq!(converter.reth_supply, U256::from(90u128) * U256::from(WAD));
        assert_eq!(
            converter.total_eth_balance,
            U256::from(103_500_000_000_000_000_000u128)
        ); // 115 - 11.5

        // ETH → rETH
        let amount_in_2 = U256::from(5u128) * U256::from(WAD);
        let amount_out_2 = converter
            .simulate_swap_mut(converter.token_1, converter.token_0, amount_in_2)
            .unwrap();
        // 5 * 90 / 103.5 = 4.347826086956521739 (exact since coll = eth - excess)
        assert_eq!(amount_out_2, U256::from(4_347_826_086_956_521_739u128));
    }

    // ------------------------------------------------------------------
    //  Helpers
    // ------------------------------------------------------------------

    #[test]
    fn test_has_token_and_other() {
        let converter = test_converter();
        assert!(converter.has_token(converter.token_0));
        assert!(converter.has_token(converter.token_1));
        assert!(!converter.has_token(Address::ZERO));

        assert_eq!(converter.get_other_token(converter.token_0), Some(converter.token_1));
        assert_eq!(converter.get_other_token(converter.token_1), Some(converter.token_0));
        assert_eq!(converter.get_other_token(Address::ZERO), None);
    }
}

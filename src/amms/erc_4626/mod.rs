use super::{
    amm::{AutomatedMarketMaker, SyncAction, AMM},
    consts::{U128_0X10000000000000000, U256_10000, U256_2},
    error::AMMError,
    float::q64_to_float,
    uniswap_v2::div_uu,
};
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, B256, U256},
    providers::Provider,
    rpc::types::Log,
    sol,
    sol_types::{SolEvent, SolValue},
};
use futures::{stream::FuturesUnordered, StreamExt};
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, collections::HashMap};
use thiserror::Error;
use tracing::info;

sol! {
    /// Interface of the IERC4626Valut contract
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IERC4626Vault {
        event Withdraw(address indexed sender, address indexed receiver, address indexed owner, uint256 assets, uint256 shares);
        event Deposit(address indexed sender,address indexed owner, uint256 assets, uint256 shares);
        function totalAssets() external view returns (uint256);
        function totalSupply() external view returns (uint256);
    }
}

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    IGetERC4626VaultDataBatchRequest,
    "src/amms/abi/GetERC4626VaultDataBatchRequest.json",
}

#[derive(Error, Debug)]
pub enum ERC4626VaultError {
    #[error("Non relative or zero fee")]
    NonRelativeOrZeroFee,
    #[error("Division by zero")]
    DivisionByZero,
    #[error("Initialization error")]
    InitializationError,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ERC4626Vault {
    pub last_synced_block: u64,
    /// Token received from depositing, i.e. shares token
    pub vault_token: Address,
    pub vault_token_decimals: u8,
    /// Token received from withdrawing, i.e. underlying token
    pub asset_token: Address,
    pub asset_token_decimals: u8,
    /// Total supply of vault tokens
    pub vault_reserve: U256,
    /// Total balance of asset tokens held by vault
    pub asset_reserve: U256,
    /// Deposit fee in basis points
    pub deposit_fee: u32,
    /// Withdrawal fee in basis points
    pub withdraw_fee: u32,
    pub vault_token_price: f64,
    pub asset_token_price: f64,
}

impl AutomatedMarketMaker for ERC4626Vault {
    fn address(&self) -> Address {
        self.vault_token
    }

    fn last_synced_block(&self) -> u64 {
        self.last_synced_block
    }

    fn set_last_synced_block(&mut self, block_number: u64) {
        self.last_synced_block = self.last_synced_block.max(block_number);
    }

    fn sync_events(&self) -> Vec<B256> {
        vec![
            IERC4626Vault::Deposit::SIGNATURE_HASH,
            IERC4626Vault::Withdraw::SIGNATURE_HASH,
        ]
    }

    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError> {
        let event_signature = log.data().topics()[0];
        match event_signature {
            IERC4626Vault::Deposit::SIGNATURE_HASH => {
                let deposit_event = IERC4626Vault::Deposit::decode_log(log.as_ref())?;
                self.asset_reserve += deposit_event.assets;
                self.vault_reserve += deposit_event.shares;

                info!(
                    target = "amms::erc_4626::sync",
                    address = ?self.vault_token,
                    asset_reserve = ?self.asset_reserve,
                    vault_reserve = ?self.vault_reserve,
                    "Deposit"
                );
            }

            IERC4626Vault::Withdraw::SIGNATURE_HASH => {
                let withdraw_event = IERC4626Vault::Withdraw::decode_log(log.as_ref())?;
                self.asset_reserve -= withdraw_event.assets;
                self.vault_reserve -= withdraw_event.shares;

                info!(
                    target = "amms::erc_4626::sync",
                    address = ?self.vault_token,
                    asset_reserve = ?self.asset_reserve,
                    vault_reserve = ?self.vault_reserve,
                    "Withdraw"
                );
            }

            _ => {
                // Ignore non-ERC4626 events (e.g., ERC20 Transfer) to avoid noisy logs.
                return Ok(SyncAction::None);
            }
        }

        // Update spot prices
        self.vault_token_price = self.calculate_price(self.vault_token, self.asset_token)?;
        self.asset_token_price = self.calculate_price(self.asset_token, self.vault_token)?;

        Ok(SyncAction::None)
    }

    fn tokens(&self) -> Vec<Address> {
        vec![self.vault_token, self.asset_token]
    }

    fn has_sufficient_liquidity(&self) -> bool {
        let check_reserve = |reserve: U256, decimals: u8| -> bool {
            if decimals >= 18 {
                // 0.0001 unit
                reserve >= U256::from(10).pow(U256::from(decimals.saturating_sub(4)))
            } else if decimals >= 6 {
                // 100 units
                let threshold =
                    U256::from(100).saturating_mul(U256::from(10).pow(U256::from(decimals)));
                reserve >= threshold
            } else {
                reserve >= U256::from(100_000)
            }
        };

        check_reserve(self.asset_reserve, self.asset_token_decimals)
            && check_reserve(self.vault_reserve, self.vault_token_decimals)
    }

    fn decimals(&self, token: Address) -> u8 {
        if token == self.vault_token {
            self.vault_token_decimals
        } else if token == self.asset_token {
            self.asset_token_decimals
        } else {
            0
        }
    }

    fn calculate_price(&self, base_token: Address, _quote_token: Address) -> Result<f64, AMMError> {
        q64_to_float(self.calculate_price_64_x_64(base_token)?)
    }

    fn spot_price(&self, base_token: Address, _quote_token: Address) -> Result<f64, AMMError> {
        let price = if base_token == self.vault_token {
            self.vault_token_price
        } else if base_token == self.asset_token {
            self.asset_token_price
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
        _quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        if self.vault_token == base_token {
            Ok(self.get_amount_out(amount_in, self.vault_reserve, self.asset_reserve)?)
        } else {
            Ok(self.get_amount_out(amount_in, self.asset_reserve, self.vault_reserve)?)
        }
    }

    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        _quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        if self.vault_token == base_token {
            let amount_out =
                self.get_amount_out(amount_in, self.vault_reserve, self.asset_reserve)?;

            self.vault_reserve -= amount_in;
            self.asset_reserve -= amount_out;

            Ok(amount_out)
        } else {
            let amount_out =
                self.get_amount_out(amount_in, self.asset_reserve, self.vault_reserve)?;

            self.asset_reserve += amount_in;
            self.vault_reserve += amount_out;

            Ok(amount_out)
        }
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

        if self.vault_token == base_token {
            // Withdraw: vault_token -> asset_token
            self.get_amount_in(
                amount_out,
                self.vault_reserve,
                self.asset_reserve,
                self.withdraw_fee,
            )
        } else {
            // Deposit: asset_token -> vault_token
            self.get_amount_in(
                amount_out,
                self.asset_reserve,
                self.vault_reserve,
                self.deposit_fee,
            )
        }
    }

    // TODO: clean up this function
    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let deployer =
            IGetERC4626VaultDataBatchRequest::deploy_builder(provider, vec![self.vault_token]);
        let res = deployer.call_raw().block(block_number).await?;

        let data = <Vec<(
            Address,
            u16,
            Address,
            u16,
            U256,
            U256,
            U256,
            U256,
            U256,
            U256,
            U256,
            U256,
        )> as SolValue>::abi_decode(&res)?;
        let (
            vault_token,
            vault_token_dec,
            asset_token,
            asset_token_dec,
            vault_reserve,
            asset_reserve,
            deposit_fee_delta_1,
            deposit_fee_delta_2,
            deposit_no_fee,
            withdraw_fee_delta_1,
            withdraw_fee_delta_2,
            withdraw_no_fee,
        ) = if !data.is_empty() {
            data[0]
        } else {
            return Err(ERC4626VaultError::InitializationError)?;
        };

        // If both deltas are zero, the fee is zero
        if deposit_fee_delta_1.is_zero() && deposit_fee_delta_2.is_zero() {
            self.deposit_fee = 0;

        // Assuming 18 decimals, if the delta of 1e20 is half the delta of 2e20, relative fee.
        // Delta / (amount without fee / 10000) to give us the fee in basis points
        } else if deposit_fee_delta_1 * U256_2 == deposit_fee_delta_2 {
            self.deposit_fee = (deposit_fee_delta_1 / (deposit_no_fee / U256::from(10_000))).to();
        } else {
            todo!("Handle error")
        }

        // If both deltas are zero, the fee is zero
        if withdraw_fee_delta_1.is_zero() && withdraw_fee_delta_2.is_zero() {
            self.withdraw_fee = 0;
        // Assuming 18 decimals, if the delta of 1e20 is half the delta of 2e20, relative fee.
        // Delta / (amount without fee / 10000) to give us the fee in basis points
        } else if withdraw_fee_delta_1 * U256::from(2) == withdraw_fee_delta_2 {
            self.withdraw_fee =
                (withdraw_fee_delta_1 / (withdraw_no_fee / U256::from(10_000))).to();
        } else {
            // If not a relative fee or zero, ignore vault
            return Err(ERC4626VaultError::NonRelativeOrZeroFee.into());
        }

        // if above does not error => populate the vault
        self.vault_token = vault_token;
        self.vault_token_decimals = vault_token_dec as u8;
        self.asset_token = asset_token;
        self.asset_token_decimals = asset_token_dec as u8;
        self.vault_reserve = vault_reserve;
        self.asset_reserve = asset_reserve;

        // Calculate initial prices
        self.vault_token_price = self.calculate_price(self.vault_token, self.asset_token)?;
        self.asset_token_price = self.calculate_price(self.asset_token, self.vault_token)?;

        Ok(self)
    }
}

// TODO: swap calldata
impl ERC4626Vault {
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
        let step = 100;
        let vaults = amms
            .iter()
            .chunks(step)
            .into_iter()
            .map(|chunk| chunk.map(|amm| amm.address()).collect())
            .collect::<Vec<Vec<Address>>>();

        let mut futures_unordered = FuturesUnordered::new();
        for group in vaults {
            let deployer =
                IGetERC4626VaultDataBatchRequest::deploy_builder(provider.clone(), group.clone());

            futures_unordered.push(async move {
                let res = deployer.call_raw().block(block_number).await?;

                let return_data = <Vec<(
                    Address,
                    u16,
                    Address,
                    u16,
                    U256,
                    U256,
                    U256,
                    U256,
                    U256,
                    U256,
                    U256,
                    U256,
                )> as SolValue>::abi_decode(&res)?;

                Ok::<
                    (
                        Vec<Address>,
                        Vec<(
                            Address,
                            u16,
                            Address,
                            u16,
                            U256,
                            U256,
                            U256,
                            U256,
                            U256,
                            U256,
                            U256,
                            U256,
                        )>,
                    ),
                    AMMError,
                >((group, return_data))
            });
        }

        let mut amms = amms
            .into_iter()
            .map(|amm| (amm.address(), amm))
            .collect::<HashMap<_, _>>();

        while let Some(res) = futures_unordered.next().await {
            let (group, return_data) = res?;
            for (data, vault_address) in return_data.iter().zip(group.iter()) {
                let (
                    vault_token,
                    vault_token_dec,
                    asset_token,
                    asset_token_dec,
                    vault_reserve,
                    asset_reserve,
                    deposit_fee_delta_1,
                    deposit_fee_delta_2,
                    deposit_no_fee,
                    withdraw_fee_delta_1,
                    withdraw_fee_delta_2,
                    withdraw_no_fee,
                ) = data;

                if vault_token.is_zero() {
                    continue;
                }

                let deposit_fee;
                if deposit_fee_delta_1.is_zero() && deposit_fee_delta_2.is_zero() {
                    deposit_fee = 0;
                } else if deposit_fee_delta_1 * U256_2 == *deposit_fee_delta_2 {
                    deposit_fee =
                        (deposit_fee_delta_1 / (deposit_no_fee / U256::from(10_000))).to();
                } else {
                    tracing::warn!(?vault_address, "Invalid deposit fee delta");
                    continue;
                }

                let withdraw_fee;
                if withdraw_fee_delta_1.is_zero() && withdraw_fee_delta_2.is_zero() {
                    withdraw_fee = 0;
                } else if withdraw_fee_delta_1 * U256_2 == *withdraw_fee_delta_2 {
                    withdraw_fee =
                        (withdraw_fee_delta_1 / (withdraw_no_fee / U256::from(10_000))).to();
                } else {
                    tracing::warn!(?vault_address, "Invalid withdraw fee delta");
                    continue;
                }

                let amm = amms.get_mut(vault_address).unwrap();
                let AMM::ERC4626Vault(vault) = amm else {
                    panic!("Unexpected vault type")
                };

                vault.deposit_fee = deposit_fee;
                vault.withdraw_fee = withdraw_fee;
                vault.vault_token = *vault_token;
                vault.vault_token_decimals = *vault_token_dec as u8;
                vault.asset_token = *asset_token;
                vault.asset_token_decimals = *asset_token_dec as u8;
                vault.vault_reserve = *vault_reserve;
                vault.asset_reserve = *asset_reserve;

                if let Ok(p) = vault.calculate_price(vault.vault_token, vault.asset_token) {
                    vault.vault_token_price = p;
                }
                if let Ok(p) = vault.calculate_price(vault.asset_token, vault.vault_token) {
                    vault.asset_token_price = p;
                }
            }
        }

        let (valid_amms, invalid_amms): (Vec<_>, Vec<_>) = amms
            .into_iter()
            .partition(|(_, amm)| !amm.tokens().iter().any(|t| t.is_zero()));

        if !invalid_amms.is_empty() {
            for (_, amm) in &invalid_amms {
                info!(
                    target: "amms::erc_4626::init_batch",
                    address = ?amm.address(),
                    "Filtering out uninitialized vault"
                );
            }
        }

        let amms: Vec<AMM> = valid_amms.into_iter().map(|(_, amm)| amm).collect();

        let valid = amms.len();
        let invalid = invalid_amms.len();
        info!(
            target: "amms::erc_4626::init_batch",
            total = total,
            valid = valid,
            invalid = invalid,
            "Batch initialization complete"
        );

        Ok(amms)
    }

    // Returns a new, unsynced ERC4626 vault
    pub fn new(address: Address) -> Self {
        Self {
            vault_token: address,
            ..Default::default()
        }
    }

    pub fn get_amount_out(
        &self,
        amount_in: U256,
        reserve_in: U256,
        reserve_out: U256,
    ) -> Result<U256, AMMError> {
        if amount_in.is_zero() {
            return Ok(U256::ZERO);
        }

        if self.vault_reserve.is_zero() {
            return Ok(amount_in);
        }

        let fee = if reserve_in == self.vault_reserve {
            self.withdraw_fee
        } else {
            self.deposit_fee
        };

        if reserve_in.is_zero() || 10000 - fee == 0 {
            return Err(ERC4626VaultError::DivisionByZero.into());
        }

        Ok(amount_in * reserve_out / reserve_in * U256::from(10000 - fee) / U256_10000)
    }

    /// Calculate the amount of input tokens needed to receive a desired output amount.
    /// This is the inverse of get_amount_out.
    ///
    /// Formula derivation:
    /// amount_out = amount_in * reserve_out / reserve_in * (10000 - fee) / 10000
    /// =>
    /// amount_in = amount_out * reserve_in * 10000 / (reserve_out * (10000 - fee))
    ///
    /// We use ceiling division to ensure we get at least the desired output.
    pub fn get_amount_in(
        &self,
        amount_out: U256,
        reserve_in: U256,
        reserve_out: U256,
        fee: u32,
    ) -> Result<U256, AMMError> {
        if amount_out.is_zero() {
            return Ok(U256::ZERO);
        }

        if reserve_out.is_zero() {
            return Err(AMMError::Msg("insufficient liquidity".into()));
        }

        // Check if desired output exceeds available reserve
        if amount_out >= reserve_out {
            return Err(AMMError::Msg("insufficient liquidity for exact out".into()));
        }

        // fee cannot be 100% (10000 basis points)
        if fee >= 10000 {
            return Err(AMMError::Msg("fee too high".into()));
        }

        let fee_factor = U256::from(10000 - fee);

        // numerator = amount_out * reserve_in * 10000
        let numerator = amount_out
            .checked_mul(reserve_in)
            .and_then(|v| v.checked_mul(U256_10000))
            .ok_or(AMMError::ArithmeticError)?;

        // denominator = (reserve_out - amount_out) * fee_factor
        // Note: We subtract amount_out from reserve_out because we're computing
        // the input needed to get exactly amount_out, and the formula accounts
        // for the reserve change during the swap.
        let denominator = reserve_out
            .checked_sub(amount_out)
            .and_then(|v| v.checked_mul(fee_factor))
            .ok_or(AMMError::ArithmeticError)?;

        // Ceiling division: (numerator + denominator - 1) / denominator
        let result = numerator
            .checked_add(denominator)
            .and_then(|v| v.checked_sub(U256::from(1u64)))
            .and_then(|v| v.checked_div(denominator))
            .ok_or(AMMError::ArithmeticError)?;

        Ok(result)
    }

    pub fn calculate_price_64_x_64(&self, base_token: Address) -> Result<u128, AMMError> {
        let decimal_shift = self.vault_token_decimals as i8 - self.asset_token_decimals as i8;

        // Normalize reserves by decimal shift
        let (r_v, r_a) = match decimal_shift.cmp(&0) {
            Ordering::Less => (
                self.vault_reserve * U256::from(10u128.pow(decimal_shift.unsigned_abs() as u32)),
                self.asset_reserve,
            ),
            _ => (
                self.vault_reserve,
                self.asset_reserve * U256::from(10u128.pow(decimal_shift as u32)),
            ),
        };

        // Withdraw
        if base_token == self.vault_token {
            if r_v.is_zero() {
                // Return 1 in Q64
                Ok(U128_0X10000000000000000)
            } else {
                Ok(div_uu(r_a, r_v)?)
            }
        // Deposit
        } else if r_a.is_zero() {
            // Return 1 in Q64
            Ok(U128_0X10000000000000000)
        } else {
            Ok(div_uu(r_v, r_a)?)
        }
    }

    pub async fn get_reserves<N, P>(
        &self,
        provider: P,
        block_number: BlockId,
    ) -> Result<(U256, U256), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone + Clone,
    {
        let vault = IERC4626Vault::new(self.vault_token, provider);

        let total_assets = vault.totalAssets().block(block_number).call().await?;

        let total_supply = vault.totalSupply().block(block_number).call().await?;

        Ok((total_supply, total_assets))
    }
}

#[cfg(test)]
mod test_batch;
#[cfg(test)]
mod test_price;

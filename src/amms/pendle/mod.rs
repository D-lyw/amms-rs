//! PendlePool — Pendle Protocol AMM pool implementation.
//!
//! Exposes the [PT, Underlying] token pair while internally handling
//! the SY (StandardizedYield) intermediate layer.

mod math;
mod log_exp;

use math::*;

use crate::amms::{
    amm::{AutomatedMarketMaker, SyncAction},
    error::AMMError,
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
use alloy::primitives::{address, I256};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::info;

/// I256 18 位定点数 → f64（用于价格计算输出）
pub fn i256_to_f64(val: I256) -> f64 {
    let u = val.into_raw();
    let limbs = u.as_limbs();
    (limbs[0] as f64 + limbs[1] as f64 * 18446744073709551616.0) / 1e18
}

// ========================================================================
//  Solidity interface definitions
// ========================================================================

sol! {
    /// Market events used for event-driven state sync.
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IPMarketEvents {
        event Mint(
            address indexed receiver,
            uint256 netLpMinted,
            uint256 netSyUsed,
            uint256 netPtUsed
        );
        event Burn(
            address indexed receiverSy,
            address indexed receiverPt,
            uint256 netLpBurned,
            uint256 netSyOut,
            uint256 netPtOut
        );
        event Swap(
            address indexed caller,
            address indexed receiver,
            int256 netPtOut,
            int256 netSyOut,
            uint256 netSyFee,
            uint256 netSyToReserve
        );
        event UpdateImpliedRate(
            uint256 indexed timestamp,
            uint256 lnLastImpliedRate
        );
    }
}

sol! {
    /// PT (PrincipalToken) read functions.
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IPPrincipalTokenState {
        function SY() external view returns (address);
        function expiry() external view returns (uint256);
        function decimals() external view returns (uint8);
    }
}

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    GetPendlePoolDataBatchRequest,
    "src/amms/abi/GetPendlePoolDataBatchRequest.json",
}

// ── Multicall3 调用函数签名 ───────────────────────────────────────
sol! {
    function _storage() external view returns (int128 totalPt, int128 totalSy, uint96 lastLnImpliedRate, uint16, uint16, uint16);
    function exchangeRate() external view returns (uint256);
    function isExpired() external view returns (bool);

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

const MULTICALL3_ADDRESS: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");

// ========================================================================
//  Error type
// ========================================================================

#[derive(Error, Debug)]
pub enum PendleError {
    #[error("Market math error")]
    MathError,
    #[error("Division by zero")]
    DivisionByZero,
    #[error("Insufficient liquidity in market")]
    InsufficientLiquidity,
    #[error("Market expired")]
    MarketExpired,
    #[error("Proportion too high (>96%)")]
    ProportionTooHigh,
    #[error("Unsupported swap direction")]
    UnsupportedDirection,
}

// ========================================================================
//  PendlePool struct
// ========================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PendlePool {
    pub last_synced_block: u64,

    /// Market 合约地址（池的唯一标识）
    pub address: Address,

    // ── 对外 token pair: [PT, Underlying] ──
    pub pt_token: Address,
    pub pt_decimals: u8,
    pub underlying_token: Address,
    pub underlying_decimals: u8,

    // ── 内部关联合约地址 ──
    pub sy_address: Address,
    pub market_address: Address, // 同 self.address

    // ── Market AMM 状态（事件驱动同步） ──
    pub total_pt: U256,
    pub total_sy: U256,
    pub scalar_root: U256, // 不可变
    pub last_ln_implied_rate: U256,

    // ── 费用参数（静态，init 时获取） ──
    pub ln_fee_rate_root: U256,
    pub reserve_fee_percent: u8,

    // ── SY 状态（需定期 update 刷新） ──
    pub sy_exchange_rate: U256,

    // ── 到期状态 ──
    pub expiry: u64,
    pub is_expired: bool,

    /// 最近一次同步的区块时间戳，用于 simulate_swap 中的 timeToExpiry 计算
    pub last_block_timestamp: u64,

    // ── 缓存价格 ──
    pub token_0_price: f64, // PT 价格
    pub token_1_price: f64, // Underlying 价格
}

impl PendlePool {
    pub fn new(market_address: Address) -> Self {
        Self {
            address: market_address,
            market_address,
            ..Default::default()
        }
    }

    /// 获取当前区块时间戳的估计值（系统时间作为 fallback）
    fn block_timestamp(&self) -> u64 {
        if self.last_block_timestamp > 0 {
            self.last_block_timestamp
        } else {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        }
    }

    /// Refresh cached spot prices.
    fn refresh_prices(&mut self) -> Result<(), AMMError> {
        self.token_0_price = self.calculate_price(self.pt_token, self.underlying_token)?;
        self.token_1_price = self.calculate_price(self.underlying_token, self.pt_token)?;
        Ok(())
    }

    /// Batch initialization of multiple PendlePools.
    pub async fn init_batch<N, P>(
        amms: Vec<super::amm::AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<super::amm::AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let total = amms.len();
        let mut market_addresses = Vec::with_capacity(total);
        let mut pendle_indices = Vec::with_capacity(total);

        for (idx, amm) in amms.iter().enumerate() {
            if let super::amm::AMM::PendlePool(pool) = amm {
                market_addresses.push(pool.address);
                pendle_indices.push(idx);
            }
        }

        if market_addresses.is_empty() {
            return Ok(amms);
        }

        let deployer = GetPendlePoolDataBatchRequest::deploy_builder(
            provider.clone(),
            market_addresses,
        );
        let res = deployer.call_raw().block(block_number).await?;

        let batch_data = <Vec<(
            I256, I256, I256, U256, U256, U256, U256,
            Address, Address, U256, Address, u16, u16, U256,
        )> as SolValue>::abi_decode(&res)?;

        let mut result = amms;
        for (batch_idx, &pool_idx) in pendle_indices.iter().enumerate() {
            if batch_idx >= batch_data.len() {
                continue;
            }
            let data = &batch_data[batch_idx];
            if let super::amm::AMM::PendlePool(ref mut pool) = result[pool_idx] {
                pool.total_pt = data.0.into_raw();
                pool.total_sy = data.1.into_raw();
                pool.scalar_root = if data.2 >= I256::ZERO {
                    data.2.into_raw()
                } else {
                    info!(
                        target: "amms::pendle::init_batch",
                        address = ?pool.address,
                        "Negative scalarRoot, skipping pool"
                    );
                    continue;
                };
                pool.expiry = data.3.to();
                pool.ln_fee_rate_root = data.4;
                pool.reserve_fee_percent = data.5.to();
                pool.last_ln_implied_rate = data.6;
                pool.pt_token = data.7;
                pool.sy_address = data.8;
                pool.sy_exchange_rate = data.9;
                pool.underlying_token = data.10;
                pool.underlying_decimals = data.11 as u8;
                pool.pt_decimals = data.12 as u8;
                pool.last_block_timestamp = data.13.to();
                pool.is_expired = pool.expiry <= pool.last_block_timestamp;
                let _ = pool.refresh_prices();

                info!(
                    target: "amms::pendle::init_batch",
                    address = ?pool.address,
                    pt = ?pool.pt_token,
                    underlying = ?pool.underlying_token,
                    "PendlePool batch init complete"
                );
            }
        }

        Ok(result)
    }
}

// ========================================================================
//  AutomatedMarketMaker trait implementation
// ========================================================================

impl AutomatedMarketMaker for PendlePool {
    fn address(&self) -> Address {
        self.address
    }

    fn supported_chains(&self) -> Option<Vec<u64>> {
        Some(vec![1, 42161, 10, 8453, 534352, 146, 5000, 324, 59144])
    }

    fn last_synced_block(&self) -> u64 {
        self.last_synced_block
    }

    fn set_last_synced_block(&mut self, block_number: u64) {
        self.last_synced_block = self.last_synced_block.max(block_number);
    }

    fn sync_events(&self) -> Vec<B256> {
        vec![
            IPMarketEvents::Swap::SIGNATURE_HASH,
            IPMarketEvents::Mint::SIGNATURE_HASH,
            IPMarketEvents::Burn::SIGNATURE_HASH,
            IPMarketEvents::UpdateImpliedRate::SIGNATURE_HASH,
        ]
    }

    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError> {
        let event_signature = log.topics()[0];
        match event_signature {
            IPMarketEvents::Swap::SIGNATURE_HASH => {
                let ev = IPMarketEvents::Swap::decode_log(log.as_ref())?;

                // totalPt change = -(netPtOut)
                if ev.netPtOut > I256::ZERO {
                    // PT flows to user → market loses PT
                    self.total_pt = self.total_pt.saturating_sub(ev.netPtOut.into_raw());
                } else {
                    // PT flows to market → market gains PT
                    self.total_pt = self
                        .total_pt
                        .saturating_add((-ev.netPtOut).into_raw());
                }

                // totalSy change = -(netSyOut + netSyToReserve)
                let sy_total = ev.netSyOut + I256::from_raw(ev.netSyToReserve);
                if sy_total > I256::ZERO {
                    // SY flows to user + reserve → market loses SY
                    self.total_sy = self.total_sy.saturating_sub(sy_total.into_raw());
                } else {
                    // SY flows to market
                    self.total_sy = self
                        .total_sy
                        .saturating_add((-sy_total).into_raw());
                }

                info!(
                    target: "amms::pendle::sync",
                    address = ?self.address,
                    total_pt = ?self.total_pt,
                    total_sy = ?self.total_sy,
                    "Swap"
                );
            }
            IPMarketEvents::Mint::SIGNATURE_HASH => {
                let ev = IPMarketEvents::Mint::decode_log(log.as_ref())?;
                self.total_pt = self.total_pt.saturating_add(ev.netPtUsed);
                self.total_sy = self.total_sy.saturating_add(ev.netSyUsed);

                info!(
                    target: "amms::pendle::sync",
                    address = ?self.address,
                    total_pt = ?self.total_pt,
                    total_sy = ?self.total_sy,
                    "Mint"
                );
            }
            IPMarketEvents::Burn::SIGNATURE_HASH => {
                let ev = IPMarketEvents::Burn::decode_log(log.as_ref())?;
                self.total_pt = self.total_pt.saturating_sub(ev.netPtOut);
                self.total_sy = self.total_sy.saturating_sub(ev.netSyOut);

                info!(
                    target: "amms::pendle::sync",
                    address = ?self.address,
                    total_pt = ?self.total_pt,
                    total_sy = ?self.total_sy,
                    "Burn"
                );
            }
            IPMarketEvents::UpdateImpliedRate::SIGNATURE_HASH => {
                let ev = IPMarketEvents::UpdateImpliedRate::decode_log(log.as_ref())?;
                self.last_ln_implied_rate = ev.lnLastImpliedRate;
                let _ = self.refresh_prices();

                info!(
                    target: "amms::pendle::sync",
                    address = ?self.address,
                    last_ln_implied_rate = ?self.last_ln_implied_rate,
                    "UpdateImpliedRate"
                );
            }
            _ => {
                return Ok(SyncAction::None);
            }
        }

        Ok(SyncAction::None)
    }

    fn tokens(&self) -> Vec<Address> {
        vec![self.pt_token, self.underlying_token]
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

        let block_time = self.block_timestamp();

        let base_is_pt = base_token == self.pt_token;
        let base_is_underlying = base_token == self.underlying_token;

        if self.is_expired || self.expiry <= block_time {
            // 到期后 PT = SY (1:1)，但 SY → Underlying 仍需 exchangeRate 转换
            if base_is_pt {
                return Ok(sy_to_asset(amount_in, self.sy_exchange_rate));
            }
            if base_is_underlying {
                return Ok(asset_to_sy(amount_in, self.sy_exchange_rate));
            }
            return Err(AMMError::TokenNotFound(base_token));
        }

        if base_is_pt {
            // PT → SY (AMM) → Underlying (exchange rate)
            let (sy_out, _fee) = calc_swap_exact_pt_for_sy(
                self.total_pt,
                self.total_sy,
                self.scalar_root,
                self.last_ln_implied_rate,
                self.ln_fee_rate_root,
                self.reserve_fee_percent,
                self.sy_exchange_rate,
                self.expiry,
                block_time,
                amount_in,
            )?;
            Ok(sy_to_asset(sy_out, self.sy_exchange_rate))
        } else if base_is_underlying {
            // Underlying → SY (exchange rate) → PT (AMM reverse)
            let sy_amount_in = asset_to_sy(amount_in, self.sy_exchange_rate);
            if sy_amount_in.is_zero() {
                return Err(AMMError::DivisionByZero);
            }
            let pt_out = calc_swap_exact_sy_for_pt(
                self.total_pt,
                self.total_sy,
                self.scalar_root,
                self.last_ln_implied_rate,
                self.ln_fee_rate_root,
                self.reserve_fee_percent,
                self.sy_exchange_rate,
                self.expiry,
                block_time,
                sy_amount_in,
            )?;
            Ok(pt_out)
        } else {
            Err(AMMError::TokenNotFound(base_token))
        }
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

        let block_time = self.block_timestamp();

        let base_is_pt = base_token == self.pt_token;
        let base_is_underlying = base_token == self.underlying_token;

        if self.is_expired || self.expiry <= block_time {
            if base_is_pt {
                return Ok(sy_to_asset(amount_in, self.sy_exchange_rate));
            }
            if base_is_underlying {
                return Ok(asset_to_sy(amount_in, self.sy_exchange_rate));
            }
            return Err(AMMError::TokenNotFound(base_token));
        }

        if base_is_pt {
            // PT → SY → Underlying
            let (sy_out, _fee) = calc_swap_exact_pt_for_sy(
                self.total_pt,
                self.total_sy,
                self.scalar_root,
                self.last_ln_implied_rate,
                self.ln_fee_rate_root,
                self.reserve_fee_percent,
                self.sy_exchange_rate,
                self.expiry,
                block_time,
                amount_in,
            )?;
            let underlying_out = sy_to_asset(sy_out, self.sy_exchange_rate);

            // Update state
            self.total_pt = self.total_pt.saturating_add(amount_in);
            self.total_sy = self.total_sy.saturating_sub(sy_out);

            // Recompute implied rate
            if let Ok(new_rate) = calc_new_ln_implied_rate(
                self.total_pt,
                self.total_sy,
                self.scalar_root,
                self.sy_exchange_rate,
                self.last_ln_implied_rate,
                self.expiry,
                block_time,
            ) {
                self.last_ln_implied_rate = new_rate;
            }

            self.refresh_prices()?;
            Ok(underlying_out)
        } else if base_is_underlying {
            // Underlying → SY → PT
            let sy_amount_in = asset_to_sy(amount_in, self.sy_exchange_rate);
            if sy_amount_in.is_zero() {
                return Err(AMMError::DivisionByZero);
            }

            let pt_out = calc_swap_exact_sy_for_pt(
                self.total_pt,
                self.total_sy,
                self.scalar_root,
                self.last_ln_implied_rate,
                self.ln_fee_rate_root,
                self.reserve_fee_percent,
                self.sy_exchange_rate,
                self.expiry,
                block_time,
                sy_amount_in,
            )?;

            // Update state
            self.total_pt = self.total_pt.saturating_sub(pt_out);
            self.total_sy = self.total_sy.saturating_add(sy_amount_in);

            // Recompute implied rate
            if let Ok(new_rate) = calc_new_ln_implied_rate(
                self.total_pt,
                self.total_sy,
                self.scalar_root,
                self.sy_exchange_rate,
                self.last_ln_implied_rate,
                self.expiry,
                block_time,
            ) {
                self.last_ln_implied_rate = new_rate;
            }

            self.refresh_prices()?;
            Ok(pt_out)
        } else {
            Err(AMMError::TokenNotFound(base_token))
        }
    }

    fn calculate_price(&self, base_token: Address, _quote_token: Address) -> Result<f64, AMMError> {
        let block_time = self.block_timestamp();
        let pt_price = if self.is_expired || self.expiry <= block_time {
            // 到期后 PT = underlying = 1:1
            1.0
        } else {
            let time_to_expiry = self.expiry - block_time;
            // PT marginal price = e^(lastLnImpliedRate * timeToExpiry / IMPLIED_RATE_TIME)
            let rate = get_exchange_rate_from_implied_rate(self.last_ln_implied_rate, time_to_expiry);
            i256_to_f64(rate)
        };


        if base_token == self.pt_token {
            Ok(pt_price)
        } else if base_token == self.underlying_token {
            if pt_price == 0.0 {
                Ok(0.0)
            } else {
                Ok(1.0 / pt_price)
            }
        } else {
            Err(AMMError::TokenNotFound(base_token))
        }
    }

    fn has_sufficient_liquidity(&self) -> bool {
        // 至少需要 0.001 PT + 对应的 SY 才认为有流动性
        let min_pt = U256::from(10u128.pow(15)); // 0.001 * 1e18
        let min_sy = U256::from(10u128.pow(15));
        self.total_pt >= min_pt && self.total_sy >= min_sy
    }

    fn spot_price(&self, base_token: Address, _quote_token: Address) -> Result<f64, AMMError> {
        let price = if base_token == self.pt_token {
            self.token_0_price
        } else if base_token == self.underlying_token {
            self.token_1_price
        } else {
            return Err(AMMError::TokenNotFound(base_token));
        };

        if price <= 0.0 || !price.is_finite() {
            return Err(AMMError::Msg("Invalid cached spot price".into()));
        }

        Ok(price)
    }

    fn decimals(&self, token: Address) -> u8 {
        if token == self.pt_token {
            self.pt_decimals
        } else if token == self.underlying_token {
            self.underlying_decimals
        } else {
            0
        }
    }

    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let deployer =
            GetPendlePoolDataBatchRequest::deploy_builder(provider.clone(), vec![self.address]);
        let res = deployer.call_raw().block(block_number).await?;

        // Tuple matches PendlePoolData struct order
        // NOTE: uint8 uses u16 for SolValue trait compatibility
        let data_vec = <Vec<(
            I256,     // totalPt
            I256,     // totalSy
            I256,     // scalarRoot
            U256,     // expiry
            U256,     // lnFeeRateRoot
            U256,     // reserveFeePercent
            U256,     // lastLnImpliedRate
            Address,  // pt
            Address,  // sy
            U256,     // syExchangeRate
            Address,  // underlying
            u16,      // underlyingDecimals (uint8 → u16 for SolValue)
            u16,      // ptDecimals
            U256,     // blockTimestamp
        )> as SolValue>::abi_decode(&res)?;

        let d = data_vec
            .first()
            .ok_or_else(|| AMMError::Msg("Empty PendlePool init data".into()))?;

        self.total_pt = d.0.into_raw();
        self.total_sy = d.1.into_raw();
        self.scalar_root = d.2.into_raw();
        self.expiry = d.3.to();
        self.ln_fee_rate_root = d.4;
        self.reserve_fee_percent = d.5.to();
        self.last_ln_implied_rate = d.6;
        self.pt_token = d.7;
        self.sy_address = d.8;
        self.sy_exchange_rate = d.9;
        self.underlying_token = d.10;
        self.underlying_decimals = d.11 as u8;
        self.pt_decimals = d.12 as u8;
        self.last_block_timestamp = d.13.to();
        self.is_expired = self.expiry <= self.last_block_timestamp;

        self.refresh_prices()?;

        info!(
            target: "amms::pendle::init",
            address = ?self.address,
            pt = ?self.pt_token,
            underlying = ?self.underlying_token,
            total_pt = ?self.total_pt,
            total_sy = ?self.total_sy,
            "PendlePool initialized"
        );

        Ok(self)
    }

    async fn update<N, P>(&mut self, provider: P) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        use alloy::sol_types::SolCall;

        let multicall = IMulticall3::new(MULTICALL3_ADDRESS, provider.clone());

        let calls = vec![
            IMulticall3::Call3 {
                target: self.address,
                allowFailure: false,
                callData: _storageCall {}.abi_encode().into(),
            },
            IMulticall3::Call3 {
                target: self.sy_address,
                allowFailure: false,
                callData: exchangeRateCall {}.abi_encode().into(),
            },
            IMulticall3::Call3 {
                target: self.address,
                allowFailure: false,
                callData: isExpiredCall {}.abi_encode().into(),
            },
        ];

        let results = multicall.aggregate3(calls).call().await?;

        // Decode _storage result
        let storage_bytes = &results[0].returnData;
        let storage_result = <_storageCall as SolCall>::abi_decode_returns(storage_bytes)?;
        self.total_pt = U256::from(storage_result.totalPt as u128);
        self.total_sy = U256::from(storage_result.totalSy as u128);
        self.last_ln_implied_rate = U256::from(storage_result.lastLnImpliedRate);

        // Decode exchangeRate result
        let rate_bytes = &results[1].returnData;
        let rate_result = <exchangeRateCall as SolCall>::abi_decode_returns(rate_bytes)?;
        self.sy_exchange_rate = rate_result;

        // Decode isExpired result
        let expired_bytes = &results[2].returnData;
        let expired_result = <isExpiredCall as SolCall>::abi_decode_returns(expired_bytes)?;
        self.is_expired = expired_result;

        self.refresh_prices()?;

        info!(
            target: "amms::pendle::update",
            address = ?self.address,
            sy_exchange_rate = ?self.sy_exchange_rate,
            total_pt = ?self.total_pt,
            total_sy = ?self.total_sy,
            is_expired = self.is_expired,
            "PendlePool updated"
        );

        Ok(())
    }
}
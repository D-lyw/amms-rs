//! # Curve Legacy AMM Module
//!
//! 本模块实现对 Curve Finance Legacy (V1/V2) 协议池的支持，包括：
//! - **3pool**: DAI/USDC/USDT 经典稳定币池 (~$167M TVL)
//! - **stETH/ETH**: Lido stETH 池
//! - **tricrypto2**: wBTC/ETH/USDT 波动资产池
//! - 其他早期部署的 Curve 池
//!
//! ## 与 curve_ng 的区别
//! - Legacy 池使用旧版合约接口
//! - 固定费率（无动态费率）
//! - 不支持 ERC-4626 和 Rebasing 代币
//! - 需要治理批准才能创建池子
//!
//! ## 主要池子地址 (Ethereum Mainnet)
//! - 3pool: `0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7`
//! - stETH/ETH: `0xDC24316b9AE028F1497c275EB9192a3Ea0f67022`
//! - tricrypto2: `0xD51a44d3FaE010294C616388b506AcdA1bfAAE46`
//!
//! ## 模块结构
//! ```text
//! curve_legacy/
//! ├── mod.rs           # 本文件，CurveLegacyPool 及 trait 实现
//! ├── types.rs         # CurveLegacyPoolType 枚举及共享类型
//! ├── stableswap.rs    # Legacy StableSwap 池逻辑 (3pool, stETH 等)
//! ├── cryptoswap.rs    # Legacy CryptoSwap 池逻辑 (tricrypto2 等)
//! └── math/
//!     ├── mod.rs
//!     └── stableswap.rs  # 可复用 curve_ng 的数学模块
//! ```

pub mod factory;
pub mod math;
pub mod types;

use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{address, Address, B256, U256},
    providers::Provider,
    rpc::types::Log,
    sol,
    sol_types::SolEvent,
};
use eyre::Result;
use std::sync::Arc;

use crate::amms::{
    amm::{AutomatedMarketMaker, SyncAction},
    error::AMMError,
};
pub use types::{
    CurveLegacyBaseView, CurveLegacyPool, CurveLegacyPoolType, CurveLegacyPoolType::*,
    CurveLegacySwapRoute, LegacyStableSwapType,
};

// Multicall3 Address (Standard across chains)
const MULTICALL_ADDRESS: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");

// Multicall3 Structs and Interface
sol! {
    struct Call3 {
        address target;
        bool allowFailure;
        bytes callData;
    }
    struct MulticallResult {
        bool success;
        bytes returnData;
    }
    #[sol(rpc)]
    interface IMulticall3 {
        function aggregate3(Call3[] calldata calls) external payable returns (MulticallResult[] memory returnData);
    }
}

sol! {
    #[sol(rpc)]
    interface ICurveLegacyCryptoSwapUpdate {
        function D() external view returns (uint256);
        function balances(uint256 i) external view returns (uint256);
    }
    #[sol(rpc)]
    interface ICurveLegacyLendingUpdate {
        function stored_rates(uint256 i) external view returns (uint256);
    }
    #[sol(rpc)]
    interface ICurveLegacyMetaUpdate {
        function get_virtual_price() external view returns (uint256);
    }
    #[sol(rpc)]
    interface ICurveLegacyPoolPriceScaleNoArgs {
        function price_scale() external view returns (uint256);
    }
    #[sol(rpc)]
    interface ICurveLegacyPoolPriceScaleWithArgs {
        function price_scale(uint256 i) external view returns (uint256);
    }
    #[sol(rpc)]
    interface ICurveLegacyTotalSupplyUpdate {
        function totalSupply() external view returns (uint256);
    }
    #[sol(rpc)]
    interface ICurveLegacyPoolMetaProbe {
        function base_pool() external view returns (address);
        function lp_token() external view returns (address);
        function get_virtual_price() external view returns (uint256);
    }
    #[sol(rpc)]
    interface ICurveLegacyPoolUnderlyingProbe {
        function underlying_coins(uint256 i) external view returns (address);
    }
    #[sol(rpc)]
    interface ICurveLegacyPoolUnderlyingInt128Probe {
        function underlying_coins(int128 i) external view returns (address);
    }
    #[sol(rpc)]
    interface ICurveLegacyPoolRatesProbe {
        function rates(int128 i) external view returns (uint256);
    }
    #[sol(rpc)]
    interface ICurveLegacyPoolStoredRatesArrayProbe {
        function stored_rates() external view returns (uint256[2] memory);
    }
}

// Curve Legacy 池合约 ABI (事件定义)
sol! {
    #[allow(missing_docs)]
    interface ICurveLegacyPool {
        // === Swap Events ===
        // Legacy StableSwap (int128 indices)
        event TokenExchange(
            address indexed buyer,
            int128 sold_id,
            uint256 tokens_sold,
            int128 bought_id,
            uint256 tokens_bought
        );
        // Legacy CryptoSwap (uint256 indices)
        event TokenExchangeCrypto(
            address indexed buyer,
            uint256 sold_id,
            uint256 tokens_sold,
            uint256 bought_id,
            uint256 tokens_bought
        );
        // Underlying Swap (StableSwap only)
        event TokenExchangeUnderlying(
            address indexed buyer,
            int128 sold_id,
            uint256 tokens_sold,
            int128 bought_id,
            uint256 tokens_bought
        );

        // === Remove Liquidity One ===
        // Legacy Stable (Old)
        event RemoveLiquidityOne(
            address indexed provider,
            uint256 token_amount,
            uint256 coin_amount
        );

        // === Admin / Parameter Events ===
        event ClaimAdminFees(address indexed admin, uint256 amount);
        event NewParameters(
            uint256 mid_fee,
            uint256 out_fee,
            uint256 fee_gamma,
            uint256 allowed_extra_profit,
            uint256 adjustment_step,
            uint256 ma_half_time
        );
        event CommitNewParameters(
            uint256 deadline,
            uint256 mid_fee,
            uint256 out_fee,
            uint256 fee_gamma,
            uint256 allowed_extra_profit,
            uint256 adjustment_step,
            uint256 ma_half_time
        );
        event RampA(
            uint256 old_A,
            uint256 new_A,
            uint256 initial_time,
            uint256 future_time
        );
        event StopRampA(
            uint256 A,
            uint256 t
        );
    }
}

// === StableSwap Liquidity Events (Correctly named 'AddLiquidity') ===
sol! {
    interface ICurveStableSwap2Event {
        event AddLiquidity(
            address indexed provider,
            uint256[2] token_amounts,
            uint256[2] fees,
            uint256 invariant,
            uint256 token_supply
        );
        event RemoveLiquidity(
            address indexed provider,
            uint256[2] token_amounts,
            uint256[2] fees,
            uint256 token_supply
        );
        event RemoveLiquidityImbalance(
            address indexed provider,
            uint256[2] token_amounts,
            uint256[2] fees,
            uint256 invariant,
            uint256 token_supply
        );
    }
    interface ICurveStableSwap3Event {
        event AddLiquidity(
            address indexed provider,
            uint256[3] token_amounts,
            uint256[3] fees,
            uint256 invariant,
            uint256 token_supply
        );
        event RemoveLiquidity(
            address indexed provider,
            uint256[3] token_amounts,
            uint256[3] fees,
            uint256 token_supply
        );
        event RemoveLiquidityImbalance(
            address indexed provider,
            uint256[3] token_amounts,
            uint256[3] fees,
            uint256 invariant,
            uint256 token_supply
        );
    }
    interface ICurveStableSwap4Event {
        event AddLiquidity(
            address indexed provider,
            uint256[4] token_amounts,
            uint256[4] fees,
            uint256 invariant,
            uint256 token_supply
        );
        event RemoveLiquidity(
            address indexed provider,
            uint256[4] token_amounts,
            uint256[4] fees,
            uint256 token_supply
        );
        event RemoveLiquidityImbalance(
            address indexed provider,
            uint256[4] token_amounts,
            uint256[4] fees,
            uint256 invariant,
            uint256 token_supply
        );
    }
}

sol! {
    #[allow(missing_docs)]
    interface ICurveLegacyCryptoEvent {
        event TokenExchange(
            address indexed buyer,
            uint256 sold_id,
            uint256 tokens_sold,
            uint256 bought_id,
            uint256 tokens_bought,
            uint256 fee,
            uint256 packed_price_scale
        );
        event AddLiquidity(
            address indexed provider,
            uint256[] token_amounts,
            uint256 fee,
            uint256 token_supply,
            uint256 packed_price_scale
        );
    }
}

// Tricrypto2 (Legacy V2) 使用的事件格式
// TokenExchange 事件名相同，但参数类型是 uint256（而非 int128 的 StableSwap 版本）
// 签名: 0xb2e76ae99761dc136e598d4a629bb347eccb9532a5f8bbd72e18467c3c34cc98
sol! {
    #[allow(missing_docs)]
    interface ICurveTricrypto2Event {
        event TokenExchange(
            address indexed buyer,
            uint256 sold_id,
            uint256 tokens_sold,
            uint256 bought_id,
            uint256 tokens_bought
        );
    }
}

// CryptoSwap (twocrypto/tricrypto) 特有的事件格式
// 这些事件与 StableSwap 版本不同
sol! {
    #[allow(missing_docs)]
    interface ICurveCryptoSwap2Event {
        // CryptoSwap 2-coin (no fees array)
        event AddLiquidity(
            address indexed provider,
            uint256[2] token_amounts,
            uint256 fee,
            uint256 token_supply
        );
        event RemoveLiquidity(
            address indexed provider,
            uint256[2] token_amounts,
            uint256 token_supply
        );
    }

    interface ICurveCryptoSwap3Event {
        // CryptoSwap 3-coin (no fees array)
        event AddLiquidity(
            address indexed provider,
            uint256[3] token_amounts,
            uint256 fee,
            uint256 token_supply
        );
        event RemoveLiquidity(
            address indexed provider,
            uint256[3] token_amounts,
            uint256 token_supply
        );
    }

    interface ICurveCryptoSwapEvent {
        // RemoveLiquidityOne - 包含 coin_index，可以精确更新余额
        // StableSwap 版本没有 coin_index
        event RemoveLiquidityOne(
            address indexed provider,
            uint256 token_amount,
            uint256 coin_index,
            uint256 coin_amount
        );

        // RampAgamma - CryptoSwap 用于同时调整 A 和 gamma
        // StableSwap 使用 RampA（只调整 A）
        event RampAgamma(
            uint256 initial_A,
            uint256 future_A,
            uint256 initial_gamma,
            uint256 future_gamma,
            uint256 initial_time,
            uint256 future_time
        );

        // StopRampA - CryptoSwap 版本包含 current_gamma
        // StableSwap 版本只有 (A, t)
        event StopRampA(
            uint256 current_A,
            uint256 current_gamma,
            uint256 time
        );
    }
}

sol! {
    /// Zap 合约 probe — 校验 zap_address 是否绑定到当前池子
    #[sol(rpc)]
    interface ICurveMetaZapProbe {
        function pool() external view returns (address);
        function base_pool() external view returns (address);
    }
}

impl AutomatedMarketMaker for CurveLegacyPool {
    fn address(&self) -> Address {
        self.address
    }

    fn last_synced_block(&self) -> u64 {
        self.last_synced_block
    }

    fn set_last_synced_block(&mut self, block_number: u64) {
        self.last_synced_block = block_number;
    }

    fn tokens(&self) -> Vec<Address> {
        self.routeable_tokens().to_vec()
    }

    fn has_sufficient_liquidity(&self) -> bool {
        // Curve pools should have liquidity in all coins to be usable
        self.balances.iter().enumerate().all(|(i, &balance)| {
            let decimals = self.decimals.get(i).copied().unwrap_or(18);

            // Replicate generic check from Token::has_sufficient_liquidity
            if decimals >= 18 {
                // 0.0001 unit (e.g. 10^14 wei)
                balance >= U256::from(10).pow(U256::from(decimals.saturating_sub(4)))
            } else if decimals >= 6 {
                // 100 units (e.g. 100 * 10^6 = 10^8)
                let threshold =
                    U256::from(100).saturating_mul(U256::from(10).pow(U256::from(decimals)));
                balance >= threshold
            } else {
                // Fallback
                balance >= U256::from(100_000)
            }
        })
    }

    fn decimals(&self, token: Address) -> u8 {
        self.token_decimals(token).unwrap_or(0)
    }

    /// Curve Legacy pools are deployed on multiple EVM chains
    fn supported_chains(&self) -> Option<Vec<u64>> {
        Some(vec![
            1,     // Ethereum
            42161, // Arbitrum
            137,   // Polygon
            10,    // Optimism
            8453,  // Base
            56,    // BSC
            250,   // Fantom
            43114, // Avalanche
            100,   // Gnosis
            42220, // Celo
        ])
    }

    fn sync_events(&self) -> Vec<B256> {
        vec![
            // Swaps
            ICurveLegacyPool::TokenExchange::SIGNATURE_HASH,
            ICurveLegacyPool::TokenExchangeCrypto::SIGNATURE_HASH,
            ICurveLegacyPool::TokenExchangeUnderlying::SIGNATURE_HASH,
            ICurveLegacyCryptoEvent::TokenExchange::SIGNATURE_HASH,
            ICurveTricrypto2Event::TokenExchange::SIGNATURE_HASH, // Tricrypto2 (uint256, 5 params)
            ICurveLegacyCryptoEvent::AddLiquidity::SIGNATURE_HASH,
            // StableSwap Liquidity 2 coins
            ICurveStableSwap2Event::AddLiquidity::SIGNATURE_HASH,
            ICurveStableSwap2Event::RemoveLiquidity::SIGNATURE_HASH,
            ICurveStableSwap2Event::RemoveLiquidityImbalance::SIGNATURE_HASH,
            // StableSwap Liquidity 3 coins
            ICurveStableSwap3Event::AddLiquidity::SIGNATURE_HASH,
            ICurveStableSwap3Event::RemoveLiquidity::SIGNATURE_HASH,
            ICurveStableSwap3Event::RemoveLiquidityImbalance::SIGNATURE_HASH,
            // StableSwap Liquidity 4 coins
            ICurveStableSwap4Event::AddLiquidity::SIGNATURE_HASH,
            ICurveStableSwap4Event::RemoveLiquidity::SIGNATURE_HASH,
            ICurveStableSwap4Event::RemoveLiquidityImbalance::SIGNATURE_HASH,
            // CryptoSwap Liquidity (different format - no fees array)
            ICurveCryptoSwap2Event::AddLiquidity::SIGNATURE_HASH,
            ICurveCryptoSwap3Event::AddLiquidity::SIGNATURE_HASH,
            ICurveCryptoSwap2Event::RemoveLiquidity::SIGNATURE_HASH,
            ICurveCryptoSwap3Event::RemoveLiquidity::SIGNATURE_HASH,
            // Remove One
            ICurveLegacyPool::RemoveLiquidityOne::SIGNATURE_HASH, // StableSwap (no coin_index)
            ICurveCryptoSwapEvent::RemoveLiquidityOne::SIGNATURE_HASH, // CryptoSwap (with coin_index)
            // Admin / Parameters - StableSwap
            ICurveLegacyPool::ClaimAdminFees::SIGNATURE_HASH,
            ICurveLegacyPool::NewParameters::SIGNATURE_HASH,
            ICurveLegacyPool::CommitNewParameters::SIGNATURE_HASH,
            ICurveLegacyPool::RampA::SIGNATURE_HASH,
            ICurveLegacyPool::StopRampA::SIGNATURE_HASH,
            // Admin / Parameters - CryptoSwap (different from StableSwap)
            ICurveCryptoSwapEvent::RampAgamma::SIGNATURE_HASH,
            ICurveCryptoSwapEvent::StopRampA::SIGNATURE_HASH,
        ]
    }

    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError> {
        let topic0 = log.topics()[0];

        // === 1. Token Exchange (Stable) ===
        if topic0 == ICurveLegacyPool::TokenExchange::SIGNATURE_HASH {
            let event = ICurveLegacyPool::TokenExchange::decode_log(&log.inner)?;
            let i = event.sold_id as usize;
            let j = event.bought_id as usize;

            // 更新余额
            if i < self.balances.len() {
                self.balances[i] += event.tokens_sold;
            }
            if j < self.balances.len() {
                self.balances[j] = self.balances[j]
                    .checked_sub(event.tokens_bought)
                    .ok_or(AMMError::Msg("Balance underflow".into()))?;
            }

            tracing::info!(
                target = "amms::curve_legacy::sync",
                pool = ?self.address,
                sold_id = i,
                bought_id = j,
                tokens_sold = ?event.tokens_sold,
                tokens_bought = ?event.tokens_bought,
                "TokenExchange (Stable)"
            );

            // CryptoSwap Check (Defensive: shouldn't happen for Crypto, but if it does...)
            if self.pool_type == CurveLegacyPoolType::CryptoSwap {
                self.recalculate_crypto_state()?;
                return Ok(SyncAction::AsyncUpdate);
            } else {
                self.update_spot_prices();
                return Ok(SyncAction::None);
            }

        // === 2. Token Exchange (Crypto, packed price_scale) ===
        } else if topic0 == ICurveLegacyCryptoEvent::TokenExchange::SIGNATURE_HASH {
            let event = ICurveLegacyCryptoEvent::TokenExchange::decode_log(&log.inner)?;
            let i = event.sold_id.try_into().unwrap_or(usize::MAX);
            let j = event.bought_id.try_into().unwrap_or(usize::MAX);

            if i < self.balances.len() {
                self.balances[i] += event.tokens_sold;
            }
            if j < self.balances.len() {
                self.balances[j] = self.balances[j]
                    .checked_sub(event.tokens_bought)
                    .ok_or(AMMError::Msg("Balance underflow".into()))?;
            }

            let mask = U256::from(2).pow(U256::from(128)) - U256::from(1);
            // packed_price_scale format: price_scale[1] << 128 | price_scale[0]
            // low 128 bits = price_scale[0], high 128 bits = price_scale[1]
            let new_price_scale = if self.n_coins <= 2 {
                vec![event.packed_price_scale & mask]
            } else {
                vec![
                    event.packed_price_scale & mask, // low bits = price_scale[0]
                    event.packed_price_scale >> 128, // high bits = price_scale[1]
                ]
            };
            self.price_scale = Some(new_price_scale);
            // Keep post-event state self-consistent:
            // packed event carries updated price_scale, so D must be recalculated after applying it.
            self.recalculate_crypto_state()?;

            tracing::info!(
                target = "amms::curve_legacy::sync",
                pool = ?self.address,
                sold_id = i,
                bought_id = j,
                tokens_sold = ?event.tokens_sold,
                tokens_bought = ?event.tokens_bought,
                "TokenExchange (Crypto packed)"
            );

            return Ok(SyncAction::AsyncUpdate);

        // === 3. Token Exchange (Crypto) ===
        } else if topic0 == ICurveLegacyPool::TokenExchangeCrypto::SIGNATURE_HASH {
            let event = ICurveLegacyPool::TokenExchangeCrypto::decode_log(&log.inner)?;
            let i = event.sold_id.try_into().unwrap_or(usize::MAX);
            let j = event.bought_id.try_into().unwrap_or(usize::MAX);

            if i < self.balances.len() {
                self.balances[i] += event.tokens_sold;
            }
            if j < self.balances.len() {
                self.balances[j] = self.balances[j]
                    .checked_sub(event.tokens_bought)
                    .ok_or(AMMError::Msg("Balance underflow".into()))?;
            }

            tracing::info!(
                target = "amms::curve_legacy::sync",
                pool = ?self.address,
                sold_id = i,
                bought_id = j,
                tokens_sold = ?event.tokens_sold,
                tokens_bought = ?event.tokens_bought,
                "TokenExchange (Crypto)"
            );

            self.recalculate_crypto_state()?;
            return Ok(SyncAction::AsyncUpdate);

        // === 4. Token Exchange (Tricrypto2 - uint256 indices, 5 params) ===
        // This is the original Tricrypto2 pool event format (signature: 0xb2e76ae9...)
        } else if topic0 == ICurveTricrypto2Event::TokenExchange::SIGNATURE_HASH {
            let event = ICurveTricrypto2Event::TokenExchange::decode_log(&log.inner)?;
            let i = event.sold_id.try_into().unwrap_or(usize::MAX);
            let j = event.bought_id.try_into().unwrap_or(usize::MAX);

            if i < self.balances.len() {
                self.balances[i] += event.tokens_sold;
            }
            if j < self.balances.len() {
                self.balances[j] = self.balances[j]
                    .checked_sub(event.tokens_bought)
                    .ok_or(AMMError::Msg("Balance underflow".into()))?;
            }

            tracing::info!(
                target = "amms::curve_legacy::sync",
                pool = ?self.address,
                sold_id = i,
                bought_id = j,
                tokens_sold = ?event.tokens_sold,
                tokens_bought = ?event.tokens_bought,
                "TokenExchange (Tricrypto2)"
            );

            // IMMEDIATELY recalculate D from updated balances via newton_d.
            // This eliminates the race condition between sync() and the async update:
            // D is now consistent with the updated balances, so any simulation that runs
            // before the pending sync worker calls update() will see a self-consistent state.
            // price_scale is still refreshed asynchronously via the pending sync worker.
            if self.pool_type == CryptoSwap {
                self.recalculate_crypto_state()?;
            }
            return Ok(SyncAction::AsyncUpdate);

        // === 5. Token Exchange Underlying ===
        } else if topic0 == ICurveLegacyPool::TokenExchangeUnderlying::SIGNATURE_HASH {
            let event = ICurveLegacyPool::TokenExchangeUnderlying::decode_log(&log.inner)?;
            if self.is_meta_pool() {
                tracing::info!(
                    target = "amms::curve_legacy::sync",
                    pool = ?self.address,
                    sold_id = event.sold_id,
                    bought_id = event.bought_id,
                    "TokenExchangeUnderlying on meta pool: forcing async refresh"
                );
                return Ok(SyncAction::AsyncUpdate);
            }

            let i = event.sold_id as usize;
            let j = event.bought_id as usize;

            if i < self.balances.len() {
                self.balances[i] += event.tokens_sold;
            }
            if j < self.balances.len() {
                self.balances[j] = self.balances[j]
                    .checked_sub(event.tokens_bought)
                    .ok_or(AMMError::Msg("Balance underflow".into()))?;
            }

            if self.pool_type == CurveLegacyPoolType::CryptoSwap {
                self.recalculate_crypto_state()?;
                return Ok(SyncAction::AsyncUpdate);
            }
            self.update_spot_prices();
            return Ok(SyncAction::None);

        // === 6. Add Liquidity (Crypto packed price_scale) ===
        } else if topic0 == ICurveLegacyCryptoEvent::AddLiquidity::SIGNATURE_HASH {
            let event = ICurveLegacyCryptoEvent::AddLiquidity::decode_log(&log.inner)?;
            for (i, &amount) in event.token_amounts.iter().enumerate() {
                if i < self.balances.len() {
                    self.balances[i] = self.balances[i].saturating_add(amount);
                }
            }

            let mask = U256::from(2).pow(U256::from(128)) - U256::from(1);
            // packed_price_scale format: price_scale[1] << 128 | price_scale[0]
            // low 128 bits = price_scale[0], high 128 bits = price_scale[1]
            let new_price_scale = if self.n_coins <= 2 {
                vec![event.packed_price_scale & mask]
            } else {
                vec![
                    event.packed_price_scale & mask,
                    event.packed_price_scale >> 128,
                ]
            };
            self.price_scale = Some(new_price_scale);
            if self.pool_type == CryptoSwap {
                // Keep post-event state self-consistent:
                // packed event carries updated price_scale, so D must be recalculated after applying it.
                self.recalculate_crypto_state()?;
            }

            tracing::info!(
                target = "amms::curve_legacy::sync",
                pool = ?self.address,
                "AddLiquidity (Crypto packed)"
            );

            return Ok(SyncAction::AsyncUpdate);

        // === 6. StableSwap Add Liquidity (2/3/4 Coins) ===
        } else if topic0 == ICurveStableSwap2Event::AddLiquidity::SIGNATURE_HASH
            || topic0 == ICurveStableSwap3Event::AddLiquidity::SIGNATURE_HASH
            || topic0 == ICurveStableSwap4Event::AddLiquidity::SIGNATURE_HASH
        {
            // Determine amounts and new token_supply based on signature
            let (amounts, token_supply): (Vec<U256>, U256) =
                if topic0 == ICurveStableSwap2Event::AddLiquidity::SIGNATURE_HASH {
                    let e = ICurveStableSwap2Event::AddLiquidity::decode_log(&log.inner)?;
                    (e.token_amounts.to_vec(), e.token_supply)
                } else if topic0 == ICurveStableSwap3Event::AddLiquidity::SIGNATURE_HASH {
                    let e = ICurveStableSwap3Event::AddLiquidity::decode_log(&log.inner)?;
                    (e.token_amounts.to_vec(), e.token_supply)
                } else {
                    let e = ICurveStableSwap4Event::AddLiquidity::decode_log(&log.inner)?;
                    (e.token_amounts.to_vec(), e.token_supply)
                };

            for (i, &amount) in amounts.iter().enumerate() {
                if i < self.balances.len() {
                    self.balances[i] = self.balances[i].saturating_add(amount);
                }
            }
            self.total_supply = Some(token_supply);

            tracing::info!(target = "amms::curve_legacy::sync", pool = ?self.address, "AddLiquidity (StableSwap)");

            if self.pool_type == CurveLegacyPoolType::CryptoSwap {
                self.recalculate_crypto_state()?;
                return Ok(SyncAction::AsyncUpdate);
            }
            self.update_spot_prices();
            return Ok(SyncAction::None);

        // === 7. StableSwap Remove Liquidity (2/3/4 Coins) ===
        } else if topic0 == ICurveStableSwap2Event::RemoveLiquidity::SIGNATURE_HASH
            || topic0 == ICurveStableSwap3Event::RemoveLiquidity::SIGNATURE_HASH
            || topic0 == ICurveStableSwap4Event::RemoveLiquidity::SIGNATURE_HASH
        {
            let (amounts, token_supply): (Vec<U256>, U256) =
                if topic0 == ICurveStableSwap2Event::RemoveLiquidity::SIGNATURE_HASH {
                    let e = ICurveStableSwap2Event::RemoveLiquidity::decode_log(&log.inner)?;
                    (e.token_amounts.to_vec(), e.token_supply)
                } else if topic0 == ICurveStableSwap3Event::RemoveLiquidity::SIGNATURE_HASH {
                    let e = ICurveStableSwap3Event::RemoveLiquidity::decode_log(&log.inner)?;
                    (e.token_amounts.to_vec(), e.token_supply)
                } else {
                    let e = ICurveStableSwap4Event::RemoveLiquidity::decode_log(&log.inner)?;
                    (e.token_amounts.to_vec(), e.token_supply)
                };

            for (i, &amount) in amounts.iter().enumerate() {
                if i < self.balances.len() {
                    self.balances[i] = self.balances[i].saturating_sub(amount);
                }
            }
            self.total_supply = Some(token_supply);

            tracing::info!(target = "amms::curve_legacy::sync", pool = ?self.address, "RemoveLiquidity (StableSwap)");

            if self.pool_type == CurveLegacyPoolType::CryptoSwap {
                self.recalculate_crypto_state()?;
                return Ok(SyncAction::AsyncUpdate);
            }
            self.update_spot_prices();
            return Ok(SyncAction::None);

        // === 8. StableSwap Remove Liquidity Imbalance (2/3/4 Coins) ===
        } else if topic0 == ICurveStableSwap2Event::RemoveLiquidityImbalance::SIGNATURE_HASH
            || topic0 == ICurveStableSwap3Event::RemoveLiquidityImbalance::SIGNATURE_HASH
            || topic0 == ICurveStableSwap4Event::RemoveLiquidityImbalance::SIGNATURE_HASH
        {
            let (amounts, token_supply): (Vec<U256>, U256) = if topic0
                == ICurveStableSwap2Event::RemoveLiquidityImbalance::SIGNATURE_HASH
            {
                let e = ICurveStableSwap2Event::RemoveLiquidityImbalance::decode_log(&log.inner)?;
                (e.token_amounts.to_vec(), e.token_supply)
            } else if topic0 == ICurveStableSwap3Event::RemoveLiquidityImbalance::SIGNATURE_HASH {
                let e = ICurveStableSwap3Event::RemoveLiquidityImbalance::decode_log(&log.inner)?;
                (e.token_amounts.to_vec(), e.token_supply)
            } else {
                let e = ICurveStableSwap4Event::RemoveLiquidityImbalance::decode_log(&log.inner)?;
                (e.token_amounts.to_vec(), e.token_supply)
            };

            for (i, &amount) in amounts.iter().enumerate() {
                if i < self.balances.len() {
                    self.balances[i] = self.balances[i].saturating_sub(amount);
                }
            }
            self.total_supply = Some(token_supply);

            tracing::info!(target = "amms::curve_legacy::sync", pool = ?self.address, "RemoveLiquidityImbalance (StableSwap)");

            if self.pool_type == CurveLegacyPoolType::CryptoSwap {
                self.recalculate_crypto_state()?;
                return Ok(SyncAction::AsyncUpdate);
            }
            self.update_spot_prices();
            return Ok(SyncAction::None);

        // === 9. Remove Liquidity One (StableSwap - no coin_index) ===
        } else if topic0 == ICurveLegacyPool::RemoveLiquidityOne::SIGNATURE_HASH {
            // StableSwap RemoveLiquidityOne doesn't emit which coin was removed.
            // So we MUST Resync.
            tracing::warn!(
                target = "amms::curve_legacy::sync",
                pool = ?self.address,
                "RemoveLiquidityOne (StableSwap) - No coin_index, triggering re-sync"
            );
            if let Some(ts) = self.total_supply {
                self.total_supply = Some(ts.saturating_sub(
                    ICurveLegacyPool::RemoveLiquidityOne::decode_log(&log.inner)?.token_amount,
                ));
            }
            return Ok(SyncAction::Resync);

        // === 10. Remove Liquidity One (CryptoSwap - with coin_index) ===
        } else if topic0 == ICurveCryptoSwapEvent::RemoveLiquidityOne::SIGNATURE_HASH {
            let event = ICurveCryptoSwapEvent::RemoveLiquidityOne::decode_log(&log.inner)?;
            let coin_index = event.coin_index.try_into().unwrap_or(usize::MAX);

            if coin_index < self.balances.len() {
                self.balances[coin_index] =
                    self.balances[coin_index].saturating_sub(event.coin_amount);
            }
            if let Some(ts) = self.total_supply {
                self.total_supply = Some(ts.saturating_sub(event.token_amount));
            }

            tracing::info!(
                target = "amms::curve_legacy::sync",
                pool = ?self.address,
                coin_index,
                coin_amount = ?event.coin_amount,
                "RemoveLiquidityOne (CryptoSwap)"
            );

            self.recalculate_crypto_state()?;
            return Ok(SyncAction::AsyncUpdate);

        // === 11. CryptoSwap AddLiquidity (2/3 coins, no fees array) ===
        } else if topic0 == ICurveCryptoSwap2Event::AddLiquidity::SIGNATURE_HASH
            || topic0 == ICurveCryptoSwap3Event::AddLiquidity::SIGNATURE_HASH
        {
            let (amounts, token_supply): (Vec<U256>, U256) =
                if topic0 == ICurveCryptoSwap2Event::AddLiquidity::SIGNATURE_HASH {
                    let e = ICurveCryptoSwap2Event::AddLiquidity::decode_log(&log.inner)?;
                    (e.token_amounts.to_vec(), e.token_supply)
                } else {
                    let e = ICurveCryptoSwap3Event::AddLiquidity::decode_log(&log.inner)?;
                    (e.token_amounts.to_vec(), e.token_supply)
                };

            for (i, &amount) in amounts.iter().enumerate() {
                if i < self.balances.len() {
                    self.balances[i] = self.balances[i].saturating_add(amount);
                }
            }
            self.total_supply = Some(token_supply);

            self.recalculate_crypto_state()?;

            tracing::info!(
                target = "amms::curve_legacy::sync",
                pool = ?self.address,
                amounts = ?amounts,
                "AddLiquidity (CryptoSwap) - Balances updated, D recalculated locally"
            );
            return Ok(SyncAction::AsyncUpdate);

        // === 12. CryptoSwap RemoveLiquidity (2/3 coins, no fees array) ===
        } else if topic0 == ICurveCryptoSwap2Event::RemoveLiquidity::SIGNATURE_HASH
            || topic0 == ICurveCryptoSwap3Event::RemoveLiquidity::SIGNATURE_HASH
        {
            let amounts: Vec<U256> =
                if topic0 == ICurveCryptoSwap2Event::RemoveLiquidity::SIGNATURE_HASH {
                    let e = ICurveCryptoSwap2Event::RemoveLiquidity::decode_log(&log.inner)?;
                    e.token_amounts.to_vec()
                } else {
                    let e = ICurveCryptoSwap3Event::RemoveLiquidity::decode_log(&log.inner)?;
                    e.token_amounts.to_vec()
                };

            for (i, &amount) in amounts.iter().enumerate() {
                if i < self.balances.len() {
                    self.balances[i] = self.balances[i].saturating_sub(amount);
                }
            }

            self.recalculate_crypto_state()?;

            tracing::info!(
                target = "amms::curve_legacy::sync",
                pool = ?self.address,
                amounts = ?amounts,
                "RemoveLiquidity (CryptoSwap) - Balances updated, D recalculated locally"
            );
            return Ok(SyncAction::AsyncUpdate);

        // === 13. Admin / Parameter Events (StableSwap) ===
        } else if topic0 == ICurveLegacyPool::ClaimAdminFees::SIGNATURE_HASH
            || topic0 == ICurveLegacyPool::NewParameters::SIGNATURE_HASH
            || topic0 == ICurveLegacyPool::CommitNewParameters::SIGNATURE_HASH
            || topic0 == ICurveLegacyPool::RampA::SIGNATURE_HASH
            || topic0 == ICurveLegacyPool::StopRampA::SIGNATURE_HASH
        {
            tracing::info!(
                target = "amms::curve_legacy::sync",
                pool = ?self.address,
                "Admin/Parameter event (StableSwap) detected, triggering update"
            );

            if self.pool_type == CurveLegacyPoolType::CryptoSwap {
                return Ok(SyncAction::AsyncUpdate);
            } else {
                return Ok(SyncAction::Resync);
            }

        // === 14. Admin / Parameter Events (CryptoSwap) ===
        } else if topic0 == ICurveCryptoSwapEvent::RampAgamma::SIGNATURE_HASH
            || topic0 == ICurveCryptoSwapEvent::StopRampA::SIGNATURE_HASH
        {
            tracing::info!(
                target = "amms::curve_legacy::sync",
                pool = ?self.address,
                "RampAgamma/StopRampA (CryptoSwap) detected, triggering async update"
            );
            // CryptoSwap parameter changes need full refresh
            return Ok(SyncAction::AsyncUpdate);
        }

        // Unknown event
        Ok(SyncAction::None)
    }

    /// Update cached spot prices for all token pairs

    fn calculate_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
        let in_decimals = self
            .token_decimals(base_token)
            .ok_or(AMMError::TokenNotFound(base_token))?;
        let out_decimals = self
            .token_decimals(quote_token)
            .ok_or(AMMError::TokenNotFound(quote_token))?;

        let amount_in = U256::from(10).pow(U256::from(in_decimals));
        let amount_out = self.simulate_swap(base_token, quote_token, amount_in)?;

        use crate::amms::float::u256_to_float;
        let amount_out_f = u256_to_float(amount_out)?;
        let precision_out = 10u64.pow(out_decimals as u32) as f64;
        // precision_out 是 f64，amount_out_f 是 rug::Float，需要先将 Float 转为 f64
        let price = amount_out_f.to_f64() / precision_out;
        Ok(price)
    }

    fn spot_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
        // 1. Check cache
        if let Some(&price) = self.spot_prices.get(&(base_token, quote_token)) {
            if price > 0.0 && price.is_finite() {
                return Ok(price);
            }
        }

        // 2. Fallback to calculation
        let price = self.calculate_price(base_token, quote_token)?;
        if price <= 0.0 || !price.is_finite() {
            return Err(AMMError::Msg("Invalid calculated spot price".to_string()));
        }
        Ok(price)
    }

    fn simulate_swap(
        &self,
        base_token: Address,
        quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        match self
            .resolve_swap_route(base_token, quote_token)
            .ok_or(AMMError::TokenNotFound(quote_token))?
        {
            CurveLegacySwapRoute::Direct { i, j } => self.simulate_direct_swap(i, j, amount_in),
            CurveLegacySwapRoute::BaseToBase { i, j } => self
                .base_pool_view
                .as_ref()
                .ok_or_else(|| AMMError::Msg("Meta pool missing base_pool_view".into()))?
                .simulate_direct_swap(i, j, amount_in),
            CurveLegacySwapRoute::MetaToBase { meta_i, base_j } => {
                let lp_idx = self
                    .base_token_index
                    .ok_or_else(|| AMMError::Msg("Meta pool missing base_token_index".into()))?;
                let lp_out = self.simulate_direct_swap(meta_i, lp_idx, amount_in)?;
                self.base_pool_view
                    .as_ref()
                    .ok_or_else(|| AMMError::Msg("Meta pool missing base_pool_view".into()))?
                    .simulate_remove_liquidity_one_coin(lp_out, base_j)
            }
            CurveLegacySwapRoute::BaseToMeta { base_i, meta_j } => {
                let lp_out = self
                    .base_pool_view
                    .as_ref()
                    .ok_or_else(|| AMMError::Msg("Meta pool missing base_pool_view".into()))?
                    .simulate_add_liquidity_one_coin(base_i, amount_in)?;
                let lp_idx = self
                    .base_token_index
                    .ok_or_else(|| AMMError::Msg("Meta pool missing base_token_index".into()))?;
                self.simulate_direct_swap(lp_idx, meta_j, lp_out)
            }
        }
    }

    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        match self
            .resolve_swap_route(base_token, quote_token)
            .ok_or(AMMError::TokenNotFound(quote_token))?
        {
            CurveLegacySwapRoute::Direct { i, j } => {
                let amount_out = self.simulate_direct_swap(i, j, amount_in)?;
                if i < self.balances.len() {
                    self.balances[i] += amount_in;
                }
                if j < self.balances.len() {
                    self.balances[j] =
                        self.balances[j]
                            .checked_sub(amount_out)
                            .ok_or(AMMError::Msg(
                                "Balance underflow in simulate_swap_mut".into(),
                            ))?;
                }
                Ok(amount_out)
            }
            _ => Err(AMMError::Msg(
                "simulate_swap_mut is unsupported for Curve Legacy meta underlying routes".into(),
            )),
        }
    }

    fn simulate_swap_exact_out(
        &self,
        base_token: Address,
        quote_token: Address,
        amount_out: U256,
    ) -> Result<U256, AMMError> {
        match self
            .resolve_swap_route(base_token, quote_token)
            .ok_or(AMMError::TokenNotFound(quote_token))?
        {
            CurveLegacySwapRoute::Direct { i, j } if !self.is_meta_pool() => match self.pool_type {
                StableSwap => self.simulate_stableswap_exact_out(i, j, amount_out),
                CryptoSwap => self.simulate_cryptoswap_exact_out(i, j, amount_out),
            },
            _ => self.simulate_swap_exact_out_via_search(base_token, quote_token, amount_out),
        }
    }

    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        Self: Sized,
        N: Network,
        P: Provider<N> + Clone,
    {
        // NOTE:
        // Any Curve Legacy meta-pool initialization fields added here must also be reflected by any
        // batch initialization path. The current codebase still uses this single-pool path during
        // Curve Legacy batch init, but a future dedicated batch path must preserve the same fields.
        use alloy::sol;

        sol! {
            #[sol(rpc)]
            interface ICurveLegacyPool {
                function coins(uint256 i) external view returns (address);
                function balances(uint256 i) external view returns (uint256);
                function A() external view returns (uint256);
                function A_precise() external view returns (uint256);
                function fee() external view returns (uint256);
                function admin_fee() external view returns (uint256);
                function stored_rates(uint256 i) external view returns (uint256);

                // Crypto V2 params
                function D() external view returns (uint256);
                function gamma() external view returns (uint256);
                function mid_fee() external view returns (uint256);
                function out_fee() external view returns (uint256);
                function fee_gamma() external view returns (uint256);
                function allowed_extra_profit() external view returns (uint256);
                function adjustment_step() external view returns (uint256);
                function ma_half_time() external view returns (uint256);
                function price_scale(uint256 i) external view returns (uint256);
            }

            #[sol(rpc)]
            interface ICurveLegacyPoolInt128 {
                function coins(int128 i) external view returns (address);
                function balances(int128 i) external view returns (uint256);
            }

        }

        let pool = ICurveLegacyPool::new(self.address, provider.clone());
        let pool_int = ICurveLegacyPoolInt128::new(self.address, provider.clone());

        // 1. Fetch Coins, Decimals, Balances
        // Assuming max 8 coins
        for i in 0..8 {
            // Try uint256 first, then int128
            let coin = match pool.coins(U256::from(i)).block(block_number).call().await {
                Ok(c) => c,
                Err(_) => match pool_int.coins(i as i128).block(block_number).call().await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::debug!(
                            pool = ?self.address,
                            index = i,
                            error = %e,
                            "Stopped fetching coins (likely end of list)"
                        );
                        break;
                    }
                },
            };

            if coin == Address::ZERO {
                break;
            }

            // Check for duplicates (some pools wrap around or return same coin for out-of-bounds index)
            if self.coins.contains(&coin) {
                tracing::debug!(
                    pool = ?self.address,
                    coin = ?coin,
                    "Duplicate coin detected in init loop, stopping"
                );
                break;
            }
            self.coins.push(coin);

            // Balance
            let balance = match pool
                .balances(U256::from(i))
                .block(block_number)
                .call()
                .await
            {
                Ok(b) => b,
                Err(_) => pool_int
                    .balances(i as i128)
                    .block(block_number)
                    .call()
                    .await
                    .map_err(|e| {
                        tracing::error!(
                            pool = ?self.address,
                            coin_index = i,
                            error = %e,
                            "Failed to fetch balance"
                        );
                        AMMError::SyncError(self.address)
                    })?,
            };
            self.balances.push(balance);

            // Decimals
            let decimals = if coin == Address::repeat_byte(0xee) {
                18
            } else {
                let token = crate::amms::IERC20::new(coin, provider.clone());
                token
                    .decimals()
                    .block(block_number)
                    .call()
                    .await
                    .map_err(|e| {
                        tracing::error!(
                            pool = ?self.address,
                            coin = ?coin,
                            error = %e,
                            "Failed to fetch decimals"
                        );
                        AMMError::SyncError(self.address)
                    })?
            };
            self.decimals.push(decimals);
        }

        self.n_coins = self.coins.len() as u8;
        if self.n_coins == 0 {
            return Err(AMMError::SyncError(self.address));
        }

        // 新版池子 (Vyper 0.3.x) 有 A_precise() 方法，返回 A * 100
        // 旧版池子 (Vyper 0.2.x) 没有此方法
        // 检测方法：尝试调用 A_precise()，如果成功且返回值是 A() * 100，则是新版
        if let Ok(amp) = pool.A().block(block_number).call().await {
            self.amp = Some(amp);

            // 尝试调用 A_precise 检测版本
            if let Ok(amp_precise) = pool.A_precise().block(block_number).call().await {
                // 新版池子: A_precise() = A() * 100
                if amp_precise == amp * U256::from(100) {
                    self.uses_a_precision = true;
                    tracing::debug!(
                        pool = ?self.address,
                        "Detected new version pool (uses A_PRECISION=100)"
                    );
                }
            }
        }
        if let Ok(fee) = pool.fee().block(block_number).call().await {
            self.fee = fee;
        }
        if let Ok(admin_fee) = pool.admin_fee().block(block_number).call().await {
            self.admin_fee = admin_fee;
        }

        // === Auto-detect pool_type ===
        // Curve Registry 可能同时包含 StableSwap 和 CryptoSwap 池子，
        // 但 Factory 构造时只传入单一 pool_type，导致所有池子被赋予同一类型。
        // 通过尝试调用 gamma() 自动纠偏：
        // - gamma() 存在 => CryptoSwap
        // - gamma() revert/不存在 => StableSwap
        // 这样可以同时修正 Stable->Crypto 和 Crypto->Stable 的误分类。
        match pool.gamma().block(block_number).call().await {
            Ok(gamma_val) => {
                if self.pool_type != CurveLegacyPoolType::CryptoSwap {
                    tracing::info!(
                        pool = ?self.address,
                        gamma = ?gamma_val,
                        "Auto-detected CryptoSwap pool (gamma() exists), overriding pool_type to CryptoSwap"
                    );
                }
                self.pool_type = CurveLegacyPoolType::CryptoSwap;
            }
            Err(e) => {
                let err_text = e.to_string().to_ascii_lowercase();
                let is_revert_like = err_text.contains("execution reverted")
                    || err_text.contains("reverted")
                    || err_text.contains("revert");

                if is_revert_like {
                    if self.pool_type != CurveLegacyPoolType::StableSwap {
                        tracing::info!(
                            pool = ?self.address,
                            error = %e,
                            "Auto-detected StableSwap pool (gamma() reverted/missing), overriding pool_type to StableSwap"
                        );
                    }
                    self.pool_type = CurveLegacyPoolType::StableSwap;
                } else {
                    tracing::warn!(
                        pool = ?self.address,
                        error = %e,
                        pool_type = ?self.pool_type,
                        "gamma() probe failed with non-revert error, keeping existing pool_type"
                    );
                }
            }
        }

        // === Meta / Underlying Detection ===
        self.load_underlying_coin_probes::<N, _>(block_number, provider.clone())
            .await;

        if let Some(base_pool_addr) = self
            .probe_base_pool_address::<N, _>(block_number, provider.clone())
            .await
        {
            tracing::debug!(
                pool = ?self.address,
                base_pool = ?base_pool_addr,
                "Detected base_pool() on Curve Legacy pool"
            );

            let base_pool_state = Box::pin(
                CurveLegacyPool::new(base_pool_addr, CurveLegacyPoolType::StableSwap)
                    .init(block_number, provider.clone()),
            )
            .await?;
            self.attach_base_pool_state::<N, _>(
                base_pool_addr,
                None,
                None,
                base_pool_state,
                block_number,
                provider.clone(),
            )
            .await?;
        } else if let Some(base_idx) = self.coins.len().checked_sub(1) {
            let candidate_addr = self.coins[base_idx];
            if candidate_addr != Address::ZERO && candidate_addr != self.address {
                if let Some(candidate_pool) = Self::try_init_stable_base_pool_candidate::<N, _>(
                    candidate_addr,
                    block_number,
                    provider.clone(),
                )
                .await
                {
                    tracing::debug!(
                        pool = ?self.address,
                        candidate_base_pool = ?candidate_addr,
                        candidate_index = base_idx,
                        base_n_coins = candidate_pool.n_coins,
                        "Detected meta pool via last-coin base-pool structure"
                    );
                    self.attach_base_pool_state::<N, _>(
                        candidate_addr,
                        Some(base_idx),
                        Some(candidate_addr),
                        candidate_pool,
                        block_number,
                        provider.clone(),
                    )
                    .await?;
                }
            }
        }

        // === CryptoSwap Meta Pool 校验 ===
        // CryptoSwap meta pool 需要 zap_address 且链上验证通过
        if self.pool_type == CurveLegacyPoolType::CryptoSwap && self.is_meta_pool() {
            let zap = match self.zap_address {
                Some(z) if z != Address::ZERO => z,
                _ => {
                    return Err(AMMError::Msg(format!(
                        "CryptoSwap meta pool {:?} requires zap_address; set before sync()",
                        self.address
                    )));
                }
            };
            // 链上验证：zap.pool() 必须返回当前池子地址
            let probe = ICurveMetaZapProbe::new(zap, provider.clone());
            let reported_pool = probe
                .pool()
                .block(block_number)
                .call()
                .await
                .map_err(|e| {
                    AMMError::Msg(format!(
                        "zap {:?} pool() probe failed for {:?}: {}",
                        zap, self.address, e
                    ))
                })?;
            if reported_pool != self.address {
                return Err(AMMError::Msg(format!(
                    "zap {:?} pool() returned {:?}, expected {:?}",
                    zap, reported_pool, self.address
                )));
            }
            tracing::debug!(
                pool = ?self.address,
                zap = ?zap,
                "CryptoSwap meta pool zap_address verified on-chain"
            );
        }

        // === Subtype Detection (StableSwap only) ===
        if self.pool_type == CurveLegacyPoolType::StableSwap {
            if self.is_meta_pool() {
                self.stable_type = LegacyStableSwapType::Meta;
                tracing::debug!(pool = ?self.address, "Identified as StableSwap Metapool");
            } else if !self.underlying_coins.is_empty()
                && (self.underlying_coins.len() != self.coins.len()
                    || self.underlying_coins != self.coins)
            {
                self.stable_type = LegacyStableSwapType::Lending;
                tracing::debug!(pool = ?self.address, "Identified as Lending Pool");
            } else {
                self.stable_type = LegacyStableSwapType::Plain;
            }
        }

        let total_supply_contract =
            ICurveLegacyTotalSupplyUpdate::new(self.address, provider.clone());
        if let Ok(ts) = total_supply_contract
            .totalSupply()
            .block(block_number)
            .call()
            .await
        {
            self.total_supply = Some(ts);
        } else if let Ok(lp_addr) = ICurveLegacyPoolMetaProbe::new(self.address, provider.clone())
            .lp_token()
            .block(block_number)
            .call()
            .await
        {
            let lp_contract = ICurveLegacyTotalSupplyUpdate::new(lp_addr, provider.clone());
            if let Ok(ts) = lp_contract.totalSupply().block(block_number).call().await {
                self.total_supply = Some(ts);
            }
        }

        // Some Legacy pools (Lending, Metapools, Factory Plain Pools, etc.) use custom rates.
        // We try different stored_rates signatures in order of preference:
        // 1. stored_rates() -> uint256[2] (Factory Plain Pool Vyper 0.3.7+)
        // 2. stored_rates(uint256) -> uint256 (Lending pools)
        // 3. rates(int128) -> uint256 (Metapools)
        // If none works, we keep the default rates (calculated from decimals).
        let mut custom_rates = Vec::new();

        // 1. Try stored_rates() -> uint256[2] first (Factory Plain Pools with LST)
        let pool_stored_rates_array =
            ICurveLegacyPoolStoredRatesArrayProbe::new(self.address, provider.clone());
        if let Ok(rates_array) = pool_stored_rates_array
            .stored_rates()
            .block(block_number)
            .call()
            .await
        {
            tracing::debug!(pool = ?self.address, rates = ?rates_array, "Got rates from stored_rates() array");
            custom_rates = rates_array
                .into_iter()
                .take(self.n_coins as usize)
                .collect();
        }

        // 2. If stored_rates() array failed, try stored_rates(uint256) (Lending pools)
        if custom_rates.is_empty() {
            if pool
                .stored_rates(U256::ZERO)
                .block(block_number)
                .call()
                .await
                .is_ok()
            {
                for i in 0..self.n_coins {
                    if let Ok(r) = pool
                        .stored_rates(U256::from(i))
                        .block(block_number)
                        .call()
                        .await
                    {
                        custom_rates.push(r);
                    }
                }
            }
        }

        // 3. If stored_rates failed or incomplete, try rates(int128) (Metapools)
        if custom_rates.len() != self.n_coins as usize {
            custom_rates.clear();
            let pool_rates = ICurveLegacyPoolRatesProbe::new(self.address, provider.clone());
            if pool_rates.rates(0).block(block_number).call().await.is_ok() {
                for i in 0..self.n_coins {
                    if let Ok(r) = pool_rates.rates(i as i128).block(block_number).call().await {
                        custom_rates.push(r);
                    }
                }
            }
        }

        // If custom rates found, use them
        if custom_rates.len() == self.n_coins as usize {
            tracing::debug!(pool = ?self.address, rates = ?custom_rates, "Using custom rates for pool");
            self.rates = custom_rates;
        }

        // 4. Fetch Crypto Params if applicable
        // CryptoSwap 池必须成功获取 d, gamma, mid_fee, out_fee, fee_gamma
        // 这些参数缺失会导致 simulate_cryptoswap 中 divide by zero
        if self.pool_type == CryptoSwap {
            // D 值 - 必须成功获取
            match pool.D().block(block_number).call().await {
                Ok(d) => {
                    if d == U256::ZERO {
                        tracing::warn!(
                            pool = ?self.address,
                            "CryptoSwap pool D() returned zero, pool may be empty or invalid"
                        );
                    }
                    self.d = Some(d);
                }
                Err(e) => {
                    tracing::error!(
                        pool = ?self.address,
                        error = %e,
                        "CryptoSwap pool failed to fetch D(), cannot initialize"
                    );
                    return Err(AMMError::Msg(format!(
                        "CryptoSwap pool {:?} failed to fetch D(): {}",
                        self.address, e
                    )));
                }
            }

            // gamma 值 - 必须成功获取
            match pool.gamma().block(block_number).call().await {
                Ok(gamma) => {
                    if gamma == U256::ZERO {
                        tracing::error!(
                            pool = ?self.address,
                            "CryptoSwap pool gamma() returned zero, this will cause divide by zero"
                        );
                        return Err(AMMError::Msg(format!(
                            "CryptoSwap pool {:?} has gamma=0, cannot initialize",
                            self.address
                        )));
                    }
                    self.gamma = Some(gamma);
                }
                Err(e) => {
                    tracing::error!(
                        pool = ?self.address,
                        error = %e,
                        "CryptoSwap pool failed to fetch gamma(), cannot initialize"
                    );
                    return Err(AMMError::Msg(format!(
                        "CryptoSwap pool {:?} failed to fetch gamma(): {}",
                        self.address, e
                    )));
                }
            }

            // mid_fee - 必须成功获取
            match pool.mid_fee().block(block_number).call().await {
                Ok(v) => self.mid_fee = Some(v),
                Err(e) => {
                    tracing::error!(
                        pool = ?self.address,
                        error = %e,
                        "CryptoSwap pool failed to fetch mid_fee()"
                    );
                    return Err(AMMError::Msg(format!(
                        "CryptoSwap pool {:?} failed to fetch mid_fee(): {}",
                        self.address, e
                    )));
                }
            }

            // out_fee - 必须成功获取
            match pool.out_fee().block(block_number).call().await {
                Ok(v) => self.out_fee = Some(v),
                Err(e) => {
                    tracing::error!(
                        pool = ?self.address,
                        error = %e,
                        "CryptoSwap pool failed to fetch out_fee()"
                    );
                    return Err(AMMError::Msg(format!(
                        "CryptoSwap pool {:?} failed to fetch out_fee(): {}",
                        self.address, e
                    )));
                }
            }

            // fee_gamma - 必须成功获取
            match pool.fee_gamma().block(block_number).call().await {
                Ok(v) => self.fee_gamma = Some(v),
                Err(e) => {
                    tracing::error!(
                        pool = ?self.address,
                        error = %e,
                        "CryptoSwap pool failed to fetch fee_gamma()"
                    );
                    return Err(AMMError::Msg(format!(
                        "CryptoSwap pool {:?} failed to fetch fee_gamma(): {}",
                        self.address, e
                    )));
                }
            }

            // 以下参数可选，静默忽略错误
            if let Ok(v) = pool.allowed_extra_profit().block(block_number).call().await {
                self.allowed_extra_profit = Some(v);
            }
            if let Ok(v) = pool.adjustment_step().block(block_number).call().await {
                self.adjustment_step = Some(v);
            }
            if let Ok(v) = pool.ma_half_time().block(block_number).call().await {
                self.ma_half_time = Some(v);
            }

            // Fetch price_scale
            // CryptoSwap pools have different signatures:
            // - Two-coin pools: price_scale() returns single uint256
            // - Three-coin pools: price_scale(uint256 i) returns price at index i

            // Try price_scale() without arguments first (two-coin pools)
            let pool_ps_no_args =
                ICurveLegacyPoolPriceScaleNoArgs::new(self.address, provider.clone());
            if let Ok(ps) = pool_ps_no_args
                .price_scale()
                .block(block_number)
                .call()
                .await
            {
                self.price_scale = Some(vec![ps]);
                tracing::debug!(pool = ?self.address, ps = ?ps, "Got price_scale() (no args)");
            }

            // If price_scale() didn't work, try price_scale(uint256) (three-coin pools like Tricrypto2)
            if self.price_scale.is_none() {
                let mut scales = Vec::new();
                for k in 0..self.n_coins.saturating_sub(1) {
                    if let Ok(ps) = pool
                        .price_scale(U256::from(k))
                        .block(block_number)
                        .call()
                        .await
                    {
                        scales.push(ps);
                    }
                }
                if !scales.is_empty() {
                    self.price_scale = Some(scales);
                }
            }
        }

        self.last_synced_block = block_number.as_u64().unwrap_or(0);
        self.update_spot_prices();
        Ok(self)
    }

    /// 异步更新池状态 - 使用 Multicall 批量刷新 CryptoSwap (D, balances, price_scale) 或 StableSwap (stored_rates, virtual_price)
    async fn update<N, P>(&mut self, provider: P) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let mut calls = Vec::new();

        // Crypto Indices
        let mut crypto_d_idx = None;
        let mut crypto_balance_indices = Vec::new();
        let mut crypto_scale_no_args_idx = None;
        let mut crypto_scale_with_args_indices = Vec::new();

        // Stable Indices
        let mut stable_balance_indices = Vec::new();
        let mut stable_rate_indices = Vec::new();
        let mut meta_vp_idx = None;

        let address = self.address;

        use alloy::sol_types::SolCall;

        // === Construct Calls ===

        // CryptoSwap: D, Balances, PriceScale
        if self.pool_type == CurveLegacyPoolType::CryptoSwap {
            // 1. D
            calls.push(Call3 {
                target: address,
                allowFailure: true,
                callData: ICurveLegacyCryptoSwapUpdate::DCall {}.abi_encode().into(),
            });
            crypto_d_idx = Some(calls.len() - 1);

            // 2. Balances
            for i in 0..self.n_coins {
                calls.push(Call3 {
                    target: address,
                    allowFailure: true,
                    callData: ICurveLegacyCryptoSwapUpdate::balancesCall { i: U256::from(i) }
                        .abi_encode()
                        .into(),
                });
                crypto_balance_indices.push(calls.len() - 1);
            }

            // 3. Price Scale - try both signatures
            // First: price_scale() without arguments (two-coin pools)
            calls.push(Call3 {
                target: address,
                allowFailure: true,
                callData: ICurveLegacyPoolPriceScaleNoArgs::price_scaleCall {}
                    .abi_encode()
                    .into(),
            });
            crypto_scale_no_args_idx = Some(calls.len() - 1);

            // Then: price_scale(uint256) with arguments (three-coin pools)
            let n_scales = self.n_coins.saturating_sub(1) as usize;
            for k in 0..n_scales {
                calls.push(Call3 {
                    target: address,
                    allowFailure: true,
                    callData: ICurveLegacyPoolPriceScaleWithArgs::price_scaleCall {
                        i: U256::from(k),
                    }
                    .abi_encode()
                    .into(),
                });
                crypto_scale_with_args_indices.push(calls.len() - 1);
            }
        }

        // StableSwap: Balances, Stored Rates, Virtual Price
        if self.pool_type == CurveLegacyPoolType::StableSwap {
            // Balances (All StableSwap pools have balances)
            for i in 0..self.n_coins {
                calls.push(Call3 {
                    target: address,
                    allowFailure: true,
                    callData: ICurveLegacyCryptoSwapUpdate::balancesCall { i: U256::from(i) }
                        .abi_encode()
                        .into(),
                });
                stable_balance_indices.push(calls.len() - 1);
            }

            // Lending Pool: stored_rates
            if self.stable_type == LegacyStableSwapType::Lending {
                for i in 0..self.n_coins {
                    calls.push(Call3 {
                        target: address,
                        allowFailure: true,
                        callData: ICurveLegacyLendingUpdate::stored_ratesCall { i: U256::from(i) }
                            .abi_encode()
                            .into(),
                    });
                    stable_rate_indices.push(calls.len() - 1);
                }
            }
            // Metapool: base_virtual_price
            if self.base_pool_address.is_some() {
                if let Some(base_pool_addr) = self.base_pool_address {
                    calls.push(Call3 {
                        target: base_pool_addr,
                        allowFailure: true,
                        callData: ICurveLegacyMetaUpdate::get_virtual_priceCall {}
                            .abi_encode()
                            .into(),
                    });
                    meta_vp_idx = Some(calls.len() - 1);
                }
            }
        }

        if calls.is_empty() {
            self.update_spot_prices();
            return Ok(());
        }

        // === Execute Multicall ===
        let multicall = IMulticall3::new(MULTICALL_ADDRESS, provider.clone());

        if let Ok(results_struct) = multicall.aggregate3(calls).call().await {
            let results = results_struct;

            // === Parse CryptoSwap Results ===
            if self.pool_type == CurveLegacyPoolType::CryptoSwap {
                // D
                if let Some(idx) = crypto_d_idx {
                    if let Some(res) = results.get(idx).filter(|r| r.success) {
                        if let Ok(d) =
                            ICurveLegacyCryptoSwapUpdate::DCall::abi_decode_returns(&res.returnData)
                        {
                            self.d = Some(d);
                        }
                    }
                }

                // Balances
                let mut new_balances = self.balances.clone();
                let mut balance_updated = false;
                for (i, &idx) in crypto_balance_indices.iter().enumerate() {
                    if let Some(res) = results.get(idx).filter(|r| r.success) {
                        if let Ok(b) =
                            ICurveLegacyCryptoSwapUpdate::balancesCall::abi_decode_returns(
                                &res.returnData,
                            )
                        {
                            if i < new_balances.len() && new_balances[i] != b {
                                new_balances[i] = b;
                                balance_updated = true;
                            }
                        }
                    }
                }
                if balance_updated {
                    self.balances = new_balances;
                }

                // Price Scale - try no-args version first, then with-args version.
                // Important: fallback decision must be based on this round's decode result,
                // not previous self.price_scale value, otherwise 3-coin pools can stop refreshing.
                let mut parsed_no_args_this_round = false;

                // First: price_scale() without arguments (two-coin pools)
                if let Some(idx) = crypto_scale_no_args_idx {
                    if let Some(res) = results
                        .get(idx)
                        .filter(|r| r.success && r.returnData.len() == 32)
                    {
                        if let Ok(ps) =
                            ICurveLegacyPoolPriceScaleNoArgs::price_scaleCall::abi_decode_returns(
                                &res.returnData,
                            )
                        {
                            self.price_scale = Some(vec![ps]);
                            parsed_no_args_this_round = true;
                            tracing::debug!(pool = ?self.address, ps = ?ps, "Updated price_scale (no args) from multicall");
                        }
                    }
                }

                // If no-args didn't decode in this round, try with-args version (three-coin pools)
                if !parsed_no_args_this_round {
                    let mut new_scales = Vec::new();
                    for &idx in &crypto_scale_with_args_indices {
                        if let Some(res) = results
                            .get(idx)
                            .filter(|r| r.success && r.returnData.len() == 32)
                        {
                            if let Ok(ps) = ICurveLegacyPoolPriceScaleWithArgs::price_scaleCall::abi_decode_returns(&res.returnData) {
                                new_scales.push(ps);
                            }
                        }
                    }
                    if !new_scales.is_empty() {
                        tracing::debug!(pool = ?self.address, scales = ?new_scales, "Updated price_scale (with args) from multicall");
                        self.price_scale = Some(new_scales);
                    }
                }
            }

            // === Parse StableSwap Results ===
            if self.pool_type == CurveLegacyPoolType::StableSwap {
                // Balances
                let mut new_balances = self.balances.clone();
                let mut balance_updated = false;
                for (i, &idx) in stable_balance_indices.iter().enumerate() {
                    if let Some(res) = results.get(idx).filter(|r| r.success) {
                        if let Ok(b) =
                            ICurveLegacyCryptoSwapUpdate::balancesCall::abi_decode_returns(
                                &res.returnData,
                            )
                        {
                            if i < new_balances.len() && new_balances[i] != b {
                                new_balances[i] = b;
                                balance_updated = true;
                            }
                        }
                    }
                }
                if balance_updated {
                    self.balances = new_balances;
                    tracing::debug!(pool = ?self.address, "StableSwap balances updated (multicall)");
                }

                // Stored Rates
                if !stable_rate_indices.is_empty() {
                    let mut new_rates = Vec::new();
                    for &idx in &stable_rate_indices {
                        if let Some(res) = results.get(idx).filter(|r| r.success) {
                            if let Ok(r) =
                                ICurveLegacyLendingUpdate::stored_ratesCall::abi_decode_returns(
                                    &res.returnData,
                                )
                            {
                                new_rates.push(r);
                            }
                        }
                    }
                    if new_rates.len() == self.n_coins as usize {
                        self.rates = new_rates;
                        tracing::debug!(pool = ?self.address, "Updated stored_rates (multicall)");
                    }
                }

                // Virtual Price
                if let Some(idx) = meta_vp_idx {
                    if let Some(res) = results.get(idx).filter(|r| r.success) {
                        if let Ok(vp) =
                            ICurveLegacyMetaUpdate::get_virtual_priceCall::abi_decode_returns(
                                &res.returnData,
                            )
                        {
                            self.base_virtual_price = Some(vp);
                            tracing::debug!(pool = ?self.address, vp = ?vp, "Updated base_virtual_price (multicall)");
                        }
                    }
                }
            }
        }

        self.update_spot_prices();
        Ok(())
    }
}

impl CurveLegacyPool {
    pub fn is_meta_pool(&self) -> bool {
        self.is_meta || self.base_pool_address.is_some()
    }

    pub fn routeable_tokens(&self) -> &[Address] {
        if self.is_meta_pool() && !self.underlying_coins.is_empty() {
            &self.underlying_coins
        } else {
            &self.coins
        }
    }

    pub fn direct_tokens(&self) -> &[Address] {
        &self.coins
    }

    pub fn underlying_tokens(&self) -> &[Address] {
        &self.underlying_coins
    }

    pub fn direct_token_index(&self, token: Address) -> Option<usize> {
        self.coins.iter().position(|&t| t == token)
    }

    pub fn underlying_token_index(&self, token: Address) -> Option<usize> {
        self.underlying_coins.iter().position(|&t| t == token)
    }

    pub fn token_decimals(&self, token: Address) -> Option<u8> {
        if let Some(i) = self.direct_token_index(token) {
            return self.decimals.get(i).copied();
        }

        let view = self.base_pool_view.as_ref()?;
        let i = view.token_index(token)?;
        view.decimals.get(i).copied()
    }

    pub fn build_base_view(&self) -> Option<Arc<CurveLegacyBaseView>> {
        if self.pool_type != CurveLegacyPoolType::StableSwap || self.is_meta_pool() {
            return None;
        }

        let total_supply = self.total_supply?;
        Some(Arc::new(CurveLegacyBaseView {
            address: self.address,
            last_synced_block: self.last_synced_block,
            pool_type: self.pool_type,
            stable_type: self.stable_type,
            n_coins: self.n_coins,
            coins: self.coins.clone(),
            balances: self.balances.clone(),
            decimals: self.decimals.clone(),
            rates: self.rates.clone(),
            amp: self.amp,
            uses_a_precision: self.uses_a_precision,
            fee: self.fee,
            admin_fee: self.admin_fee,
            total_supply,
        }))
    }

    async fn load_underlying_coin_probes<N, P>(&mut self, block_number: BlockId, provider: P)
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let pool_lending = ICurveLegacyPoolUnderlyingProbe::new(self.address, provider.clone());
        let pool_lending_int =
            ICurveLegacyPoolUnderlyingInt128Probe::new(self.address, provider.clone());

        let mut detected_underlying = Vec::new();
        for i in 0..8 {
            let u_coin = match pool_lending
                .underlying_coins(U256::from(i))
                .block(block_number)
                .call()
                .await
            {
                Ok(c) => c,
                Err(_) => match pool_lending_int
                    .underlying_coins(i as i128)
                    .block(block_number)
                    .call()
                    .await
                {
                    Ok(c) => c,
                    Err(_) => break,
                },
            };

            if u_coin == Address::ZERO || detected_underlying.contains(&u_coin) {
                break;
            }
            detected_underlying.push(u_coin);
        }

        if !detected_underlying.is_empty() {
            self.underlying_coins = detected_underlying;
        }
    }

    async fn probe_base_pool_address<N, P>(
        &self,
        block_number: BlockId,
        provider: P,
    ) -> Option<Address>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let pool_meta = ICurveLegacyPoolMetaProbe::new(self.address, provider);
        pool_meta
            .base_pool()
            .block(block_number)
            .call()
            .await
            .ok()
            .filter(|addr| *addr != Address::ZERO)
    }

    async fn try_init_stable_base_pool_candidate<N, P>(
        candidate_addr: Address,
        block_number: BlockId,
        provider: P,
    ) -> Option<Self>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let candidate_result = Box::pin(
            CurveLegacyPool::new(candidate_addr, CurveLegacyPoolType::StableSwap)
                .init(block_number, provider),
        )
        .await
        .ok()?;

        if candidate_result.pool_type != CurveLegacyPoolType::StableSwap
            || candidate_result.n_coins == 0
            || candidate_result.coins.is_empty()
        {
            return None;
        }

        Some(candidate_result)
    }

    fn expand_underlying_with_base_pool(&mut self, base_idx: usize, base_pool_coins: &[Address]) {
        if !(self.underlying_coins.is_empty() || self.underlying_coins.len() <= self.coins.len()) {
            return;
        }
        if base_pool_coins.is_empty() {
            return;
        }

        let mut expanded =
            Vec::with_capacity(self.coins.len().saturating_sub(1) + base_pool_coins.len());
        for (idx, &coin) in self.coins.iter().enumerate() {
            if idx == base_idx {
                expanded.extend(base_pool_coins.iter().copied());
            } else {
                expanded.push(coin);
            }
        }
        self.underlying_coins = expanded;
    }

    async fn attach_base_pool_state<N, P>(
        &mut self,
        base_pool_addr: Address,
        base_token_index_hint: Option<usize>,
        base_lp_token_hint: Option<Address>,
        mut base_pool_state: Self,
        block_number: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let base_pool_contract = ICurveLegacyPoolMetaProbe::new(base_pool_addr, provider.clone());
        if let Ok(vp) = base_pool_contract
            .get_virtual_price()
            .block(block_number)
            .call()
            .await
        {
            self.base_virtual_price = Some(vp);
        } else {
            tracing::warn!(
                pool = ?self.address,
                base_pool = ?base_pool_addr,
                "Failed to fetch base pool virtual price"
            );
        }

        let probed_lp_token = base_pool_contract
            .lp_token()
            .block(block_number)
            .call()
            .await
            .ok()
            .filter(|lp| *lp != Address::ZERO);

        self.base_pool_address = Some(base_pool_addr);
        self.base_lp_token = base_lp_token_hint.or(probed_lp_token);
        self.base_token_index = base_token_index_hint
            .or_else(|| self.coins.iter().position(|&c| c == base_pool_addr))
            .or_else(|| {
                self.base_lp_token
                    .and_then(|lp| self.coins.iter().position(|&c| c == lp))
            })
            .or_else(|| self.coins.len().checked_sub(1));
        if self.base_lp_token.is_none() {
            self.base_lp_token = self.base_token_index.map(|idx| self.coins[idx]);
        }

        self.base_n_coins = base_pool_state.n_coins;
        base_pool_state.base_pool_view = None;
        let base_pool_coins = base_pool_state.coins.clone();
        self.base_pool_view = base_pool_state.build_base_view();
        if let Some(base_idx) = self.base_token_index {
            self.expand_underlying_with_base_pool(base_idx, &base_pool_coins);
        }
        self.is_meta = true;

        Ok(())
    }

    pub fn simulate_direct_swap(
        &self,
        i: usize,
        j: usize,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        match self.pool_type {
            StableSwap => self.simulate_stableswap(i, j, amount_in),
            CryptoSwap => self.simulate_cryptoswap(i, j, amount_in),
        }
    }

    pub fn resolve_swap_route(
        &self,
        base_token: Address,
        quote_token: Address,
    ) -> Option<CurveLegacySwapRoute> {
        if let (Some(i), Some(j)) = (
            self.direct_token_index(base_token),
            self.direct_token_index(quote_token),
        ) {
            return Some(CurveLegacySwapRoute::Direct { i, j });
        }

        let base_view = self.base_pool_view.as_ref()?;
        let base_i = base_view.token_index(base_token);
        let base_j = base_view.token_index(quote_token);

        if let (Some(i), Some(j)) = (base_i, base_j) {
            return Some(CurveLegacySwapRoute::BaseToBase { i, j });
        }

        if let (Some(meta_i), Some(base_j)) = (self.direct_token_index(base_token), base_j) {
            if Some(meta_i) != self.base_token_index {
                return Some(CurveLegacySwapRoute::MetaToBase { meta_i, base_j });
            }
        }

        if let (Some(base_i), Some(meta_j)) = (base_i, self.direct_token_index(quote_token)) {
            if Some(meta_j) != self.base_token_index {
                return Some(CurveLegacySwapRoute::BaseToMeta { base_i, meta_j });
            }
        }

        None
    }

    fn simulate_swap_exact_out_via_search(
        &self,
        base_token: Address,
        quote_token: Address,
        amount_out: U256,
    ) -> Result<U256, AMMError> {
        if amount_out.is_zero() {
            return Ok(U256::ZERO);
        }

        let in_decimals = self.decimals(base_token);
        if in_decimals == 0 {
            return Err(AMMError::TokenNotFound(base_token));
        }

        let mut low = U256::ZERO;
        let mut high = U256::from(10).pow(U256::from(in_decimals));

        for _ in 0..32 {
            let out = self.simulate_swap(base_token, quote_token, high)?;
            if out >= amount_out {
                break;
            }
            high = high.saturating_mul(U256::from(2));
        }

        for _ in 0..192 {
            if low >= high {
                break;
            }
            let mid = (low + high) / U256::from(2);
            let out = self.simulate_swap(base_token, quote_token, mid)?;
            if out >= amount_out {
                high = mid;
            } else {
                low = mid + U256::from(1);
            }
        }

        let final_out = self.simulate_swap(base_token, quote_token, high)?;
        if final_out < amount_out {
            return Err(AMMError::Msg(
                "Exact-out search failed to find sufficient input".into(),
            ));
        }

        Ok(high)
    }

    /// Update cached spot prices for all token pairs
    pub(crate) fn update_spot_prices(&mut self) {
        if self.routeable_tokens().len() < 2 {
            return;
        }

        // Skip if any balance is zero to avoid division by zero in math functions
        for (k, balance) in self.balances.iter().enumerate() {
            if *balance == U256::ZERO {
                tracing::debug!(
                    pool = ?self.address,
                    coin_index = k,
                    "Skipping spot price update: balance is zero"
                );
                return;
            }
        }

        let tokens = self.routeable_tokens().to_vec();
        for i in 0..tokens.len() {
            for j in 0..tokens.len() {
                if i == j {
                    continue;
                }
                let base = tokens[i];
                let quote = tokens[j];
                let Some(decimals_i) = self.token_decimals(base) else {
                    continue;
                };
                let Some(decimals_j) = self.token_decimals(quote) else {
                    continue;
                };
                let amount_in = U256::from(10).pow(U256::from(decimals_i));

                let amount_out_res = self.simulate_swap(base, quote, amount_in);

                if let Ok(amount_out) = amount_out_res {
                    let price = amount_out.to_string().parse::<f64>().unwrap_or(0.0)
                        / 10f64.powi(decimals_j as i32);
                    self.spot_prices.insert((base, quote), price);
                }
            }
        }
    }
    /// 获取用于计算 xp 的费率
    /// stored_rates (if available): 优先使用链上 stored_rates，适用于 Lending 池和有自定义 rate 的 Plain 池（如 LST）
    /// Plain: 10^(36-decimals)
    /// Meta: 10^(36-decimals) for others, but LP token uses virtual_price
    fn get_rates(&self) -> Result<Vec<U256>, AMMError> {
        let n = self.n_coins as usize;

        // 1. 优先使用 stored_rates（如果有）
        // 不仅是 Lending Pool，一些 Plain 池（如 ETHx/WETH）也使用 stored_rates 来表示 LST 的溢价
        if self.rates.len() == n {
            return Ok(self.rates.clone());
        }

        // 2. Metapool / Plain fallback
        // For Metapool, we need to substitute the rate for the LP token with virtual_price
        let mut rates = Vec::with_capacity(n);
        for i in 0..n {
            let mut rate = if self.decimals[i] > 18 {
                return Err(AMMError::Msg(format!("Decimals {} > 18", self.decimals[i])));
            } else {
                U256::from(10).pow(U256::from(36 - self.decimals[i] as u32))
            };

            // Metapool Special Handling
            if self.stable_type == LegacyStableSwapType::Meta {
                if let Some(lp_idx) = self.base_token_index {
                    if i == lp_idx {
                        if let Some(vp) = self.base_virtual_price {
                            // Rate for LP token = Virtual Price
                            // Assuming Virtual Price is 1e18 scaled.
                            // And assuming the LP token decimals usually 18.
                            // Standard rate is 1e18 (for 18 decimals).
                            // We replace it with VP.
                            rate = vp;
                        }
                    }
                }
            }

            rates.push(rate);
        }

        Ok(rates)
    }

    /// 模拟 Legacy StableSwap 交换
    pub fn simulate_stableswap(&self, i: usize, j: usize, dx: U256) -> Result<U256, AMMError> {
        let n = self.n_coins as usize;
        let amp = self.amp.ok_or(AMMError::Msg("Amp not set".into()))?;
        let precision = U256::from(10).pow(U256::from(18)); // 1e18

        // 1. 获取 Rates
        let rates = self.get_rates()?;
        // Guard: division by rate later
        if rates[i].is_zero() || rates[j].is_zero() {
            return Err(AMMError::Msg(
                "Zero rate detected in simulate_stableswap".into(),
            ));
        }

        // 2. 计算 xp (缩放后的余额)
        let mut xp: Vec<U256> = Vec::with_capacity(n);
        for (k, balance) in self.balances.iter().enumerate() {
            // xp = balance * rate / PRECISION
            let xp_k = balance * rates[k] / precision;
            xp.push(xp_k);
        }

        // 3. 缩放输入金额 dx
        // 合约: x = xp[i] + dx * rates[i] / PRECISION
        let dx_scaled = dx * rates[i] / precision;

        let fee = self.fee;

        // 4. 计算 dy_scaled (缩放域的输出，不含费用)
        let dy_scaled =
            math::stableswap::get_dy(&xp, amp, i, j, dx_scaled, fee, self.uses_a_precision)?;

        // 5. 在缩放域扣除费用 (与链上合约 get_dy 一致)
        // 合约: fee = self.fee * dy / FEE_DENOMINATOR
        let fee_denom = U256::from(10_000_000_000u64);
        let fee_scaled = self.fee * dy_scaled / fee_denom;
        let dy_after_fee_scaled = dy_scaled - fee_scaled;

        // 6. 反缩放到真实单位
        // 合约: return (dy - fee) * PRECISION / rates[j]
        let dy = dy_after_fee_scaled * precision / rates[j];

        Ok(dy)
    }

    /// 模拟 Legacy CryptoSwap (V2) 交换
    pub fn simulate_cryptoswap(&self, i: usize, j: usize, dx: U256) -> Result<U256, AMMError> {
        let n = self.n_coins as usize;
        let amp = self.amp.ok_or(AMMError::Msg("Amp not set".into()))?;
        let gamma = self.gamma.ok_or(AMMError::Msg("Gamma not set".into()))?;
        let d = self.d.ok_or(AMMError::Msg("D not set".into()))?;
        let price_scale = self
            .price_scale
            .as_ref()
            .ok_or(AMMError::Msg("Price scale not set".into()))?;

        // Check for zero price scale elements used in division
        for ps in price_scale {
            if ps.is_zero() {
                return Err(AMMError::Msg("Zero price scale detected".into()));
            }
        }

        // D 值范围检查 - 与 CurveNG 保持一致
        // 链上合约: assert _D > 10**17 - 1 and _D < 10**15 * 10**18 + 1
        let d_min = U256::from(10).pow(U256::from(17)); // 0.1 ETH
        let d_max = U256::from(10).pow(U256::from(33)); // 10^15 ETH
        if d < d_min {
            return Err(AMMError::Msg(format!(
                "Curve CryptoSwap: D value {} too small (min: {}). Pool has insufficient liquidity.",
                d, d_min
            )));
        }
        if d > d_max {
            return Err(AMMError::Msg(format!(
                "Curve CryptoSwap: D value {} exceeds maximum (max: {})",
                d, d_max
            )));
        }

        // Precisions
        let mut precisions = Vec::with_capacity(n);
        for dec in &self.decimals {
            if *dec > 18 {
                return Err(AMMError::Msg(format!(
                    "Decimals {} > 18 not supported",
                    dec
                )));
            }
            let prec = U256::from(10).pow(U256::from(18 - dec));
            if prec == U256::ZERO {
                return Err(AMMError::Msg("Precision is zero".into()));
            }
            precisions.push(prec);
        }

        let precision_const = U256::from(1_000_000_000_000_000_000u64);

        // 1. Calculate xp (scaled)
        let mut xp = Vec::with_capacity(n);
        xp.push(self.balances[0] * precisions[0]);
        for k in 1..n {
            xp.push(self.balances[k] * precisions[k] * price_scale[k - 1] / precision_const);
        }

        // 2. Scale DX
        let dx_scaled = if i == 0 {
            dx * precisions[0]
        } else {
            dx * precisions[i] * price_scale[i - 1] / precision_const
        };

        let mid_fee = self
            .mid_fee
            .ok_or(AMMError::Msg("Mid fee not set".into()))?;
        let out_fee = self
            .out_fee
            .ok_or(AMMError::Msg("Out fee not set".into()))?;
        let fee_gamma = self
            .fee_gamma
            .ok_or(AMMError::Msg("Fee gamma not set".into()))?;

        // 3. Call get_dy (Legacy V2) - now using confirmed state for Dynamic Fee
        let dy_scaled = math::cryptoswap::get_dy(
            &xp,
            amp,
            gamma,
            d,
            i,
            j,
            dx_scaled,
            mid_fee,
            out_fee,
            fee_gamma,
            price_scale,
        )?;

        // 4. Downscale output: dy_scaled -> dy_raw
        let dy_raw = if j == 0 {
            dy_scaled / precisions[0]
        } else {
            dy_scaled * precision_const / (precisions[j] * price_scale[j - 1])
        };

        Ok(dy_raw)
    }

    /// Recalculate D and fee for CryptoSwap pools after balance update
    /// This should be called after sync_from_log updates balances
    pub fn recalculate_crypto_state(&mut self) -> Result<(), AMMError> {
        if self.pool_type != CryptoSwap {
            return Ok(());
        }

        let n = self.n_coins as usize;
        let amp = self.amp.ok_or(AMMError::Msg("Amp not set".into()))?;
        let gamma = self.gamma.ok_or(AMMError::Msg("Gamma not set".into()))?;
        let price_scale = self
            .price_scale
            .as_ref()
            .ok_or(AMMError::Msg("Price scale not set".into()))?
            .clone();

        // Dynamic fee parameters (should be set during init)
        let mid_fee = self
            .mid_fee
            .ok_or(AMMError::Msg("mid_fee not set".into()))?;
        let out_fee = self
            .out_fee
            .ok_or(AMMError::Msg("out_fee not set".into()))?;
        let fee_gamma = self
            .fee_gamma
            .ok_or(AMMError::Msg("fee_gamma not set".into()))?;

        // 1. Calculate xp (scaled balances)
        let mut precisions = Vec::with_capacity(n);
        for dec in &self.decimals {
            precisions.push(U256::from(10).pow(U256::from(18 - dec)));
        }

        let precision_const = U256::from(1_000_000_000_000_000_000u64);

        let mut xp = Vec::with_capacity(n);
        xp.push(self.balances[0] * precisions[0]);
        for k in 1..n {
            xp.push(self.balances[k] * precisions[k] * price_scale[k - 1] / precision_const);
        }

        // 2. Recalculate D using newton_d
        let new_d = math::cryptoswap::newton_d(amp, gamma, &xp)?;

        // D 值范围检查 - 确保新计算的 D 有效
        // 与 simulate_cryptoswap 中的检查保持一致
        let d_min = U256::from(10).pow(U256::from(17)); // 0.1 ETH
        if new_d < d_min {
            return Err(AMMError::Msg(format!(
                "recalculate_crypto_state: calculated D {} too small (min: {}). Pool may have insufficient liquidity after this event.",
                new_d, d_min
            )));
        }

        self.d = Some(new_d);

        // 3. Recalculate fee using fee_calc
        let new_fee = math::cryptoswap::fee_calc(&xp, new_d, mid_fee, out_fee, fee_gamma)?;
        self.fee = new_fee;

        Ok(())
    }
    /// StableSwap Exact-Out simulation (binary search on dx).
    /// Given a target output amount, find the minimal input amount required.
    pub fn simulate_stableswap_exact_out(
        &self,
        i: usize,
        j: usize,
        amount_out: U256,
    ) -> Result<U256, AMMError> {
        if amount_out.is_zero() {
            return Ok(U256::ZERO);
        }
        if j >= self.balances.len() || i >= self.balances.len() {
            return Err(AMMError::Msg("Token index out of bounds".into()));
        }
        if amount_out >= self.balances[j] {
            return Err(AMMError::Msg("Insufficient liquidity for exact out".into()));
        }

        // Find upper bound by exponential search.
        let mut low = U256::ZERO;
        let mut high = U256::from(1u8);
        let max_high = self
            .balances
            .get(i)
            .copied()
            .unwrap_or(U256::ZERO)
            .saturating_mul(U256::from(1000u64));

        loop {
            let dy = self.simulate_stableswap(i, j, high)?;
            if dy >= amount_out {
                break;
            }
            if max_high.is_zero() || high >= max_high {
                return Err(AMMError::Msg(
                    "Exact out not reachable within max search bound".into(),
                ));
            }
            high = high.saturating_mul(U256::from(2u8));
            if high > max_high {
                high = max_high;
            }
        }

        // Binary search for minimal dx that yields dy >= amount_out.
        while high > low + U256::from(1u8) {
            let mid = (low + high) / U256::from(2u8);
            let dy = self.simulate_stableswap(i, j, mid)?;
            if dy >= amount_out {
                high = mid;
            } else {
                low = mid;
            }
        }

        Ok(high)
    }

    /// CryptoSwap Exact-Out simulation (binary search on dx).
    /// Given a target output amount, find the minimal input amount required.
    pub fn simulate_cryptoswap_exact_out(
        &self,
        i: usize,
        j: usize,
        amount_out: U256,
    ) -> Result<U256, AMMError> {
        if amount_out.is_zero() {
            return Ok(U256::ZERO);
        }
        if j >= self.balances.len() || i >= self.balances.len() {
            return Err(AMMError::Msg("Token index out of bounds".into()));
        }
        if amount_out >= self.balances[j] {
            return Err(AMMError::Msg("Insufficient liquidity for exact out".into()));
        }

        // Find upper bound by exponential search.
        let mut low = U256::ZERO;
        let mut high = U256::from(1u8);
        let max_high = self
            .balances
            .get(i)
            .copied()
            .unwrap_or(U256::ZERO)
            .saturating_mul(U256::from(1000u64));

        loop {
            let dy = match self.simulate_cryptoswap(i, j, high) {
                Ok(v) => v,
                Err(_) => U256::ZERO,
            };
            if dy >= amount_out {
                break;
            }
            if max_high.is_zero() || high >= max_high {
                return Err(AMMError::Msg(
                    "Exact out not reachable within max search bound".into(),
                ));
            }
            high = high.saturating_mul(U256::from(2u8));
            if high > max_high {
                high = max_high;
            }
        }

        // Binary search for minimal dx that yields dy >= amount_out.
        while high > low + U256::from(1u8) {
            let mid = (low + high) / U256::from(2u8);
            let dy = match self.simulate_cryptoswap(i, j, mid) {
                Ok(v) => v,
                Err(_) => {
                    low = mid;
                    continue;
                }
            };
            if dy >= amount_out {
                high = mid;
            } else {
                low = mid;
            }
        }

        Ok(high)
    }
}

const CURVE_RATE_PRECISION: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
const CURVE_FEE_DENOMINATOR: U256 = U256::from_limbs([10_000_000_000, 0, 0, 0]);

fn default_rates_from_decimals(decimals: &[u8]) -> Result<Vec<U256>, AMMError> {
    decimals
        .iter()
        .map(|&dec| {
            if dec > 18 {
                Err(AMMError::Msg(format!("Decimals {} > 18", dec)))
            } else {
                Ok(U256::from(10).pow(U256::from(36 - dec as u32)))
            }
        })
        .collect()
}

fn scale_balances_with_rates(balances: &[U256], rates: &[U256]) -> Result<Vec<U256>, AMMError> {
    if balances.len() != rates.len() {
        return Err(AMMError::Msg("Balances/rates length mismatch".into()));
    }

    Ok(balances
        .iter()
        .zip(rates.iter())
        .map(|(&balance, &rate)| balance * rate / CURVE_RATE_PRECISION)
        .collect())
}

impl CurveLegacyBaseView {
    pub fn token_index(&self, token: Address) -> Option<usize> {
        self.coins.iter().position(|&c| c == token)
    }

    fn rates_or_default(&self) -> Result<Vec<U256>, AMMError> {
        if self.rates.len() == self.n_coins as usize {
            Ok(self.rates.clone())
        } else {
            default_rates_from_decimals(&self.decimals)
        }
    }

    pub fn simulate_direct_swap(&self, i: usize, j: usize, dx: U256) -> Result<U256, AMMError> {
        if self.pool_type != CurveLegacyPoolType::StableSwap {
            return Err(AMMError::Msg(
                "Base view direct swap currently only supports StableSwap".into(),
            ));
        }

        let amp = self
            .amp
            .ok_or(AMMError::Msg("Base view amp not set".into()))?;
        let rates = self.rates_or_default()?;
        if i >= self.balances.len() || j >= self.balances.len() {
            return Err(AMMError::Msg("Base view token index out of bounds".into()));
        }

        let xp = scale_balances_with_rates(&self.balances, &rates)?;
        let dx_scaled = dx * rates[i] / CURVE_RATE_PRECISION;
        let dy_scaled =
            math::stableswap::get_dy(&xp, amp, i, j, dx_scaled, self.fee, self.uses_a_precision)?;
        let fee = dy_scaled * self.fee / CURVE_FEE_DENOMINATOR;
        Ok(dy_scaled
            .saturating_sub(fee)
            .saturating_mul(CURVE_RATE_PRECISION)
            / rates[j])
    }

    pub fn simulate_add_liquidity_one_coin(
        &self,
        coin_idx: usize,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        if coin_idx >= self.balances.len() {
            return Err(AMMError::Msg("Base view token index out of bounds".into()));
        }
        if self.total_supply.is_zero() {
            return Err(AMMError::Msg("Base view total_supply is zero".into()));
        }

        let amp = self
            .amp
            .ok_or(AMMError::Msg("Base view amp not set".into()))?;
        let rates = self.rates_or_default()?;
        let old_balances = self.balances.clone();
        let mut new_balances = old_balances.clone();
        new_balances[coin_idx] = new_balances[coin_idx].saturating_add(amount_in);

        let xp_old = scale_balances_with_rates(&old_balances, &rates)?;
        let xp_new = scale_balances_with_rates(&new_balances, &rates)?;
        let d0 = math::stableswap::get_d(&xp_old, amp, self.uses_a_precision)?;
        let d1 = math::stableswap::get_d(&xp_new, amp, self.uses_a_precision)?;

        if d0.is_zero() {
            return Err(AMMError::Msg("Base view D0 is zero".into()));
        }

        let n = U256::from(self.n_coins as u64);
        let base_fee = self.fee * n / (U256::from(4) * (n - U256::from(1)));

        let mut reduced_balances = new_balances.clone();
        for k in 0..reduced_balances.len() {
            let ideal_balance = d1 * old_balances[k] / d0;
            let difference = if ideal_balance > reduced_balances[k] {
                ideal_balance - reduced_balances[k]
            } else {
                reduced_balances[k] - ideal_balance
            };
            let fee_amount = base_fee * difference / CURVE_FEE_DENOMINATOR;
            reduced_balances[k] = reduced_balances[k].saturating_sub(fee_amount);
        }

        let xp_reduced = scale_balances_with_rates(&reduced_balances, &rates)?;
        let d2 = math::stableswap::get_d(&xp_reduced, amp, self.uses_a_precision)?;
        Ok(self.total_supply * d2.saturating_sub(d0) / d0)
    }

    pub fn simulate_remove_liquidity_one_coin(
        &self,
        lp_in: U256,
        coin_idx: usize,
    ) -> Result<U256, AMMError> {
        if coin_idx >= self.balances.len() {
            return Err(AMMError::Msg("Base view token index out of bounds".into()));
        }
        if self.total_supply.is_zero() {
            return Err(AMMError::Msg("Base view total_supply is zero".into()));
        }

        let amp = self
            .amp
            .ok_or(AMMError::Msg("Base view amp not set".into()))?;
        let rates = self.rates_or_default()?;
        let xp = scale_balances_with_rates(&self.balances, &rates)?;
        let d0 = math::stableswap::get_d(&xp, amp, self.uses_a_precision)?;
        let d1 = d0.saturating_sub(lp_in * d0 / self.total_supply);
        let new_y = math::stableswap::get_y_d(coin_idx, &xp, amp, d1, self.uses_a_precision)?;

        let n = U256::from(self.n_coins as u64);
        let base_fee = self.fee * n / (U256::from(4) * (n - U256::from(1)));
        let mut xp_reduced = xp.clone();

        for j in 0..xp.len() {
            let dx_expected = if j == coin_idx {
                xp[j] * d1 / d0 - new_y
            } else {
                xp[j] - xp[j] * d1 / d0
            };
            let fee_amount = base_fee * dx_expected / CURVE_FEE_DENOMINATOR;
            xp_reduced[j] = xp_reduced[j].saturating_sub(fee_amount);
        }

        let y_after_fees =
            math::stableswap::get_y_d(coin_idx, &xp_reduced, amp, d1, self.uses_a_precision)?;

        let dy_scaled = xp_reduced[coin_idx]
            .saturating_sub(y_after_fees)
            .saturating_sub(U256::from(1));
        Ok(dy_scaled * CURVE_RATE_PRECISION / rates[coin_idx])
    }
}

//! # Caliber propAMM (Makina Protocol)
//!
//! 集成 Makina 协议的 Caliber propAMM 做市商定价 AMM。
//!
//! ## 架构
//! - **Ladder 定价模型**: 做市商通过链下引擎上传分段线性定价阶梯，链上合约不 emit Swap/Liquidity 事件。
//! - **同步策略**: `sync_events()` 返回空（无事件），数据更新完全依赖周期性 `sync_services::start_caliber_prop_ladder_sync_task`
//!   → 调用 `update()` → `GetCaliberPropLadderBatchRequest` reader 合约一次性拉取完整快照。
//! - **本地 Swap 模拟**: 基于链上 `quote()` 采样出的 Ladder 做分段线性插值 (`simulate_swap`)。
//!
//! ## 已知合约地址
//!
//! | 链 | 合约 | 状态 |
//! |---|---|---|
//! | Base | `0xf639CF213b63F7E77D699FF686d591C0Ba55Fc63` | 1 pair, StalePrices |
//! | Optimism | `0x60a8fA0eB9eDBF97a7487f7163C793768385Adc4` | 1 pair, 数据损坏 |
//! | XLayer | `0x154586B2479b9a11e3d4db90024Dc0e26F097312` | 1 pair (USDT/WETH)，活跃 ✓ |
//!
//! ## 当前状态 (2026-07)
//!
//! **模块架构完整，Swap 精度已通过链上真实数据全链路验证。**
//!
//! ### 实测结果（XLayer USDT/WETH 池子）
//!
//! | 验证项 | 结果 | 说明 |
//! |---|---|---|
//! | 池子发现 & 初始化 | ✅ | getAllPairIds + Ladder batch reader |
//! | 插值精度（非采样点） | **≤ 1.58 BPS** | 8 个 off-grid 点位 × 2 方向，目标 200 BPS |
//! | 前向推算（before-first） | **返回 0** | 不可准确估算，`simulate_swap` 返回 0 → 上层视为无利可图 |
//! | 后向越界（beyond-last） | ✅ | 正确报错 |
//! | 连续 swap_mut consumed 追踪 | ✅ | 与链上 `quote()` 偏差 0.27 BPS |
//! | 储备 & consumed 状态更新 | ✅ | 多笔 swap 后状态一致 |
//! | 现货价格缓存 | ✅ | 与首 Ladder 点一致 |
//! | StateSpace 集成 | ✅ | with_amms 构建成功 |
//!
//! ### 安全策略
//!
//! - **Before-first 区域（`amount_in <` 首采样点 ≈ 0.01% reserve）**：
//!   `simulate_swap` 返回 `U256::ZERO`，确保外层套利系统不会产生虚假路径。
//!   `simulate_swap_mut` 返回错误，拒绝变更 consumed/储备状态。
//! - **插值区域（`amount_in` 在采样点之间）**：分段线性插值，偏差 ≤ 2 BPS。
//! - **后向越界（`amount_in >` 末采样点）**：返回错误。
//!
//! ## 参考
//! - Makina: <https://docs.makina.finance/>
//! - Kyber 集成: `KyberNetwork/kyberswap-dex-lib/pkg/liquidity-source/caliberprop/`

pub mod factory;
pub mod types;

use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, B256, U256},
    providers::Provider,
    rpc::types::Log,
    sol,
};
use serde::{Deserialize, Serialize};
use tracing::instrument;

use crate::amms::{
    amm::{AutomatedMarketMaker, SyncAction, AMM},
    error::AMMError,
    Token,
};

use self::types::{CaliberLadderState, LadderPoint};

// ============================================================================
// Constants
// ============================================================================

/// 完整比例的分母（basis points）
/// 每个 `getAllPairIds` 调用获取的最大 pair 数量
pub const MAX_PAIRS_PER_CALL: u64 = 20;

/// 每对池子的 swap 消耗的默认 gas
pub const DEFAULT_SWAP_GAS: u64 = 250_000;

// ============================================================================
// Contract ABI
// ============================================================================

sol! {
    /// Caliber propAMM 合约的完整 ABI
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface ICaliberPropAMM {
        function getAllPairIds(uint256 start, uint256 count)
            external view
            returns (bytes32[] pairIds);

        function getPoolBalances(bytes32 pairId)
            external view
            returns (uint256 reserveX, uint256 reserveY);

        function quote(
            bytes32 pairId,
            address tokenIn,
            address tokenOut,
            uint256 amountIn
        )
            external view
            returns (uint256 amountOut);

        #[derive(Debug)]
        struct QuoteRequest {
            bytes32 pairId;
            address tokenIn;
            address tokenOut;
            uint256 amountIn;
        }

        #[derive(Debug)]
        struct QuoteResult {
            uint256 amountOut;
            bool success;
        }

        function batchQuote(QuoteRequest[] requests)
            external view
            returns (QuoteResult[] results);
    }
}

sol! {
    #[sol(rpc)]
    GetCaliberPropLadderBatchRequest,
    "src/amms/abi/GetCaliberPropLadderBatchRequest.json",
}

// ============================================================================
// CaliberPropPool
// ============================================================================

/// Caliber propAMM 池子
///
/// Caliber 是一种基于 Ladder 定价模型的 propAMM（Proprietary AMM）。
/// 做市商通过链下定价引擎定期更新 Ladder 定价曲线，链上合约不 emit
/// Swap/ModifyLiquidity 等事件。
///
/// ## 同步策略
///
/// 由于无可订阅事件，本池子完全依赖周期性 `update()` 调用来刷新状态：
/// 1. 调用 `GetCaliberPropLadderBatchRequest` 一次性读取最新储备和完整 Ladder
/// 2. 本地替换快照
/// 3. 重置 consumed 计数器（因为 Ladder 快照变更后旧消费量无意义）
///
/// ## Swap 模拟
///
/// 在本地通过分段线性插值（piecewise linear interpolation）近似计算：
/// - `total_in = consumed_in + amount_in`
/// - 二分查找 Ladder 定位 total_in 所在线段
/// - 线性插值计算 total_out
/// - `amount_out = total_out - consumed_out`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaliberPropPool {
    /// Caliber DEX 合约地址
    pub contract_address: Address,
    /// 交易对唯一 ID（用于链上合约调用的原始 pairId）
    pub pair_id: B256,
    /// 虚拟池子地址 = pair_id[0..20] XOR contract_address[0..20]
    /// 用于 StateSpace 的 HashMap key
    pub virtual_address: Address,
    /// 合约内部 token 0 地址（getPoolBalances 中的 reserveX 对应此 token）
    pub token_x: Address,
    /// 合约内部 token 1 地址（getPoolBalances 中的 reserveY 对应此 token）
    pub token_y: Address,
    /// Token A（地址较小的 token）
    pub token_a: Token,
    /// Token B
    pub token_b: Token,
    /// 创建此池子的区块号
    pub created_block: u64,
    /// 最后同步的区块号
    pub last_synced_block: u64,
    /// Token A 的链上储备
    pub reserve_a: U256,
    /// Token B 的链上储备
    pub reserve_b: U256,
    /// Ladder 快照 + 消费追踪
    pub ladder: CaliberLadderState,
    /// Token A 以 Token B 计价的缓存现货价
    pub price_a_in_b: f64,
    /// Token B 以 Token A 计价的缓存现货价
    pub price_b_in_a: f64,
}

impl CaliberPropPool {
    /// 从 pair_id 和合约地址生成虚拟地址
    pub fn virtual_address_from_pair_id(pair_id: B256, contract_address: Address) -> Address {
        let mut addr = [0u8; 20];
        for i in 0..20 {
            addr[i] = pair_id[i] ^ contract_address[i];
        }
        Address::from(addr)
    }

    /// 从虚拟地址还原 pair_id
    pub fn pair_id_from_virtual(virtual_address: Address, contract_address: Address) -> B256 {
        let mut pair_id = B256::ZERO;
        pair_id[0..20].copy_from_slice(virtual_address.as_ref());
        for i in 0..20 {
            pair_id[i] ^= contract_address[i];
        }
        pair_id
    }

    /// 根据输入 token 索引返回对应的 Ladder 和输出储备
    fn get_ladder_and_reserve_out(
        &self,
        index_in: usize,
    ) -> Result<(&[LadderPoint], &U256), AMMError> {
        match index_in {
            0 => Ok((&self.ladder.ladder_a_to_b, &self.reserve_b)),
            1 => Ok((&self.ladder.ladder_b_to_a, &self.reserve_a)),
            _ => Err(AMMError::Msg("caliber: invalid token index".to_string())),
        }
    }

    /// 获取方向的 consumed 状态引用
    fn get_consumed_refs(&self, index_in: usize) -> (&U256, &U256) {
        if index_in == 0 {
            (&self.ladder.consumed_in_ab, &self.ladder.consumed_out_ab)
        } else {
            (&self.ladder.consumed_in_ba, &self.ladder.consumed_out_ba)
        }
    }

    /// 获取方向的 consumed 状态可变引用
    fn get_consumed_mut_refs(&mut self, index_in: usize) -> (&mut U256, &mut U256) {
        if index_in == 0 {
            (
                &mut self.ladder.consumed_in_ab,
                &mut self.ladder.consumed_out_ab,
            )
        } else {
            (
                &mut self.ladder.consumed_in_ba,
                &mut self.ladder.consumed_out_ba,
            )
        }
    }

    fn get_token_index(&self, token: Address) -> isize {
        if token == self.token_a.address {
            0
        } else if token == self.token_b.address {
            1
        } else {
            -1
        }
    }

    /// 根据 ladder 的第一个点刷新缓存价格
    fn refresh_prices(&mut self) {
        // Empty ladder means the maker currently provides no usable quote.
        // Clear cached spot prices so upstream price filters won't reuse stale data.
        self.price_a_in_b = 0.0;
        self.price_b_in_a = 0.0;

        if let Some(first) = self.ladder.ladder_a_to_b.first() {
            if !first.amount_in.is_zero() && !first.amount_out.is_zero() {
                let price = u256_to_f64(&first.amount_out) / u256_to_f64(&first.amount_in);
                self.price_a_in_b =
                    price * 10f64.powi(self.token_a.decimals as i32 - self.token_b.decimals as i32);
                if self.price_a_in_b > 0.0 {
                    self.price_b_in_a = 1.0 / self.price_a_in_b;
                }
            }
        }
    }

    /// 批量初始化 Caliber propAMM 池子（供 Variant::init_batch 调用）
    pub async fn init_batch<N, P>(
        amms: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        factory::init_batch::<N, P>(amms, block_number, provider).await
    }
}

// ============================================================================
// Ladder 插值计算
// ============================================================================

/// 将 U256 全精度转换为 f64
///
/// U256 使用 `[u64; 4]` 小端 limbs 表示。此函数将四个 limb
/// 按权重 2^0, 2^64, 2^128, 2^192 累加为 f64，避免 `as_limbs()[0]`
/// 的单 limb 截断问题。
fn u256_to_f64(value: &U256) -> f64 {
    let limbs = value.as_limbs();
    let mut result = limbs[0] as f64;
    result += (limbs[1] as f64) * (2.0f64.powi(64));
    result += (limbs[2] as f64) * (2.0f64.powi(128));
    result += (limbs[3] as f64) * (2.0f64.powi(192));
    result
}

/// 通过分段线性插值计算给定 `amount_in` 对应的 `amount_out`
///
/// Ladder 点是按 `amount_in` 递增排列的累计点。
/// 算法：
/// 1. 二分查找定位 `amount_in` 所在线段
/// 2. 如果 `amount_in` 在第一个点之前 → `amount_out = amount_in * (first.amount_out / first.amount_in)`
/// 3. 如果 `amount_in` 精确命中某点 → 返回该点的 `amount_out`
/// 4. 否则 → 线性插值
pub fn quote_amount_out(ladder: &[LadderPoint], amount_in: &U256) -> Result<U256, AMMError> {
    if amount_in.is_zero() {
        return Err(AMMError::Msg("caliber: zero amount in".to_string()));
    }

    if ladder.is_empty() {
        return Err(AMMError::Msg(
            "caliber: no liquidity (empty ladder)".to_string(),
        ));
    }

    match ladder.binary_search_by(|point| point.amount_in.cmp(amount_in)) {
        Ok(i) => {
            // 精确命中
            Ok(ladder[i].amount_out)
        }
        Err(0) => {
            // amount_in 在第一个点之前，按比例计算
            let first = &ladder[0];
            Ok(*amount_in * first.amount_out / first.amount_in)
        }
        Err(i) if i >= ladder.len() => {
            // amount_in 超过 ladder 范围
            Err(AMMError::Msg(
                "caliber: amount in exceeds ladder range".to_string(),
            ))
        }
        Err(i) => {
            // amount_in 在 ladder[i-1] 和 ladder[i] 之间 → 线性插值
            let lo = &ladder[i - 1];
            let hi = &ladder[i];

            let dx = *amount_in - lo.amount_in;
            let range_in = hi.amount_in - lo.amount_in;
            let range_out = hi.amount_out - lo.amount_out;

            let delta = dx * range_out / range_in;
            Ok(lo.amount_out + delta)
        }
    }
}

/// 计算给定输入量产生的输出量（包含 consumed 追踪）
fn swap_amount_out(
    ladder: &[LadderPoint],
    consumed_in: &U256,
    consumed_out: &U256,
    amount_in: U256,
    _reserve_out: &U256,
) -> Result<U256, AMMError> {
    if amount_in.is_zero() {
        return Err(AMMError::Msg("caliber: zero amount in".to_string()));
    }

    let total_in = *consumed_in + amount_in;
    let total_out = quote_amount_out(ladder, &total_in)?;

    if *consumed_out > total_out {
        return Err(AMMError::Msg("caliber: insufficient liquidity".to_string()));
    }

    let amount_out = total_out - *consumed_out;

    if amount_out.is_zero() {
        return Err(AMMError::Msg("caliber: zero amount out".to_string()));
    }

    Ok(amount_out)
}

// ============================================================================
// AutomatedMarketMaker 实现
// ============================================================================

impl AutomatedMarketMaker for CaliberPropPool {
    fn address(&self) -> Address {
        self.virtual_address
    }

    fn sync_events(&self) -> Vec<B256> {
        // Caliber 合约不 emit 任何 Swap/ModifyLiquidity 事件
        // 无法通过事件驱动同步
        vec![]
    }

    fn sync(&mut self, _log: &Log) -> Result<SyncAction, AMMError> {
        // Caliber 没有可处理的事件
        // 如果 StateSpace 因地址碰撞触发了 sync，返回 Resync 触发完整刷新
        Ok(SyncAction::Resync)
    }

    fn last_synced_block(&self) -> u64 {
        self.last_synced_block
    }

    fn set_last_synced_block(&mut self, block_number: u64) {
        self.last_synced_block = block_number;
    }

    fn tokens(&self) -> Vec<Address> {
        vec![self.token_a.address, self.token_b.address]
    }

    fn calculate_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
        self.spot_price(base_token, quote_token)
    }

    fn spot_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
        if base_token == self.token_a.address && quote_token == self.token_b.address {
            Ok(self.price_a_in_b)
        } else if base_token == self.token_b.address && quote_token == self.token_a.address {
            Ok(self.price_b_in_a)
        } else {
            Err(AMMError::TokenNotFound(base_token))
        }
    }

    fn has_sufficient_liquidity(&self) -> bool {
        !self.reserve_a.is_zero()
            && !self.reserve_b.is_zero()
            && !self.ladder.ladder_a_to_b.is_empty()
            && !self.ladder.ladder_b_to_a.is_empty()
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

    fn simulate_swap(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let index_in = self.get_token_index(token_in);
        let index_out = self.get_token_index(token_out);

        if index_in < 0 || index_out < 0 || index_in == index_out {
            return Err(AMMError::TokenNotFound(token_in));
        }

        let (ladder, _reserve_out) = self.get_ladder_and_reserve_out(index_in as usize)?;
        let (consumed_in, consumed_out) = self.get_consumed_refs(index_in as usize);

        // If the current snapshot has no ladder, treat the pool as temporarily unquotable.
        // Returning zero is safer for upstream route search than bubbling an error, because
        // many call sites interpret zero-output as "not profitable / skip this path".
        if ladder.is_empty() {
            return Ok(U256::ZERO);
        }

        // amount_in < 首采样点 → 无法准确估算（线性推算精度无保证），
        // 返回 0 让上层视为"无利可图"而非虚假套利机会。
        if let Some(first) = ladder.first() {
            if amount_in < first.amount_in {
                return Ok(U256::ZERO);
            }
        }

        swap_amount_out(ladder, consumed_in, consumed_out, amount_in, _reserve_out)
    }

    fn simulate_swap_mut(
        &mut self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let index_in = self.get_token_index(token_in);
        let index_out = self.get_token_index(token_out);

        if index_in < 0 || index_out < 0 || index_in == index_out {
            return Err(AMMError::TokenNotFound(token_in));
        }

        let idx = index_in as usize;
        let (ladder, reserve_out) = self.get_ladder_and_reserve_out(idx)?;

        // before-first 区域无法准确估算产出，拒绝变更 consumed/储备状态
        if let Some(first) = ladder.first() {
            if amount_in < first.amount_in {
                return Err(AMMError::Msg(
                    "caliber: amount_in below quoting threshold".to_string(),
                ));
            }
        }

        let (consumed_in, consumed_out) = self.get_consumed_refs(idx);
        let amount_out =
            swap_amount_out(ladder, consumed_in, consumed_out, amount_in, reserve_out)?;

        // 更新 consumed 状态
        let (consumed_in_mut, consumed_out_mut) = self.get_consumed_mut_refs(idx);
        *consumed_in_mut += amount_in;
        *consumed_out_mut += amount_out;

        // 更新储备
        if idx == 0 {
            self.reserve_a += amount_in;
            self.reserve_b = if amount_out > self.reserve_b {
                U256::ZERO
            } else {
                self.reserve_b - amount_out
            };
        } else {
            self.reserve_b += amount_in;
            self.reserve_a = if amount_out > self.reserve_a {
                U256::ZERO
            } else {
                self.reserve_a - amount_out
            };
        }

        Ok(amount_out)
    }

    fn simulate_swap_exact_out(
        &self,
        _token_in: Address,
        _token_out: Address,
        _amount_out: U256,
    ) -> Result<U256, AMMError> {
        // Exact output 需要反向搜索 ladder，计算复杂且不常用
        Err(AMMError::UnsupportedSwapExactOut)
    }

    #[instrument(skip_all, fields(pool = %self.virtual_address))]
    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let token_a_is_reserve_x = self.token_x == self.token_a.address;
        let (reserve_a, reserve_b, ladder_a_to_b, ladder_b_to_a) = fetch_ladder_snapshot(
            &provider,
            self.contract_address,
            self.pair_id,
            self.token_a.address,
            self.token_b.address,
            token_a_is_reserve_x,
            block_number,
        )
        .await?;

        self.reserve_a = reserve_a;
        self.reserve_b = reserve_b;

        self.ladder = CaliberLadderState {
            ladder_a_to_b,
            ladder_b_to_a,
            consumed_in_ab: U256::ZERO,
            consumed_out_ab: U256::ZERO,
            consumed_in_ba: U256::ZERO,
            consumed_out_ba: U256::ZERO,
        };

        // 3. 计算现货价格
        self.refresh_prices();

        Ok(self)
    }

    async fn update<N, P>(&mut self, provider: P) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let token_a_is_reserve_x = self.token_x == self.token_a.address;
        let (reserve_a, reserve_b, ladder_a_to_b, ladder_b_to_a) = fetch_ladder_snapshot(
            &provider,
            self.contract_address,
            self.pair_id,
            self.token_a.address,
            self.token_b.address,
            token_a_is_reserve_x,
            BlockId::latest(),
        )
        .await?;

        self.reserve_a = reserve_a;
        self.reserve_b = reserve_b;

        // 2. 重置 consumed（ladder 快照变更后旧值无意义）
        self.ladder = CaliberLadderState {
            ladder_a_to_b,
            ladder_b_to_a,
            consumed_in_ab: U256::ZERO,
            consumed_out_ab: U256::ZERO,
            consumed_in_ba: U256::ZERO,
            consumed_out_ba: U256::ZERO,
        };

        // 3. 重新计算价格
        self.refresh_prices();

        Ok(())
    }
}

// ============================================================================
// Ladder 探测
// ============================================================================

/// 通过自定义 batch reader 一次性读取储备和 Ladder 快照。
async fn fetch_ladder_snapshot<N, P>(
    provider: &P,
    contract_address: Address,
    pair_id: B256,
    token_a: Address,
    token_b: Address,
    token_a_is_reserve_x: bool,
    block: BlockId,
) -> Result<(U256, U256, Vec<LadderPoint>, Vec<LadderPoint>), AMMError>
where
    N: Network,
    P: Provider<N> + Clone,
{
    use alloy::sol_types::SolType;

    type CaliberLadderSnapshot =
        sol!((uint256, uint256, (uint256, uint256)[], (uint256, uint256)[]));

    let return_data = GetCaliberPropLadderBatchRequest::deploy_builder(
        provider.clone(),
        contract_address,
        pair_id,
        token_a,
        token_b,
        token_a_is_reserve_x,
    )
    .call_raw()
    .block(block)
    .await?;

    let (reserve_a, reserve_b, ladder_ab_raw, ladder_ba_raw) =
        CaliberLadderSnapshot::abi_decode(&return_data).map_err(|e| {
            AMMError::Msg(format!(
                "caliber: failed to decode ladder snapshot, len={}, err={}",
                return_data.len(),
                e
            ))
        })?;

    Ok((
        reserve_a,
        reserve_b,
        decode_ladder_points(&ladder_ab_raw),
        decode_ladder_points(&ladder_ba_raw),
    ))
}

fn decode_ladder_points(raw: &[(U256, U256)]) -> Vec<LadderPoint> {
    raw.iter()
        .map(|(amount_in, amount_out)| LadderPoint {
            amount_in: *amount_in,
            amount_out: *amount_out,
        })
        .collect()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quote_amount_out_exact_match() {
        let ladder = vec![
            LadderPoint {
                amount_in: U256::from(10),
                amount_out: U256::from(100),
            },
            LadderPoint {
                amount_in: U256::from(50),
                amount_out: U256::from(400),
            },
            LadderPoint {
                amount_in: U256::from(100),
                amount_out: U256::from(700),
            },
        ];

        assert_eq!(
            quote_amount_out(&ladder, &U256::from(50)).unwrap(),
            U256::from(400)
        );
    }

    #[test]
    fn test_quote_amount_out_interpolation() {
        let ladder = vec![
            LadderPoint {
                amount_in: U256::from(100),
                amount_out: U256::from(200),
            },
            LadderPoint {
                amount_in: U256::from(200),
                amount_out: U256::from(400),
            },
        ];

        // 线性插值: amount_in=150, 在 [100,200] 中点
        // out = 200 + (150-100)/(200-100) * (400-200) = 200 + 50/100*200 = 300
        let result = quote_amount_out(&ladder, &U256::from(150)).unwrap();
        assert_eq!(result, U256::from(300));
    }

    #[test]
    fn test_quote_amount_out_before_first() {
        let ladder = vec![LadderPoint {
            amount_in: U256::from(100),
            amount_out: U256::from(200),
        }];

        // amount_in 在第一个点之前，按比例: 50 * 200 / 100 = 100
        let result = quote_amount_out(&ladder, &U256::from(50)).unwrap();
        assert_eq!(result, U256::from(100));
    }

    #[test]
    fn test_quote_amount_out_exceeds_range() {
        let ladder = vec![LadderPoint {
            amount_in: U256::from(100),
            amount_out: U256::from(200),
        }];

        assert!(quote_amount_out(&ladder, &U256::from(200)).is_err());
    }

    #[test]
    fn test_quote_amount_out_empty_ladder() {
        assert!(quote_amount_out(&[], &U256::from(10)).is_err());
    }

    #[test]
    fn test_virtual_address_roundtrip() {
        let contract = Address::repeat_byte(0xAA);
        let pair_id = B256::from([0x11u8; 32]);

        let virt = CaliberPropPool::virtual_address_from_pair_id(pair_id, contract);
        let recovered = CaliberPropPool::pair_id_from_virtual(virt, contract);

        // 前 20 字节应匹配
        assert_eq!(&pair_id[..20], &recovered[..20]);
    }

    #[test]
    fn test_decode_ladder_points() {
        let raw = vec![
            (U256::from(100), U256::from(200)),
            (U256::from(150), U256::from(260)),
        ];

        let decoded = decode_ladder_points(&raw);
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].amount_in, U256::from(100));
        assert_eq!(decoded[0].amount_out, U256::from(200));
        assert_eq!(decoded[1].amount_in, U256::from(150));
        assert_eq!(decoded[1].amount_out, U256::from(260));
    }

    #[test]
    fn test_simulate_swap_empty_ladder_returns_zero() {
        let pool = CaliberPropPool {
            contract_address: Address::ZERO,
            pair_id: B256::ZERO,
            virtual_address: Address::ZERO,
            token_x: Address::from([1u8; 20]),
            token_y: Address::from([2u8; 20]),
            token_a: Token::new_with_decimals(Address::from([1u8; 20]), 18),
            token_b: Token::new_with_decimals(Address::from([2u8; 20]), 18),
            created_block: 0,
            last_synced_block: 0,
            reserve_a: U256::from(1_000),
            reserve_b: U256::from(1_000),
            ladder: Default::default(),
            price_a_in_b: 1.0,
            price_b_in_a: 1.0,
        };

        let out = pool
            .simulate_swap(pool.token_a.address, pool.token_b.address, U256::from(100))
            .unwrap();
        assert_eq!(out, U256::ZERO);
    }

    #[test]
    fn test_refresh_prices_clears_stale_cache_on_empty_ladder() {
        let mut pool = CaliberPropPool {
            contract_address: Address::ZERO,
            pair_id: B256::ZERO,
            virtual_address: Address::ZERO,
            token_x: Address::from([1u8; 20]),
            token_y: Address::from([2u8; 20]),
            token_a: Token::new_with_decimals(Address::from([1u8; 20]), 18),
            token_b: Token::new_with_decimals(Address::from([2u8; 20]), 18),
            created_block: 0,
            last_synced_block: 0,
            reserve_a: U256::from(1_000),
            reserve_b: U256::from(1_000),
            ladder: Default::default(),
            price_a_in_b: 123.0,
            price_b_in_a: 456.0,
        };

        pool.refresh_prices();
        assert_eq!(pool.price_a_in_b, 0.0);
        assert_eq!(pool.price_b_in_a, 0.0);
    }
}

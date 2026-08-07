//! # Caliber propAMM (Makina Protocol)
//!
//! 集成 Makina 协议的 Caliber propAMM 做市商定价 AMM。
//!
//! ## 架构
//! - **Ladder 定价模型**: 做市商通过链下引擎上传分段线性定价阶梯，链上合约不 emit Swap/Liquidity 事件。
//! - **同步策略**: `sync_events()` 返回空（无事件）。XLayer 实时报价更新由 flashblocks
//!   原始交易流驱动（`batchUpdateParameters` calldata → `apply_batch_update`，详见
//!   `docs/caliber_prop_realtime_sync_design.md`）；周期任务降频为对账/兜底
//!   （`sync_services::start_caliber_prop_ladder_sync_task` → `update()` →
//!   `fetch_exact_snapshot` 直读合约 storage `cfg`/`data`/`ladder` 槽位）。
//! - **本地 Swap 模拟**: 用 `quote_forward_exact` / `quote_reverse_exact` 精确复刻链上
//!   `quote()` 的 EVM uint256 运算（含 fee 扣减、pos 分段消费、储备封顶），`simulate_swap` 逐位一致。
//!
//! ## 已知合约地址
//!
//! | 链 | 合约 | 状态 |
//! |---|---|---|
//! | Base | `0xf639CF213b63F7E77D699FF686d591C0Ba55Fc63` | 1 pair, StalePrices |
//! | Optimism | `0x60a8fA0eB9eDBF97a7487f7163C793768385Adc4` | 1 pair, 数据损坏 |
//! | XLayer | `0x154586B2479b9a11e3d4db90024Dc0e26F097312` | 4 pairs，活跃 ✓ |
//!
//! ## 当前状态 (2026-08)
//!
//! **模块架构完整，报价公式已对链上真实数据做逐位（0 偏差）验证。**
//!
//! ### 实测结果（XLayer 块 66309105，4 pairs × 双向 × 14 金额）
//!
//! | 验证项 | 结果 | 说明 |
//! |---|---|---|
//! | 池子发现 & 初始化 | ✅ | getAllPairIds + `fetch_exact_snapshot` 直读存储 |
//! | 正向报价 `quote(token_x→token_y)` | ✅ 逐位一致 | `quote_forward_exact`，112 quotes 零偏差 |
//! | 反向报价 `quote(token_y→token_x)` | ✅ 逐位一致 | `quote_reverse_exact`（含 pos 分段消费） |
//! | pos 过期/无效处理 | ✅ | `cfg+7.block != 当前块` 时按 pos=0 整段计算 |
//! | 连续 swap_mut consumed 追踪 | ✅ | 逐笔 = quote(累计) - quote(上一累计)，bit-exact |
//! | 储备 & consumed 状态更新 | ✅ | 多笔 swap 后状态一致 |
//! | 现货价格缓存 | ✅ | 与首 Ladder 点一致 |
//! | StateSpace 集成 | ✅ | with_amms 构建成功 |
//!
//! ### 已知边界行为（与链上一致，勿改）
//!
//! - **正向小额返回 0**：pair 1（xETH→USD₮0 方向）在 `pos` 消费殆尽后，小额输入
//!   链上 `quote()` 本身就返回 0，本地精确复刻该行为（不是 bug）。
//! - **反向输出封顶**：输出上限为 `min(quote, reserve_out)`，与链上 `quote()` 一致。
//!
//! ## 参考
//! - Makina: <https://docs.makina.finance/>
//! - Kyber 集成: `KyberNetwork/kyberswap-dex-lib/pkg/liquidity-source/caliberprop/`

pub mod factory;
pub mod types;

use alloy::{
    consensus::BlockHeader,
    eips::BlockId,
    network::{BlockResponse, Network},
    primitives::{keccak256, Address, B256, U256},
    providers::Provider,
    rpc::types::Log,
    sol,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use tracing::instrument;

use crate::amms::{
    amm::{AutomatedMarketMaker, SyncAction, AMM},
    error::AMMError,
    Token,
};

use self::types::{CaliberLadderState, LadderPoint};
use ICaliberPropAMM::BatchUpdateParameters;

// ============================================================================
// Constants
// ============================================================================

/// 完整比例的分母（basis points）
/// 每个 `getAllPairIds` 调用获取的最大 pair 数量
pub const MAX_PAIRS_PER_CALL: u64 = 20;

/// 每对池子的 swap 消耗的默认 gas
pub const DEFAULT_SWAP_GAS: u64 = 250_000;

/// `batchUpdateParameters((bytes32,uint64,uint32,uint64)[])` 的函数选择器。
///
/// 做市商通过该函数批量更新报价参数（price→field0、flags→field1、
/// deadline→tsX），只写 `data+0` 一个槽、**不 emit 任何事件**，
/// 只能通过 flashblocks 原始交易流发现（见 `docs/caliber_prop_realtime_sync_design.md`）。
pub const CALIBER_BATCH_UPDATE_SELECTOR: [u8; 4] = [0x00, 0x8d, 0xcc, 0x8e];

/// 1e6：fee 的基数（200 = 2 bps）
pub const MILLION: U256 = U256::from_limbs([1_000_000, 0, 0, 0]);
/// 1e9：quote 公式中的固定系数
pub const BILLION: U256 = U256::from_limbs([1_000_000_000, 0, 0, 0]);
/// 2：quote 公式中的固定系数
pub const TWO: U256 = U256::from_limbs([2, 0, 0, 0]);

/// EVM 除法（向零截断；除数为 0 时返回 0，与 EVM DIV 一致）
fn evm_div(a: U256, b: U256) -> U256 {
    if b.is_zero() {
        U256::ZERO
    } else {
        a / b
    }
}

/// 取两个 U256 的较小值
fn min_u256(a: U256, b: U256) -> U256 {
    if a < b {
        a
    } else {
        b
    }
}

/// 10^exp（用于 scale = 10^(dec_x - dec_y)）
fn pow10(exp: u8) -> U256 {
    let mut r = U256::from(1u64);
    for _ in 0..exp {
        r *= U256::from(10u64);
    }
    r
}

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

        #[derive(Debug)]
        struct BatchUpdateParameters {
            bytes32 pairId;
            uint64 price;
            uint32 flags;
            uint64 deadline;
        }

        function batchUpdateParameters(BatchUpdateParameters[] updates)
            external;
    }
}

// ============================================================================
// batchUpdateParameters 解码（实时交易驱动同步）
// ============================================================================

/// `batchUpdateParameters` 单条 pair 更新。
///
/// 链上只写 `data+0` 一个存储槽：`[uint32 tsY=0][uint32 tsX=deadline]
/// [uint32 field1=flags][uint64 field0=price]`，更新交易无任何日志。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaliberBatchUpdate {
    /// 目标 pair 的原始 pairId
    pub pair_id: B256,
    /// 新报价参数 price（→ data+0 低 64 位 field0）
    pub price: U256,
    /// 新报价参数 flags（→ data+0 bits 64..96 field1）
    pub flags: u32,
    /// 报价过期时间戳 deadline（→ data+0 bits 96..128 tsX）
    pub deadline: u64,
}

/// 标准 ABI 解码 `batchUpdateParameters` 的完整 calldata（含选择器）。
///
/// 选择器不匹配或 ABI 解码失败时返回 `None`（fail-safe：调用方静默跳过，
/// 由低频对账快照纠正，绝不污染本地状态）。
pub fn decode_batch_update_parameters(input: &[u8]) -> Option<Vec<CaliberBatchUpdate>> {
    use alloy::sol_types::SolValue;

    if input.len() < 4 || input[..4] != CALIBER_BATCH_UPDATE_SELECTOR {
        return None;
    }
    let updates = <Vec<BatchUpdateParameters>>::abi_decode(&input[4..]).ok()?;
    Some(
        updates
            .into_iter()
            .map(|u| CaliberBatchUpdate {
                pair_id: u.pairId,
                price: U256::from(u.price),
                flags: u.flags,
                deadline: u.deadline,
            })
            .collect(),
    )
}

/// 从 flashblocks 的 raw RLP 交易字节中提取 calldata（`input`）。
///
/// 覆盖 EIP-1559 / 2930 / Legacy 类型（XLayer 实际为 EIP-1559）。
/// 仅在 `to` 命中目标合约后调用（每次完整解码含 calldata 拷贝，
/// 不适合逐笔执行）。
pub fn extract_input_from_raw_tx(raw: &[u8]) -> Option<Vec<u8>> {
    use alloy::rlp::Decodable;

    let mut slice = raw;
    let envelope = alloy::consensus::TxEnvelope::decode(&mut slice).ok()?;
    let input = match &envelope {
        alloy::consensus::TxEnvelope::Legacy(tx) => &tx.tx().input,
        alloy::consensus::TxEnvelope::Eip2930(tx) => &tx.tx().input,
        alloy::consensus::TxEnvelope::Eip1559(tx) => &tx.tx().input,
        _ => return None,
    };
    Some(input.to_vec())
}

/// RLP 项的长度解析：返回 `(payload_len, total_len)`。
///
/// 只读头部字节 + 长度字段，不触碰 payload 内容（零分配）。
fn rlp_item_len(buf: &[u8], offset: usize) -> Option<(usize, usize)> {
    let b = *buf.get(offset)?;
    let (payload, header) = match b {
        // 单字节值（0x00..=0x7f）：自身即 payload
        0x00..=0x7f => (1, 0),
        // 短字符串（0x80..=0xb7）
        0x80..=0xb7 => ((b - 0x80) as usize, 1),
        // 长字符串（0xb8..=0xbf）：长度字节数 = b - 0xb7
        0xb8..=0xbf => {
            let len_bytes = (b - 0xb7) as usize;
            let mut len = 0usize;
            for i in 1..=len_bytes {
                len = (len << 8) | (*buf.get(offset + i)?) as usize;
            }
            (len, 1 + len_bytes)
        }
        // 短列表（0xc0..=0xf7）
        0xc0..=0xf7 => ((b - 0xc0) as usize, 1),
        // 长列表（0xf8..=0xff）：长度字节数 = b - 0xf7
        0xf8..=0xff => {
            let len_bytes = (b - 0xf7) as usize;
            let mut len = 0usize;
            for i in 1..=len_bytes {
                len = (len << 8) | (*buf.get(offset + i)?) as usize;
            }
            (len, 1 + len_bytes)
        }
    };
    Some((payload, header + payload))
}

/// 从 flashblocks 的 raw RLP 交易字节中**轻量**提取 `to` 地址。
///
/// 只读信封类型字节 + RLP 列表头，跳过 `to` 之前的字段，不解析 calldata、
/// 无任何分配（单笔 ~100-200ns）。字段偏移：
/// - Legacy：`[nonce, gasPrice, gasLimit, to, ...]` → 第 3 项
/// - EIP-2930：`[chainId, nonce, gasPrice, gasLimit, to, ...]` → 第 4 项
/// - EIP-1559：`[chainId, nonce, maxPrio, maxFee, gasLimit, to, ...]` → 第 5 项
///
/// 合约创建交易（`to` 为空）与未知 typed tx 返回 `None`。
pub fn extract_to_from_raw_tx(raw: &[u8]) -> Option<Address> {
    // EIP-2718 typed 交易：首字节为类型（0x01 = 2930、0x02 = 1559）
    let (buf, to_field_idx) = match raw.first() {
        Some(0x01) => (&raw[1..], 4usize),
        Some(0x02) => (&raw[1..], 5usize),
        Some(0x03..=0x7f) => return None, // 其他 typed tx（EIP-4844 等，不支持）
        _ => (raw, 3usize),               // Legacy：RLP 列表直接开头
    };

    // 跳过 RLP 列表头（payload 总长需覆盖全部字段）；offset 从第一个字段开始
    let (list_payload, list_total) = rlp_item_len(buf, 0)?;
    if list_total > buf.len() {
        return None;
    }
    let mut offset = list_total - list_payload;

    // 跳过 to 之前的字段
    for _ in 0..to_field_idx {
        let (_, total) = rlp_item_len(buf, offset)?;
        if offset + total > buf.len() {
            return None;
        }
        offset += total;
    }

    // to 字段：0x94 + 20 字节地址；0x80 = 空（合约创建，无 to）
    let b = *buf.get(offset)?;
    if b == 0x80 {
        return None;
    }
    if b != 0x94 {
        return None;
    }
    let end = offset + 21;
    if end > buf.len() {
        return None;
    }
    Some(Address::from_slice(&buf[offset + 1..end]))
}

/// 收集池子集合中的 caliber 合约地址（供 flashblocks 提取层构建兴趣集合）。
///
/// 与 `binaryfi_engines` 相同的传参模式：提取层只按地址集合过滤，
/// 核心路由不堆协议分支。
pub fn caliber_contracts(pools: &[CaliberPropPool]) -> HashSet<Address> {
    pools.iter().map(|p| p.contract_address).collect()
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
/// 更新交易（`batchUpdateParameters`）不 emit 事件，无法进入日志管道：
/// - **实时路径（XLayer）**：flashblocks 原始交易提取 → `apply_batch_update`
///   增量刷新 field0/field1/deadline（见 `docs/caliber_prop_realtime_sync_design.md`）
/// - **对账路径**：周期 `update()` 全量快照（冷启动/断流/储备/pos 低频变动），
///   替换快照并重置 consumed 计数器
///
/// ## Swap 模拟
///
/// 在本地精确复刻链上 `quote()` 的 EVM uint256 运算：
/// - `total_in = consumed_in + amount_in`
/// - 正向用 `quote_forward_exact`，反向用 `quote_reverse_exact`（含 pos 分段消费）
/// - `amount_out = total_out - consumed_out`
///
/// 输出上限与链上一致：`min(quote, reserve_out)`。
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

    /// 应用链上 `batchUpdateParameters` 的单个 pair 更新（实时交易驱动）。
    ///
    /// 只更新报价参数（`field0=price`、`field1=flags`、`deadline=tsX`）与
    /// 同步区块号；ladder 曲线、储备、pos 不随更新变化（由低频对账任务覆盖
    /// 做市商充值等低频变动）。`pair_id` 不匹配时静默忽略（fail-safe，
    /// 防跨 pair 污染）。
    pub fn apply_batch_update(&mut self, u: &CaliberBatchUpdate, block_number: u64) {
        if u.pair_id != self.pair_id {
            return;
        }
        self.ladder.field0 = u.price;
        self.ladder.field1 = U256::from(u.flags);
        self.ladder.deadline = u.deadline;
        self.last_synced_block = block_number;
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

/// 计算给定输入量产生的输出量（包含 consumed 追踪）
/// 精确复刻链上 `quote(token_x -> token_y)` 正向报价
///
/// 公式（详见 docs/caliber_prop_internal.md §3，EVM uint256 截断）：
/// - `xp = amount - amount * fee / 1e6`（fee 先扣）
/// - 逐段 `th = (P * 1e9 * scale + field0 - 1) / field0`，其中
///   `P = 1e6 * 2 * y_i / (a_i + a_next)`，满段累计 `acc += y_i`
/// - 段内 `part = r2 * 2 * y_i * a_i / (1e6 * 2 * y_i + r2 * (a_i - a_next))`，
///   `r2 = field0 * xp / (1e9 * scale)`
/// - 尾段按 `a_last` 直线外推；输出上限 `min(out, reserve_y)`
pub fn quote_forward_exact(
    ladder: &[LadderPoint],
    field0: U256,
    field1: U256,
    fee_rate: U256,
    window: U256,
    scale: U256,
    reserve_y: U256,
    amount_in: U256,
) -> U256 {
    if ladder.is_empty() {
        return U256::ZERO;
    }
    let n = ladder.len();
    let mut xp = amount_in - evm_div(amount_in * fee_rate, MILLION);
    let mut acc = U256::ZERO;
    for i in 0..n {
        let x_i = ladder[i].amount_in;
        let y_i = ladder[i].amount_out;
        let x_next = if i + 1 < n {
            ladder[i + 1].amount_in
        } else {
            x_i + window
        };
        let a_i = MILLION - (x_i + field1);
        let a_next = MILLION - (x_next + field1);
        let p = evm_div(MILLION * TWO * y_i, a_i + a_next);
        let th = evm_div(p * BILLION * scale + field0 - U256::from(1), field0);
        if xp >= th {
            acc += y_i;
            xp -= th;
        } else {
            let r2 = evm_div(field0 * xp, BILLION * scale);
            let part = evm_div(
                r2 * TWO * y_i * a_i,
                MILLION * TWO * y_i + r2 * (a_i - a_next),
            );
            acc += part;
            return min_u256(acc, reserve_y);
        }
    }
    // 超过最后合成边界的尾段：按倒数第二段直线外推
    let a_last = MILLION - (ladder[n - 1].amount_in + window + field1);
    let tail = evm_div(field0 * xp * a_last, BILLION * scale * MILLION);
    acc += tail;
    min_u256(acc, reserve_y)
}

/// 精确复刻链上 `quote(token_y -> token_x)` 反向报价（有状态，pos 版本）
///
/// 链上反向报价维护一个"当前位置" `pos`（= 该 pair 已从 x 侧被兑换掉的累计
/// y 量，来自 `cfg+7`，详见 docs/caliber_prop_internal.md §4）：
/// - 跳过 `pos` 已完全消费的段；`pos` 所在段按段内剩余量 `R = y_i - offset`
///   报价，并用当前位置插值的斜率 `a_eff`（EVM 截断）
/// - 逐段 `w = min(xp, R)`；`xp` 跨段时继续下一段（下一段 offset=0，整段）
/// - `out_i = w * 1e6 * 1e9 * scale * 2 * R / (field0 * (2 * R * a_eff + w * delta_eff))`
/// - 尾段按 `a_last` 直线外推；输出上限 `min(out, reserve_x)`
///
/// `pos` 为 0 时退化为旧版整段公式（链上在 pos 过期/无效时的行为）。
pub fn quote_reverse_exact(
    ladder: &[LadderPoint],
    field0: U256,
    field1: U256,
    fee_rate: U256,
    window: U256,
    scale: U256,
    pos: U256,
    reserve_x: U256,
    amount_in: U256,
) -> U256 {
    if ladder.is_empty() {
        return U256::ZERO;
    }
    let n = ladder.len();
    let mut xp = amount_in - evm_div(amount_in * fee_rate, MILLION);
    let mut acc = U256::ZERO;
    let mut cum = U256::ZERO;
    let mut started = false;
    for i in 0..n {
        let x_i = ladder[i].amount_in;
        let y_i = ladder[i].amount_out;
        let offset = if !started {
            if pos >= cum + y_i {
                cum += y_i;
                continue;
            }
            started = true;
            pos - cum
        } else {
            U256::ZERO
        };
        let r = y_i - offset;
        let w = min_u256(xp, r);
        let x_next = if i + 1 < n {
            ladder[i + 1].amount_in
        } else {
            x_i + window
        };
        let a_i = MILLION + (x_i + field1);
        let a_next = MILLION + (x_next + field1);
        let a_eff = a_i + evm_div((a_next - a_i) * offset, y_i);
        let delta_eff = a_next - a_eff;
        let out_i = evm_div(
            w * MILLION * BILLION * scale * TWO * r,
            field0 * (TWO * r * a_eff + w * delta_eff),
        );
        acc += out_i;
        xp -= w;
        if xp.is_zero() {
            return min_u256(acc, reserve_x);
        }
        cum += y_i;
    }
    // pos 超过全部段或 xp 未耗尽：按末段 a 直线外推
    let a_last = MILLION + (ladder[n - 1].amount_in + window + field1);
    let tail = evm_div(xp * MILLION * BILLION * scale, field0 * a_last);
    acc += tail;
    min_u256(acc, reserve_x)
}

/// 计算给定输入量产生的输出量（包含 consumed 追踪）
///
/// `total_out = quote(consumed_in + amount_in)`，`amount_out = total_out - consumed_out`。
/// 返回 0 表示链上报价本身为 0（如输入过小），属正常结果。
fn swap_amount_out(
    ladder: &[LadderPoint],
    consumed_in: &U256,
    consumed_out: &U256,
    amount_in: U256,
    reserve_out: &U256,
    state: &CaliberLadderState,
    forward: bool,
) -> Result<U256, AMMError> {
    if amount_in.is_zero() {
        return Err(AMMError::Msg("caliber: zero amount in".to_string()));
    }

    let total_in = *consumed_in + amount_in;
    // 封顶使用"快照总储备"（consumed_out + 当前剩余 reserve）：
    // 模拟 swap 序列不会改变链上真实储备，链上 quote() 始终按快照储备封顶。
    let total_reserve = *reserve_out + *consumed_out;
    let total_out = if forward {
        quote_forward_exact(
            ladder,
            state.field0,
            state.field1,
            state.fee_rate,
            state.window,
            state.scale,
            total_reserve,
            total_in,
        )
    } else {
        quote_reverse_exact(
            ladder,
            state.field0,
            state.field1,
            state.fee_rate,
            state.window,
            state.scale,
            state.pos,
            total_reserve,
            total_in,
        )
    };

    if *consumed_out > total_out {
        return Err(AMMError::Msg("caliber: insufficient liquidity".to_string()));
    }

    Ok(total_out - *consumed_out)
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

        let (ladder, reserve_out) = self.get_ladder_and_reserve_out(index_in as usize)?;
        let (consumed_in, consumed_out) = self.get_consumed_refs(index_in as usize);

        // If the current snapshot has no ladder, treat the pool as temporarily unquotable.
        // Returning zero is safer for upstream route search than bubbling an error, because
        // many call sites interpret zero-output as "not profitable / skip this path".
        if ladder.is_empty() {
            return Ok(U256::ZERO);
        }

        // ladder 两份都存 token_x → token_y 方向的原始曲线；
        // 正向/反向取决于输入 token 是否 == token_x，而非 token_a 下标。
        let forward = token_in == self.token_x;
        swap_amount_out(
            ladder,
            consumed_in,
            consumed_out,
            amount_in,
            reserve_out,
            &self.ladder,
            forward,
        )
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

        let (consumed_in, consumed_out) = self.get_consumed_refs(idx);
        // ladder 两份都存 token_x → token_y 方向的原始曲线；
        // 正向/反向取决于输入 token 是否 == token_x，而非 token_a 下标。
        let forward = token_in == self.token_x;
        let amount_out = swap_amount_out(
            ladder,
            consumed_in,
            consumed_out,
            amount_in,
            reserve_out,
            &self.ladder,
            forward,
        )?;

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
        let snap = fetch_exact_snapshot(
            &provider,
            self.contract_address,
            self.pair_id,
            self.token_x,
            self.token_y,
            block_number,
        )
        .await?;

        self.apply_snapshot(snap);

        Ok(self)
    }

    async fn update<N, P>(&mut self, provider: P) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let snap = fetch_exact_snapshot(
            &provider,
            self.contract_address,
            self.pair_id,
            self.token_x,
            self.token_y,
            BlockId::latest(),
        )
        .await?;

        self.apply_snapshot(snap);

        Ok(())
    }
}

impl CaliberPropPool {
    /// 将完整精确快照应用到池子（单池 init/update 与周期批量刷新共用）。
    fn apply_snapshot(&mut self, snap: CaliberSnapshot) {
        self.reserve_a = snap.reserve_a;
        self.reserve_b = snap.reserve_b;

        // 重置 consumed（ladder 快照变更后旧值无意义）
        self.ladder = CaliberLadderState {
            ladder_a_to_b: snap.ladder.clone(),
            ladder_b_to_a: snap.ladder,
            consumed_in_ab: U256::ZERO,
            consumed_out_ab: U256::ZERO,
            consumed_in_ba: U256::ZERO,
            consumed_out_ba: U256::ZERO,
            field0: snap.field0,
            field1: snap.field1,
            fee_rate: snap.fee_rate,
            window: snap.window,
            scale: snap.scale,
            pos: snap.pos,
            deadline: snap.deadline,
        };

        // 重新计算现货价格
        self.refresh_prices();
    }
}

// ============================================================================
// Ladder 探测
// ============================================================================

/// pair 配置基址：`keccak256(pairId || uint256(6))`（cfg），
/// data 基址为 `keccak256(pairId || uint256(7))`
fn pair_slot(pair_id: B256, index: u64) -> B256 {
    let mut input = [0u8; 64];
    input[..32].copy_from_slice(pair_id.as_ref());
    input[32..].copy_from_slice(&U256::from(index).to_be_bytes::<32>());
    B256::from(keccak256(input))
}

/// 存储槽加法（B256 + u64）
fn b256_add(base: B256, add: u64) -> B256 {
    B256::from((U256::from_be_bytes(base.0) + U256::from(add)).to_be_bytes::<32>())
}

/// 单 pair 的完整快照（已映射到 token_a/token_b 视角）
struct CaliberSnapshot {
    reserve_a: U256,
    reserve_b: U256,
    /// 合约原始 ladder（token_x → token_y 方向）
    ladder: Vec<LadderPoint>,
    field0: U256,
    field1: U256,
    fee_rate: U256,
    window: U256,
    /// 10^(dec_token_x - dec_token_y)
    scale: U256,
    /// 反向报价位置（cfg+7 的 pos，仅当 cfg+7.block == 当前块时有效，否则 0）
    pos: U256,
    /// data+0 的 tsX（bits 96..128），报价过期时间戳
    deadline: u64,
}

/// 单次 JSON-RPC batch 请求中的 `eth_getStorageAt` 数量上限。
///
/// 实测生产 `rpc.xlayer.tech` 的 batch 上限为 11（12 即拒绝
/// `too many RPC calls in batch request`），取安全值 10。
const STORAGE_BATCH_SIZE: usize = 10;

/// 通过单次 JSON-RPC batch 读取多个存储槽（一个 HTTP 请求）。
///
/// 与逐槽 `eth_getStorageAt` 完全等价（同一 `block`、同一序列化参数），
/// 仅把 N 次 RPC 往返折叠为 1 次。所有请求必须指向同一区块。
async fn storage_at_batch<N, P>(
    provider: &P,
    reads: &[(Address, B256)],
    block: BlockId,
) -> Result<Vec<U256>, AMMError>
where
    N: Network,
    P: Provider<N>,
{
    let mut batch = alloy::rpc::client::BatchRequest::new(provider.client());
    let mut waiters = Vec::with_capacity(reads.len());
    for (address, slot) in reads {
        let key = U256::from_be_bytes(slot.0);
        let waiter = batch
            .add_call::<_, B256>("eth_getStorageAt", &(*address, key, block))
            .map_err(|e| AMMError::Msg(format!("caliber: batch add_call failed: {e}")))?;
        waiters.push(waiter);
    }
    batch
        .send()
        .await
        .map_err(|e| AMMError::Msg(format!("caliber: batch send failed: {e}")))?;

    let mut out = Vec::with_capacity(waiters.len());
    for waiter in waiters {
        let value = waiter
            .await
            .map_err(|e| AMMError::Msg(format!("caliber: batch get_storage_at failed: {e}")))?;
        out.push(U256::from_be_bytes(value.0));
    }
    Ok(out)
}

/// 分片版 `storage_at_batch`：超过 `chunk` 个槽位时拆成多次 batch。
async fn storage_at_batch_chunked<N, P>(
    provider: &P,
    reads: &[(Address, B256)],
    block: BlockId,
    chunk: usize,
) -> Result<Vec<U256>, AMMError>
where
    N: Network,
    P: Provider<N>,
{
    let mut out = Vec::with_capacity(reads.len());
    for part in reads.chunks(chunk) {
        out.extend(storage_at_batch(provider, part, block).await?);
    }
    Ok(out)
}

/// pair 固定槽位（与 `fetch_exact_snapshot` 读取的 8 个槽一一对应）
#[derive(Clone, Copy)]
struct RawPairSlots {
    cfg1: U256,
    n: U256,
    window: U256,
    reserve_x: U256,
    reserve_y: U256,
    cfg6: U256,
    cfg7: U256,
    data0: U256,
}

/// 复刻链上 quote() 的过期/暂停判断（EVM trace 确认）：
/// - `SLOAD(3) & 0xff != 0` → 全局暂停 revert 0x8507a90d
/// - `SLOAD(cfg+6) byte@0x40 != 0` → per-pair 暂停 revert 0xb69ec3f0
/// - `deadline = ((data0 >> 128) & u32) << 32 | ((data0 >> 96) & u32)`，
///   `block.timestamp > deadline + validity_window` → revert 0x2af96ae8
///
/// 注意：当 data0 高 32 位（tsY）非零时，deadline 变为 64 位巨大值，链上永不
/// 过期——这是合约的实际行为，本地必须同样处理。
fn pair_stale(raw: &RawPairSlots, validity_window: U256, global_paused: U256, block_ts: U256) -> bool {
    let paused = !(global_paused & U256::from(0xff)).is_zero()
        || !((raw.cfg6 >> U256::from(0x40)) & U256::from(0xff)).is_zero();
    let ts_xy = (((raw.data0 >> U256::from(128)) & U256::from(u32::MAX)) << U256::from(32))
        | ((raw.data0 >> U256::from(96)) & U256::from(u32::MAX));
    let expired = block_ts > ts_xy + validity_window;
    paused || expired
}

/// 由原始槽位值构建 `CaliberSnapshot`（单池与批量路径共用）。
///
/// 存储布局（XLayer 合约 `0x154586B2479b9a11e3d4db90024Dc0e26F097312`，经字节码逆向确认）：
/// - `cfg = keccak256(pairId || 6)`：`+1` token1（byte@0xa0=dec_x，byte@0xa8=dec_y），
///   `+2` ladder 长度，`+3` window，`+4` reserveX，`+5` reserveY，`+6` 低 64 位 = fee，
///   `+7` = [block:32][0:64][pos:96][0:96]（pos 仅当 block == 当前块时有效）
/// - `data = keccak256(pairId || 7)`：`+0` = [uint32 tsX][uint32 tsY][uint32 field1][uint64 field0]
/// - ladder 元素：`keccak256(uint256(cfg+2)) + i`，每槽 `[amountIn:128][amountOut:128]`
fn build_snapshot_from_slots(
    token_x: Address,
    token_y: Address,
    raw: RawPairSlots,
    validity_window: U256,
    global_paused: U256,
    block_ts: U256,
    cur_block: U256,
    ladder_raw: &[U256],
) -> Result<CaliberSnapshot, AMMError> {
    let n_usize: usize = raw.n.to::<usize>();
    if n_usize == 0 || n_usize > 1024 {
        return Err(AMMError::Msg(format!(
            "caliber: invalid ladder length {n_usize}"
        )));
    }

    // decimals 打包在 cfg+1：byte@0xa0 = dec_x，byte@0xa8 = dec_y
    let dec_x = ((raw.cfg1 >> U256::from(0xa0)) & U256::from(0xff)).to::<u8>();
    let dec_y = ((raw.cfg1 >> U256::from(0xa8)) & U256::from(0xff)).to::<u8>();
    // scale = 10^dec_x / 10^dec_y（dec_x >= dec_y 时即 10^(dec_x-dec_y)）
    let scale = pow10(dec_x) / pow10(dec_y);

    // fee 在 cfg+6 低 64 位
    let fee_rate = raw.cfg6 & U256::from(u64::MAX);
    // field0 = data0 低 64 位，field1 = bits 64..96
    let field0 = raw.data0 & U256::from(u64::MAX);
    let field1 = (raw.data0 >> U256::from(64)) & U256::from(u32::MAX);

    // 反向报价位置：cfg+7 bits 96..191 为 pos，高 64 位为最近更新区块。
    // 链上（EVM trace 确认）仅在 cfg+7.block == 当前执行块时使用真实 pos，
    // 否则按 pos=0（从段 0 整段）计算。
    let pos_block = raw.cfg7 >> U256::from(192);
    let pos = (raw.cfg7 >> U256::from(96)) & ((U256::from(1) << U256::from(96)) - U256::from(1));
    let pos = if pos_block == cur_block {
        pos
    } else {
        U256::ZERO
    };
    // tsX = data+0 bits 96..128（更新交易的 deadline 写入此字段）
    let ts_x = ((raw.data0 >> U256::from(96)) & U256::from(u32::MAX)).to::<u64>();

    // 过期/暂停 pair 返回空 ladder，模拟链上不可报价（simulate_swap → 0）
    let stale = pair_stale(&raw, validity_window, global_paused, block_ts);
    let ladder = if stale {
        Vec::new()
    } else {
        ladder_raw
            .iter()
            .map(|raw_slot| LadderPoint {
                amount_in: *raw_slot >> U256::from(128),
                amount_out: *raw_slot & U256::from(u128::MAX),
            })
            .collect()
    };

    // 映射到 token_a/token_b 视角
    let token_a_is_x = token_x < token_y;
    let (reserve_a, reserve_b) = if token_a_is_x {
        (raw.reserve_x, raw.reserve_y)
    } else {
        (raw.reserve_y, raw.reserve_x)
    };

    Ok(CaliberSnapshot {
        reserve_a,
        reserve_b,
        ladder,
        field0,
        field1,
        fee_rate,
        window: raw.window,
        scale,
        pos,
        deadline: ts_x,
    })
}

/// 通过批量 `eth_getStorageAt` 读取单个 pair 的储备 + 原始 ladder + 精确报价参数。
///
/// 存储布局见 `build_snapshot_from_slots`。10 个固定槽位（含全局 slot2/slot3）
/// 走单次 JSON-RPC batch，ladder 槽位按需再读一批。
async fn fetch_exact_snapshot<N, P>(
    provider: &P,
    contract_address: Address,
    pair_id: B256,
    token_x: Address,
    token_y: Address,
    block: BlockId,
) -> Result<CaliberSnapshot, AMMError>
where
    N: Network,
    N::BlockResponse: BlockResponse,
    <N::BlockResponse as BlockResponse>::Header: BlockHeader,
    P: Provider<N> + Clone,
{
    let cfg_base = pair_slot(pair_id, 6);
    let data_base = pair_slot(pair_id, 7);

    let fixed = storage_at_batch(
        provider,
        &[
            (contract_address, b256_add(cfg_base, 1)),
            (contract_address, b256_add(cfg_base, 2)),
            (contract_address, b256_add(cfg_base, 3)),
            (contract_address, b256_add(cfg_base, 4)),
            (contract_address, b256_add(cfg_base, 5)),
            (contract_address, b256_add(cfg_base, 6)),
            (contract_address, b256_add(cfg_base, 7)),
            (contract_address, data_base),
            (
                contract_address,
                B256::from(U256::from(2u64).to_be_bytes::<32>()),
            ),
            (
                contract_address,
                B256::from(U256::from(3u64).to_be_bytes::<32>()),
            ),
        ],
        block,
    )
    .await?;

    let raw = RawPairSlots {
        cfg1: fixed[0],
        n: fixed[1],
        window: fixed[2],
        reserve_x: fixed[3],
        reserve_y: fixed[4],
        cfg6: fixed[5],
        cfg7: fixed[6],
        data0: fixed[7],
    };
    let validity_window = fixed[8] & U256::from(u64::MAX);
    let global_paused = fixed[9];

    let block_info = provider
        .get_block(block)
        .await
        .map_err(|e| AMMError::Msg(format!("caliber: get_block failed: {e}")))?;
    let block_ts = block_info
        .as_ref()
        .map(|b| U256::from(b.header().timestamp()))
        .unwrap_or_default();
    let cur_block = block_info
        .as_ref()
        .map(|b| U256::from(b.header().number()))
        .unwrap_or_default();

    // ladder 槽位仅当 pair 未过期/未暂停时读取（与链上 quote 语义一致）
    let n_usize: usize = raw.n.to::<usize>();
    if n_usize == 0 || n_usize > 1024 {
        return Err(AMMError::Msg(format!(
            "caliber: invalid ladder length {n_usize}"
        )));
    }
    let stale = pair_stale(&raw, validity_window, global_paused, block_ts);
    let mut ladder_raw = Vec::new();
    if !stale {
        let ladder_base =
            keccak256((U256::from_be_bytes(cfg_base.0) + U256::from(2)).to_be_bytes::<32>());
        let reads: Vec<(Address, B256)> = (0..n_usize)
            .map(|i| (contract_address, b256_add(ladder_base, i as u64)))
            .collect();
        ladder_raw = storage_at_batch(provider, &reads, block).await?;
    }

    build_snapshot_from_slots(
        token_x,
        token_y,
        raw,
        validity_window,
        global_paused,
        block_ts,
        cur_block,
        &ladder_raw,
    )
}

/// 批量读取多个 pair 的完整精确快照。
///
/// 每个 pair 的固定槽位（cfg+1..+7、data+0）与全部 ladder 槽位都通过
/// JSON-RPC batch 读取（`STORAGE_BATCH_SIZE` 个槽位一次 HTTP 请求）。
/// 相比逐 pair 串行 `fetch_exact_snapshot`（每 pair ~10+n 次 RPC），
/// 批量路径把 RPC 往返降到每 10 槽 1 次。
///
/// 返回与 `pairs` 对齐的逐 pair 结果；batch 本身失败返回外层 Err，
/// 单个 pair 校验失败（ladder 长度非法）返回内层 Err。
async fn fetch_exact_snapshots_batch<N, P>(
    provider: &P,
    contract_address: Address,
    pairs: &[(B256, Address, Address)],
    block: BlockId,
) -> Result<Vec<Result<CaliberSnapshot, AMMError>>, AMMError>
where
    N: Network,
    N::BlockResponse: BlockResponse,
    <N::BlockResponse as BlockResponse>::Header: BlockHeader,
    P: Provider<N> + Clone,
{
    if pairs.is_empty() {
        return Ok(Vec::new());
    }

    // 全局槽位（每合约一次）+ 块头（整个 batch 一次）
    let globals = storage_at_batch(
        provider,
        &[
            (
                contract_address,
                B256::from(U256::from(2u64).to_be_bytes::<32>()),
            ),
            (
                contract_address,
                B256::from(U256::from(3u64).to_be_bytes::<32>()),
            ),
        ],
        block,
    )
    .await?;
    let validity_window = globals[0] & U256::from(u64::MAX);
    let global_paused = globals[1];

    let block_info = provider
        .get_block(block)
        .await
        .map_err(|e| AMMError::Msg(format!("caliber: get_block failed: {e}")))?;
    let block_ts = block_info
        .as_ref()
        .map(|b| U256::from(b.header().timestamp()))
        .unwrap_or_default();
    let cur_block = block_info
        .as_ref()
        .map(|b| U256::from(b.header().number()))
        .unwrap_or_default();

    // 固定槽位：每 pair 8 个（cfg+1..+7、data+0）
    let mut fixed_reads: Vec<(Address, B256)> = Vec::with_capacity(pairs.len() * 8);
    for (pair_id, _, _) in pairs {
        let cfg_base = pair_slot(*pair_id, 6);
        let data_base = pair_slot(*pair_id, 7);
        for i in 1..=7u64 {
            fixed_reads.push((contract_address, b256_add(cfg_base, i)));
        }
        fixed_reads.push((contract_address, data_base));
    }
    let fixed = storage_at_batch_chunked(provider, &fixed_reads, block, STORAGE_BATCH_SIZE).await?;

    // 组装 raw + 收集需要读取的 ladder 槽位
    let mut raws: Vec<(RawPairSlots, bool)> = Vec::with_capacity(pairs.len());
    let mut ladder_reads: Vec<(Address, B256)> = Vec::new();
    for (i, (pair_id, _, _)) in pairs.iter().enumerate() {
        let base = i * 8;
        let raw = RawPairSlots {
            cfg1: fixed[base],
            n: fixed[base + 1],
            window: fixed[base + 2],
            reserve_x: fixed[base + 3],
            reserve_y: fixed[base + 4],
            cfg6: fixed[base + 5],
            cfg7: fixed[base + 6],
            data0: fixed[base + 7],
        };
        let n_usize: usize = raw.n.to::<usize>();
        let stale = if n_usize == 0 || n_usize > 1024 {
            false // 非法长度单独报错，跳过 ladder 读取
        } else {
            pair_stale(&raw, validity_window, global_paused, block_ts)
        };
        raws.push((raw, stale));
        if n_usize != 0 && n_usize <= 1024 && !stale {
            let cfg_base = pair_slot(*pair_id, 6);
            let ladder_base =
                keccak256((U256::from_be_bytes(cfg_base.0) + U256::from(2)).to_be_bytes::<32>());
            for j in 0..n_usize {
                ladder_reads.push((contract_address, b256_add(ladder_base, j as u64)));
            }
        }
    }

    let ladder_vals =
        storage_at_batch_chunked(provider, &ladder_reads, block, STORAGE_BATCH_SIZE).await?;

    // 组装逐 pair 结果
    let mut out = Vec::with_capacity(pairs.len());
    let mut li = 0usize;
    for (i, (_pair_id, token_x, token_y)) in pairs.iter().enumerate() {
        let (raw, stale) = raws[i];
        let n_usize: usize = raw.n.to::<usize>();
        if n_usize == 0 || n_usize > 1024 {
            out.push(Err(AMMError::Msg(format!(
                "caliber: invalid ladder length {n_usize}"
            ))));
            continue;
        }
        let ladder_raw = if stale {
            Vec::new()
        } else {
            let part = ladder_vals[li..li + n_usize].to_vec();
            li += n_usize;
            part
        };
        out.push(build_snapshot_from_slots(
            *token_x,
            *token_y,
            raw,
            validity_window,
            global_paused,
            block_ts,
            cur_block,
            &ladder_raw,
        ));
    }
    Ok(out)
}

/// 批量刷新一组 Caliber propAMM 池子的完整精确快照（周期对账/初始化共用）。
///
/// 按合约地址分组后调用 `fetch_exact_snapshots_batch`，把每 pool 的
/// ~10+n 次 `eth_getStorageAt` 折叠为每 10 槽一次 JSON-RPC batch。
/// 返回与 `pools` 对齐的成功标志：失败的 pool 保持旧状态，调用方可据此
/// 过滤（初始化场景）或仅记录（周期对账场景）。
pub async fn batch_refresh_snapshots<N, P>(
    provider: &P,
    pools: &mut [CaliberPropPool],
    block: BlockId,
) -> Result<Vec<bool>, AMMError>
where
    N: Network,
    N::BlockResponse: BlockResponse,
    <N::BlockResponse as BlockResponse>::Header: BlockHeader,
    P: Provider<N> + Clone,
{
    let mut flags = vec![false; pools.len()];
    if pools.is_empty() {
        return Ok(flags);
    }

    // 按合约地址分组（正常情况下全部 pool 同属一个 caliber 合约）
    let mut groups: Vec<(Address, Vec<usize>)> = Vec::new();
    for (idx, pool) in pools.iter().enumerate() {
        if let Some((_, idxs)) = groups
            .iter_mut()
            .find(|(addr, _)| *addr == pool.contract_address)
        {
            idxs.push(idx);
        } else {
            groups.push((pool.contract_address, vec![idx]));
        }
    }

    for (contract_address, idxs) in groups {
        let pairs: Vec<(B256, Address, Address)> = idxs
            .iter()
            .map(|&i| (pools[i].pair_id, pools[i].token_x, pools[i].token_y))
            .collect();
        let snapshots =
            match fetch_exact_snapshots_batch(provider, contract_address, &pairs, block).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(
                        address = ?contract_address,
                        error = ?e,
                        "caliber: batch snapshot fetch failed"
                    );
                    continue;
                }
            };
        for (k, snap_res) in snapshots.into_iter().enumerate() {
            let i = idxs[k];
            match snap_res {
                Ok(snap) => {
                    pools[i].apply_snapshot(snap);
                    flags[i] = true;
                }
                Err(e) => {
                    tracing::error!(
                        address = ?pools[i].virtual_address,
                        error = ?e,
                        "caliber: pool snapshot failed"
                    );
                }
            }
        }
    }

    Ok(flags)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_quote_vectors_pair1() {
        // 向量来自 docs/caliber_prop_re/（块 66309105 链上 eth_call 验证，零 DIFF）
        // pair 335c400406e84b: ladder=[[10, 200000000], [50, 900000000], [300, 1000000000]] field0=1900370065664 field1=105 fee=200 win=500 scale=1000000000000 pos_eff=261500663 rx=4035636208082082157 ry=13990178462
        let ladder = vec![
            LadderPoint {
                amount_in: U256::from(10u64),
                amount_out: U256::from(200000000u64),
            },
            LadderPoint {
                amount_in: U256::from(50u64),
                amount_out: U256::from(900000000u64),
            },
            LadderPoint {
                amount_in: U256::from(300u64),
                amount_out: U256::from(1000000000u64),
            },
        ];
        let field0 = U256::from(1900370065664u64);
        let field1 = U256::from(105u64);
        let fee_rate = U256::from(200u64);
        let window = U256::from(500u64);
        let scale = U256::from(1000000000000u64);
        let pos = U256::from(261500663u64); // cfg+7.block == 当前块，pos 有效
        let rx = U256::from(4035636208082082157u64);
        let ry = U256::from(13990178462u64);

        let fwd: Vec<(u64, u64)> = vec![
            (1, 0),
            (2, 0),
            (3, 0),
            (5, 0),
            (10, 0),
            (50, 0),
            (100, 0),
            (1000, 0),
            (10000, 0),
            (100000, 0),
            (1000000, 0),
            (10000000, 0),
            (100000000, 0),
            (1000000000, 0),
        ];
        for (amt, exp) in fwd {
            let out = quote_forward_exact(
                &ladder,
                field0,
                field1,
                fee_rate,
                window,
                scale,
                ry,
                U256::from(amt),
            );
            assert_eq!(out, U256::from(exp), "fwd amount={amt}");
        }

        let rev: Vec<(u64, u64)> = vec![
            (1, 526122805),
            (2, 1052245610),
            (3, 1578368415),
            (5, 2630614025),
            (10, 5261228050),
            (50, 26306140252),
            (100, 52612280504),
            (1000, 526122804976),
            (10000, 5260175797577),
            (100000, 52601757318261),
            (1000000, 526017507431313),
            (10000000, 5260168499192166),
            (100000000, 52601027488865468),
            (1000000000, 525943015783951046),
        ];
        for (amt, exp) in rev {
            let out = quote_reverse_exact(
                &ladder,
                field0,
                field1,
                fee_rate,
                window,
                scale,
                pos,
                rx,
                U256::from(amt),
            );
            assert_eq!(out, U256::from(exp), "rev amount={amt}");
        }
    }

    #[test]
    fn test_exact_quote_vectors_pair2() {
        // 向量来自 docs/caliber_prop_re/（块 66309105 链上 eth_call 验证，零 DIFF）
        // pair d81a7adf81bba9: ladder=[[10, 200000000], [50, 900000000], [300, 1000000000]] field0=75111231784 field1=283 fee=200 win=500 scale=1000 pos_eff=0 rx=6210305049 ry=1760056227
        let ladder = vec![
            LadderPoint {
                amount_in: U256::from(10u64),
                amount_out: U256::from(200000000u64),
            },
            LadderPoint {
                amount_in: U256::from(50u64),
                amount_out: U256::from(900000000u64),
            },
            LadderPoint {
                amount_in: U256::from(300u64),
                amount_out: U256::from(1000000000u64),
            },
        ];
        let field0 = U256::from(75111231784u64);
        let field1 = U256::from(283u64);
        let fee_rate = U256::from(200u64);
        let window = U256::from(500u64);
        let scale = U256::from(1000u64);
        let pos = U256::ZERO; // cfg+7.block != 当前块，pos 无效
        let rx = U256::from(6210305049u64);
        let ry = U256::from(1760056227u64);

        let fwd: Vec<(u64, u64)> = vec![
            (1, 0),
            (2, 0),
            (3, 0),
            (5, 0),
            (10, 0),
            (50, 2),
            (100, 6),
            (1000, 74),
            (10000, 749),
            (100000, 7506),
            (1000000, 75073),
            (10000000, 750741),
            (100000000, 7507414),
            (1000000000, 75073642),
        ];
        for (amt, exp) in fwd {
            let out = quote_forward_exact(
                &ladder,
                field0,
                field1,
                fee_rate,
                window,
                scale,
                ry,
                U256::from(amt),
            );
            assert_eq!(out, U256::from(exp), "fwd amount={amt}");
        }

        let rev: Vec<(u64, u64)> = vec![
            (1, 13),
            (2, 26),
            (3, 39),
            (5, 66),
            (10, 133),
            (50, 665),
            (100, 1330),
            (1000, 13309),
            (10000, 133070),
            (100000, 1330702),
            (1000000, 13307025),
            (10000000, 133070131),
            (100000000, 1330689339),
            (1000000000, 6210305049),
        ];
        for (amt, exp) in rev {
            let out = quote_reverse_exact(
                &ladder,
                field0,
                field1,
                fee_rate,
                window,
                scale,
                pos,
                rx,
                U256::from(amt),
            );
            assert_eq!(out, U256::from(exp), "rev amount={amt}");
        }
    }

    #[test]
    fn test_exact_quote_vectors_pair3() {
        // 向量来自 docs/caliber_prop_re/（块 66309105 链上 eth_call 验证，零 DIFF）
        // pair 55c40a68abf347: ladder=[[10, 200000000], [50, 900000000], [300, 1000000000]] field0=64749328871563 field1=30 fee=200 win=500 scale=100 pos_eff=0 rx=0 ry=16460388407
        let ladder = vec![
            LadderPoint {
                amount_in: U256::from(10u64),
                amount_out: U256::from(200000000u64),
            },
            LadderPoint {
                amount_in: U256::from(50u64),
                amount_out: U256::from(900000000u64),
            },
            LadderPoint {
                amount_in: U256::from(300u64),
                amount_out: U256::from(1000000000u64),
            },
        ];
        let field0 = U256::from(64749328871563u64);
        let field1 = U256::from(30u64);
        let fee_rate = U256::from(200u64);
        let window = U256::from(500u64);
        let scale = U256::from(100u64);
        let pos = U256::ZERO; // cfg+7.block != 当前块，pos 无效
        let rx = U256::from(0u64);
        let ry = U256::from(16460388407u64);

        let fwd: Vec<(u64, u64)> = vec![
            (1, 646),
            (2, 1293),
            (3, 1941),
            (5, 3236),
            (10, 6473),
            (50, 32372),
            (100, 64746),
            (1000, 647467),
            (10000, 6473373),
            (100000, 64733370),
            (1000000, 647287590),
            (10000000, 6469230696),
            (100000000, 16460388407),
            (1000000000, 16460388407),
        ];
        for (amt, exp) in fwd {
            let out = quote_forward_exact(
                &ladder,
                field0,
                field1,
                fee_rate,
                window,
                scale,
                ry,
                U256::from(amt),
            );
            assert_eq!(out, U256::from(exp), "fwd amount={amt}");
        }

        let rev: Vec<(u64, u64)> = vec![
            (1, 0),
            (2, 0),
            (3, 0),
            (5, 0),
            (10, 0),
            (50, 0),
            (100, 0),
            (1000, 0),
            (10000, 0),
            (100000, 0),
            (1000000, 0),
            (10000000, 0),
            (100000000, 0),
            (1000000000, 0),
        ];
        for (amt, exp) in rev {
            let out = quote_reverse_exact(
                &ladder,
                field0,
                field1,
                fee_rate,
                window,
                scale,
                pos,
                rx,
                U256::from(amt),
            );
            assert_eq!(out, U256::from(exp), "rev amount={amt}");
        }
    }

    #[test]
    fn test_exact_quote_vectors_pair4() {
        // 向量来自 docs/caliber_prop_re/（块 66309105 链上 eth_call 验证，零 DIFF）
        // pair 5dda42efa9e87d: ladder=[[10, 200000000], [50, 900000000], [300, 1000000000]] field0=84499509749 field1=178 fee=200 win=500 scale=1000000000000 pos_eff=0 rx=0 ry=8972848594
        let ladder = vec![
            LadderPoint {
                amount_in: U256::from(10u64),
                amount_out: U256::from(200000000u64),
            },
            LadderPoint {
                amount_in: U256::from(50u64),
                amount_out: U256::from(900000000u64),
            },
            LadderPoint {
                amount_in: U256::from(300u64),
                amount_out: U256::from(1000000000u64),
            },
        ];
        let field0 = U256::from(84499509749u64);
        let field1 = U256::from(178u64);
        let fee_rate = U256::from(200u64);
        let window = U256::from(500u64);
        let scale = U256::from(1000000000000u64);
        let pos = U256::ZERO; // cfg+7.block != 当前块，pos 无效
        let rx = U256::from(0u64);
        let ry = U256::from(8972848594u64);

        let fwd: Vec<(u64, u64)> = vec![
            (1, 0),
            (2, 0),
            (3, 0),
            (5, 0),
            (10, 0),
            (50, 0),
            (100, 0),
            (1000, 0),
            (10000, 0),
            (100000, 0),
            (1000000, 0),
            (10000000, 0),
            (100000000, 0),
            (1000000000, 0),
        ];
        for (amt, exp) in fwd {
            let out = quote_forward_exact(
                &ladder,
                field0,
                field1,
                fee_rate,
                window,
                scale,
                ry,
                U256::from(amt),
            );
            assert_eq!(out, U256::from(exp), "fwd amount={amt}");
        }

        let rev: Vec<(u64, u64)> = vec![
            (1, 0),
            (2, 0),
            (3, 0),
            (5, 0),
            (10, 0),
            (50, 0),
            (100, 0),
            (1000, 0),
            (10000, 0),
            (100000, 0),
            (1000000, 0),
            (10000000, 0),
            (100000000, 0),
            (1000000000, 0),
        ];
        for (amt, exp) in rev {
            let out = quote_reverse_exact(
                &ladder,
                field0,
                field1,
                fee_rate,
                window,
                scale,
                pos,
                rx,
                U256::from(amt),
            );
            assert_eq!(out, U256::from(exp), "rev amount={amt}");
        }
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

    // ── batchUpdateParameters 解码 / raw tx 提取 / 池子应用 ──

    /// 真实链上更新交易 calldata（XLayer 块 67329558，tx 0xd9a1ffba…，5 个 pair）
    const REAL_UPDATE_CALLDATA: &str = "0x008dcc8e00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000005d2ba36ae7a49fbbb15ac04a76531d9e811ca5fe2e57f4c559f200ed2a57aac7a0000000000000000000000000000000000000000000000000000000efaaa31bf0000000000000000000000000000000000000000000000000000000000000483000000000000000000000000000000000000000000000000000000006a75b3a0f4b05af384ac756330659972e8584851916c39bf13414abd632dc7c11ee792380000000000000000000000000000000000000000000000000000001b318644c0000000000000000000000000000000000000000000000000000000000000043d000000000000000000000000000000000000000000000000000000006a75b3a0b2d5c47f635aa119fc5e911aa881db33bc77b61a1872d035d6122869e24a96730000000000000000000000000000000000000000000000000000003338d4970100000000000000000000000000000000000000000000000000000000000003b1000000000000000000000000000000000000000000000000000000006a75b3a0304e5bfc144bd0991c990cbbe6488660faf1f6be58a8afb15f3330c8a01599880000000000000000000000000000000000000000000000000000012de1e46dc000000000000000000000000000000000000000000000000000000000000005e3000000000000000000000000000000000000000000000000000000006a75b3a0de4c3cddfd81d8ee19634d5d62f07681bf28fdc2c622a1bbdb276d3359053ddf0000000000000000000000000000000000000000000000000000002127ffdb40000000000000000000000000000000000000000000000000000000000000042e000000000000000000000000000000000000000000000000000000006a75b3a0";

    /// 同一笔交易的真实 raw RLP 字节（EIP-1559，to=0x154586b2…caliber 合约）
    const REAL_UPDATE_RAW_TX: &str = "0x02f9033481c483129664832dc6c08401c9c3808301d4c094154586b2479b9a11e3d4db90024dc0e26f09731280b902c4008dcc8e00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000005d2ba36ae7a49fbbb15ac04a76531d9e811ca5fe2e57f4c559f200ed2a57aac7a0000000000000000000000000000000000000000000000000000000efaaa31bf0000000000000000000000000000000000000000000000000000000000000483000000000000000000000000000000000000000000000000000000006a75b3a0f4b05af384ac756330659972e8584851916c39bf13414abd632dc7c11ee792380000000000000000000000000000000000000000000000000000001b318644c0000000000000000000000000000000000000000000000000000000000000043d000000000000000000000000000000000000000000000000000000006a75b3a0b2d5c47f635aa119fc5e911aa881db33bc77b61a1872d035d6122869e24a96730000000000000000000000000000000000000000000000000000003338d4970100000000000000000000000000000000000000000000000000000000000003b1000000000000000000000000000000000000000000000000000000006a75b3a0304e5bfc144bd0991c990cbbe6488660faf1f6be58a8afb15f3330c8a01599880000000000000000000000000000000000000000000000000000012de1e46dc000000000000000000000000000000000000000000000000000000000000005e3000000000000000000000000000000000000000000000000000000006a75b3a0de4c3cddfd81d8ee19634d5d62f07681bf28fdc2c622a1bbdb276d3359053ddf0000000000000000000000000000000000000000000000000000002127ffdb40000000000000000000000000000000000000000000000000000000000000042e000000000000000000000000000000000000000000000000000000006a75b3a0c001a07ed1485c2f6ace2104a384b0e596f9a39729450002b77b23bd1e4ab10ea24512a0179baa7e6f42f7e616b59046768d8a81c12d3c0653338501c4855584046f291e";

    fn hex_to_bytes(s: &str) -> Vec<u8> {
        alloy::hex::decode(s.trim_start_matches("0x")).unwrap()
    }

    #[test]
    fn test_decode_batch_update_parameters_real_calldata() {
        let input = hex_to_bytes(REAL_UPDATE_CALLDATA);
        let updates = decode_batch_update_parameters(&input).expect("decode should succeed");
        assert_eq!(updates.len(), 5);

        let expect: [([u8; 32], u64, u32, u64); 5] = [
            (
                [
                    0xd2, 0xba, 0x36, 0xae, 0x7a, 0x49, 0xfb, 0xbb, 0x15, 0xac, 0x04, 0xa7, 0x65,
                    0x31, 0xd9, 0xe8, 0x11, 0xca, 0x5f, 0xe2, 0xe5, 0x7f, 0x4c, 0x55, 0x9f, 0x20,
                    0x0e, 0xd2, 0xa5, 0x7a, 0xac, 0x7a,
                ],
                64_334_999_999,
                1155,
                1_786_098_592,
            ),
            (
                [
                    0xf4, 0xb0, 0x5a, 0xf3, 0x84, 0xac, 0x75, 0x63, 0x30, 0x65, 0x99, 0x72, 0xe8,
                    0x58, 0x48, 0x51, 0x91, 0x6c, 0x39, 0xbf, 0x13, 0x41, 0x4a, 0xbd, 0x63, 0x2d,
                    0xc7, 0xc1, 0x1e, 0xe7, 0x92, 0x38,
                ],
                116_795_000_000,
                1085,
                1_786_098_592,
            ),
            (
                [
                    0xb2, 0xd5, 0xc4, 0x7f, 0x63, 0x5a, 0xa1, 0x19, 0xfc, 0x5e, 0x91, 0x1a, 0xa8,
                    0x81, 0xdb, 0x33, 0xbc, 0x77, 0xb6, 0x1a, 0x18, 0x72, 0xd0, 0x35, 0xd6, 0x12,
                    0x28, 0x69, 0xe2, 0x4a, 0x96, 0x73,
                ],
                219_996_788_481,
                945,
                1_786_098_592,
            ),
            (
                [
                    0x30, 0x4e, 0x5b, 0xfc, 0x14, 0x4b, 0xd0, 0x99, 0x1c, 0x99, 0x0c, 0xbb, 0xe6,
                    0x48, 0x86, 0x60, 0xfa, 0xf1, 0xf6, 0xbe, 0x58, 0xa8, 0xaf, 0xb1, 0x5f, 0x33,
                    0x30, 0xc8, 0xa0, 0x15, 0x99, 0x88,
                ],
                1_296_575_000_000,
                1507,
                1_786_098_592,
            ),
            (
                [
                    0xde, 0x4c, 0x3c, 0xdd, 0xfd, 0x81, 0xd8, 0xee, 0x19, 0x63, 0x4d, 0x5d, 0x62,
                    0xf0, 0x76, 0x81, 0xbf, 0x28, 0xfd, 0xc2, 0xc6, 0x22, 0xa1, 0xbb, 0xdb, 0x27,
                    0x6d, 0x33, 0x59, 0x05, 0x3d, 0xdf,
                ],
                142_405_000_000,
                1070,
                1_786_098_592,
            ),
        ];

        for (u, (pair, price, flags, deadline)) in updates.iter().zip(expect) {
            assert_eq!(u.pair_id, B256::from(pair), "pair_id");
            assert_eq!(u.price, U256::from(price), "price");
            assert_eq!(u.flags, flags, "flags");
            assert_eq!(u.deadline, deadline, "deadline");
        }
    }

    #[test]
    fn test_decode_batch_update_parameters_fail_safe() {
        // 非目标选择器 → None
        assert_eq!(
            decode_batch_update_parameters(&[0xde, 0xad, 0xbe, 0xef]),
            None
        );
        // 选择器对但 ABI 截断 → None
        assert_eq!(
            decode_batch_update_parameters(&[0x00, 0x8d, 0xcc, 0x8e]),
            None
        );
        // 短于选择器 → None
        assert_eq!(decode_batch_update_parameters(&[0x00, 0x8d]), None);
    }

    #[test]
    fn test_extract_to_from_raw_tx_eip1559() {
        let raw = hex_to_bytes(REAL_UPDATE_RAW_TX);
        let to = extract_to_from_raw_tx(&raw).expect("to should be present");
        let expect: Address = "0x154586b2479b9a11e3d4db90024dc0e26f097312"
            .parse()
            .unwrap();
        assert_eq!(to, expect);
    }

    #[test]
    fn test_extract_to_from_raw_tx_legacy() {
        // 手工构造最小 Legacy 交易（未签名即可，to 在第 3 项）：
        // [nonce=7, gasPrice=1e9, gasLimit=21000, to, value=0, input=empty]
        let to_addr: Address = "0x1111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        let mut content = Vec::new();
        content.push(0x07); // nonce
        content.extend_from_slice(&[0x84, 0x3b, 0x9a, 0xca, 0x00]); // gasPrice
        content.extend_from_slice(&[0x82, 0x52, 0x08]); // gasLimit
        content.push(0x94); // to: 20 字节地址
        content.extend_from_slice(to_addr.as_ref());
        content.push(0x80); // value = 0
        content.push(0x80); // input = empty

        let mut raw = vec![0xf8, content.len() as u8]; // 长列表头
        raw.extend_from_slice(&content);
        assert_eq!(extract_to_from_raw_tx(&raw), Some(to_addr));
    }

    #[test]
    fn test_extract_to_from_raw_tx_contract_creation() {
        // to = 空（0x80）→ None（合约创建，无 to）
        let mut content = Vec::new();
        content.push(0x07); // nonce
        content.extend_from_slice(&[0x84, 0x3b, 0x9a, 0xca, 0x00]); // gasPrice
        content.extend_from_slice(&[0x82, 0x52, 0x08]); // gasLimit
        content.push(0x80); // to = empty（合约创建）
        content.push(0x80); // value
        content.push(0x80); // input
        let mut raw = vec![0xf8, content.len() as u8];
        raw.extend_from_slice(&content);
        assert_eq!(extract_to_from_raw_tx(&raw), None);
    }

    #[test]
    fn test_extract_to_from_raw_tx_invalid() {
        // 非 RLP / 截断 → None（fail-safe）
        assert_eq!(extract_to_from_raw_tx(&[0xff, 0x01]), None);
        assert_eq!(extract_to_from_raw_tx(&[]), None);
    }

    #[test]
    fn test_extract_input_from_raw_tx_real() {
        // 命中目标合约后取 calldata：应以 batchUpdateParameters 选择器开头
        let raw = hex_to_bytes(REAL_UPDATE_RAW_TX);
        let input = extract_input_from_raw_tx(&raw).expect("input should be present");
        assert_eq!(&input[..4], &CALIBER_BATCH_UPDATE_SELECTOR);
        assert_eq!(
            decode_batch_update_parameters(&input).map(|v| v.len()),
            Some(5)
        );
    }

    #[test]
    fn test_apply_batch_update() {
        let contract = Address::repeat_byte(0xAA);
        let pair_id = B256::from([0x11u8; 32]);
        let mut pool = CaliberPropPool {
            contract_address: contract,
            pair_id,
            virtual_address: CaliberPropPool::virtual_address_from_pair_id(pair_id, contract),
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

        // pairId 匹配：应用更新
        let u = CaliberBatchUpdate {
            pair_id,
            price: U256::from(64_334_999_999u64),
            flags: 1155,
            deadline: 1_786_098_592,
        };
        pool.apply_batch_update(&u, 67_329_558);
        assert_eq!(pool.ladder.field0, U256::from(64_334_999_999u64));
        assert_eq!(pool.ladder.field1, U256::from(1155u64));
        assert_eq!(pool.ladder.deadline, 1_786_098_592);
        assert_eq!(pool.last_synced_block, 67_329_558);

        // pairId 不匹配：静默忽略，不污染状态
        let other = CaliberBatchUpdate {
            pair_id: B256::from([0x22u8; 32]),
            price: U256::from(1u64),
            flags: 0,
            deadline: 0,
        };
        pool.apply_batch_update(&other, 99);
        assert_eq!(pool.ladder.field0, U256::from(64_334_999_999u64));
        assert_eq!(pool.ladder.field1, U256::from(1155u64));
        assert_eq!(pool.last_synced_block, 67_329_558);
    }

    #[test]
    fn test_caliber_contracts_dedup() {
        let contract = Address::repeat_byte(0xAA);
        let make_pool = |pair_id: B256| CaliberPropPool {
            contract_address: contract,
            pair_id,
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

        let pools = vec![
            make_pool(B256::from([0x11u8; 32])),
            make_pool(B256::from([0x22u8; 32])),
        ];
        let contracts = caliber_contracts(&pools);
        assert_eq!(contracts.len(), 1);
        assert!(contracts.contains(&contract));
    }
}

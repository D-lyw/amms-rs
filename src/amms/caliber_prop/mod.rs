//! # Caliber propAMM (Makina Protocol)
//!
//! 集成 Makina 协议的 Caliber propAMM 做市商定价 AMM。
//!
//! ## 架构
//! - **Ladder 定价模型**: 做市商通过链下引擎上传分段线性定价阶梯，链上合约在每次
//!   swap 后 emit `Swap` 事件（2026-08-11 链上实测确认，见
//!   `docs/2026-08-11_caliber_swap_consumption_sync_report.md`）。
//! - **同步策略**: `sync_events()` 返回空——swap 日志不进入通用日志管道，而是由
//!   XLayer flashblocks 提取通道（`caliber_contracts` 地址预筛）单独解析，避免
//!   与 `apply_logs_for_block_timed` 双重应用。XLayer 实时报价更新由 flashblocks
//!   原始交易流驱动（`batchUpdateParameters` calldata → `apply_batch_update`，
//!   详见 `docs/caliber_prop_realtime_sync_design.md`）；周期任务降频为对账/兜底
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
    primitives::{address, b256, keccak256, Address, B256, U256},
    providers::{DynProvider, Provider, ProviderBuilder},
    rpc::types::Log,
    sol,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
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

/// `Swap(bytes32,address,address,address,uint256,uint256,uint256)` 事件签名。
///
/// XLayer Caliber 合约在每次 swap 后 emit 该事件（topic0），
/// topics = `[sig, pairId, caller]`，data =
/// `[tokenIn, tokenOut, amountIn, amountOut, flags]`（各 32B 左对齐）。
/// 与 `batchUpdateParameters`（0 日志）不同，swap 有日志可驱动实时同步：
/// 消费追踪（ladder consumption）+ 储备/pos 更新（2026-08-11 新增）。
pub const CALIBER_SWAP_EVENT: B256 =
    b256!("36d90ab6736dbd42ac28b968350d068640e9aea3f7b807679fe64d2a50dcbb03");

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

/// 链上 `Swap` 事件（XLayer flashblocks 日志驱动实时消费同步）。
///
/// 从 receipt 日志中解码：`topics = [sig, pairId, caller]`，
/// `data = [tokenIn, tokenOut, amountIn, amountOut, flags]`。
/// 提取侧已按 `receipt.status == 0x1` 过滤未确认/回滚交易（P0 纪律，
/// 与 caliber 更新路径一致）；块内按 `tx_index` 排序应用。
///
/// 注意：事件 `amountIn`/`amountOut` 是 swap 调用方传入/期望的参数值，
/// 对走路由器聚合的复杂交易可能与实际转账金额有微小出入（取证块
/// 67650064 tx#22 差 3072 U），由周期对账快照兜底纠正；直接 swap 调用
/// 两者一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaliberSwapEvent {
    /// 被调用的 caliber 合约地址（路由层推导 virtual_address 用）
    pub contract: Address,
    /// 块内全局交易索引（EVM 语义：块内按序应用）
    pub tx_index: u64,
    /// 目标 pair 的原始 pairId
    pub pair_id: B256,
    /// 输入 token（合约 token 序）
    pub token_in: Address,
    /// 输出 token（合约 token 序）
    pub token_out: Address,
    /// 输入量（原始量纲）
    pub amount_in: U256,
    /// 输出量（原始量纲）
    pub amount_out: U256,
}

/// 从 caliper `Swap` 日志（topics + data）解码事件。
///
/// 结构不符 / 解码失败返回 `None`（fail-safe：调用方静默跳过）。
/// data 为 5 个 32B 字：`[tokenIn, tokenOut, amountIn, amountOut, flags]`。
pub fn decode_caliber_swap_log(topics: &[B256], data: &[u8]) -> Option<CaliberSwapEvent> {
    if topics.len() < 2 || topics[0] != CALIBER_SWAP_EVENT || data.len() < 160 {
        return None;
    }
    let word = |start: usize| U256::from_be_slice(&data[start..start + 32]);
    let addr = |start: usize| Address::from_slice(&data[start + 12..start + 32]);
    Some(CaliberSwapEvent {
        contract: Address::ZERO, // 由调用方填充（日志地址）
        tx_index: 0,             // 由调用方填充（块内索引）
        pair_id: topics[1],
        token_in: addr(0),
        token_out: addr(32),
        amount_in: word(64),
        amount_out: word(96),
    })
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

    /// 根据 ladder 的边际价格刷新缓存现货价（与链上 quote 语义一致）
    fn refresh_prices(&mut self) {
        // Empty ladder means the maker currently provides no usable quote.
        // Clear cached spot prices so upstream price filters won't reuse stale data.
        self.price_a_in_b = 0.0;
        self.price_b_in_a = 0.0;

        if let Some(first) = self.ladder.ladder_a_to_b.first() {
            // 现货价 = 链上边际价格（docs/caliber_prop_internal.md §5）：
            //   d(out)/d(in) = field0 * (1e6 - (x0 + field1)) / (1e9 * scale * 1e6)
            // ladder 存的是 token_x → token_y 方向，归一化后人读价格为
            // "1 token_x = X token_y"（X = field0 * (1e6 - (x0 + field1)) / 1e15，
            // scale 与 10^(dec_x-dec_y) 相消）。
            //
            // 注意方向：只有 token_x == token_a 时该值才是 price_a_in_b（token_a 以
            // token_b 计价）；当 token_x == token_b（如 USDT0/wNVDAx pair，x=wNVDAx）时
            // 它其实是 price_b_in_a，必须取倒数。旧实现直接赋值，导致该 pair 的
            // spot_price(USDT0, wNVDAx) 返回 a/b（225.06）而非 b/a（0.004443），与
            // V3 同调用方向相反，TwoCycle 预筛选错方向、漏掉 V3→Caliber 套利。
            // 注意：不能用首段斜率 y0/x0（旧实现），与真实边际价可差多个数量级。
            let a0 = MILLION - (first.amount_in + self.ladder.field1);
            let price_xy = u256_to_f64(&(self.ladder.field0 * a0)) / 1e15;
            if price_xy > 0.0 {
                // price_a_in_b = price of a in terms of b = b/a（字段文档语义），
                // price_b_in_a = price of b in terms of a = a/b。
                let (b_per_a, a_per_b) = if self.token_x == self.token_a.address {
                    (price_xy, 1.0 / price_xy)
                } else {
                    (1.0 / price_xy, price_xy)
                };
                self.price_a_in_b = b_per_a;
                self.price_b_in_a = a_per_b;
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
    /// 只更新报价参数（`field0=price`、`field1=flags`、`deadline`）与
    /// 同步区块号；ladder 曲线、储备、pos、validity_window、暂停态不随更新
    /// 变化（由低频对账任务覆盖做市商充值等低频变动）。更新写入后 deadline
    /// 刷新，若 pair 此前因过期被判定不可报价（ladder 保留自最近一次有效
    /// 快照），将立即恢复可报价状态。`pair_id` 不匹配时静默忽略（fail-safe，
    /// 防跨 pair 污染）。
    ///
    /// TODO(后续): 暂停类交易（`setPricingMode`/`setLocked`/`setWhitelistOnly`）
    /// 未纳入实时解析，暂停态只能由周期对账刷新（≤45s 滞后）；后续应在
    /// flashblocks 实时流中解析这些交易并实时更新 `ladder.paused`。
    pub fn apply_batch_update(&mut self, u: &CaliberBatchUpdate, block_number: u64) {
        if u.pair_id != self.pair_id {
            return;
        }
        self.ladder.field0 = u.price;
        self.ladder.field1 = U256::from(u.flags);
        self.ladder.deadline = u.deadline;
        self.last_synced_block = block_number;
        // field0/field1 是现货边际价公式的输入，实时更新后必须立即刷新
        // spot 缓存（否则下游 price filter 会用旧价格直到下一轮对账）。
        self.refresh_prices();
    }

    /// 应用链上 `Swap` 事件（XLayer flashblocks 日志驱动实时消费同步）。
    ///
    /// 只更新 swap 影响的状态：储备（`reserve_a`/`reserve_b`）与当前位置
    /// （`pos_forward` = cfg+7 low96 / `pos_reverse` = cfg+7 mid96）。
    /// 与链上行为一致（2026-08-11 块 67650064 取证）：每次 swap 只写当前
    /// 方向的 pos 字段并归零另一方向（同一区块连续同向 swap 为累计，
    /// 方向切换后旧方向 pos 失效）。
    ///
    /// `consumed_*` 是**纯模拟**状态（`simulate_swap_mut` 专属，快照刷新时
    /// 清零），不随真实事件更新——实时事件已直接写入 pos 字段，若再累加
    /// consumed 会在后续模拟中双重计数。
    ///
    /// `pair_id` 不匹配时静默忽略（fail-safe，防跨 pair 污染）；
    /// 提取侧已按 `receipt.status == 0x1` 过滤回滚/未确认交易。
    pub fn apply_chain_swap(&mut self, swap: &CaliberSwapEvent, block_number: u64) {
        if swap.pair_id != self.pair_id {
            return;
        }
        let forward = swap.token_in == self.token_x;
        // 输入侧储备增量 = "ladder 一致输入"（2026-08-11 块 67650064 取证）：
        // 输出未被限制（amount_out == quote(amount_in)）时链上按事件 amountIn
        // 全额入账；输出被限制（router minOut / 储备封顶，amount_out <
        // quote(amount_in)）时链上只按"产生该输出的 ladder 输入"入账，超出
        // 部分停留在合约余额、不记入 pair 储备（tx#22：事件 amountIn=526e15，
        // 链上仅入账 87.35e15）。用 `ladder_input_for_output` 复刻。
        let input_consumed = self.ladder_input_for_output(swap, forward);
        // 储备按 token_a/token_b 映射（虚拟池子视角，与 simulate_swap_mut
        // 同一约定）：token_in == token_a → reserve_a += input_consumed /
        // reserve_b -= out；token_in == token_b → 反之。输出侧始终按事件
        // amountOut 全额扣减（链上 cfg+4/5 与事件输出逐位一致）。
        // 方向（forward/reverse）只决定 pos 字段。
        match self.get_token_index(swap.token_in) {
            0 => {
                self.reserve_a += input_consumed;
                self.reserve_b = self.reserve_b.saturating_sub(swap.amount_out);
            }
            1 => {
                self.reserve_b += input_consumed;
                self.reserve_a = self.reserve_a.saturating_sub(swap.amount_out);
            }
            _ => return, // token 不在 pair 内 → 静默忽略（fail-safe）
        }
        if forward {
            // 正向 swap：low96 累计 +out，mid96 归零（链上方向切换语义）
            self.ladder.pos_forward += swap.amount_out;
            self.ladder.pos_reverse = U256::ZERO;
        } else {
            // 反向 swap（token_y → token_x）：链上 mid96 累计"扣费后的 y 输入"
            // amountIn_y - floor(amountIn_y * fee / 1e6)（2026-08-11 取证：
            // 块 67650064 附近两笔反向 swap 的 mid96 与事件输入逐位一致，
            // 如 82,580,656 in → 82,564,140 = in - floor(in*200/1e6)），
            // 与 quote_reverse_exact 的 pos（y 单位）语义一致。
            let fee = evm_div(swap.amount_in * self.ladder.fee_rate, MILLION);
            self.ladder.pos_reverse += swap.amount_in - fee;
            self.ladder.pos_forward = U256::ZERO;
        }
        self.last_synced_block = block_number;
    }

    /// 链上 swap 对"输入侧储备"的入账增量（2026-08-11 块 67650064 取证）。
    ///
    /// 链上行为：`amount_out == quote(amount_in)`（输出未被调用方限制）时按
    /// 事件 amountIn 全额入账；`amount_out < quote(amount_in)`（如 router
    /// minOut / 储备封顶）时只按"产生该输出的 ladder 输入"入账——即
    /// `{x ∈ [0, amountIn] : quote(x) == amountOut}` 平台区的上沿，超出部分
    /// 停留在合约余额、不记入 pair 储备（取证 tx#22：事件 amountIn=526e15、
    /// amountOut=19,156,271，链上 cfg+4 仅入账 87,349,782,419,593,420）。
    ///
    /// 本地用二分求"最小的 x 使 quote(x) > amountOut 再减 1"复刻平台区上沿：
    /// - 未受限（quote(amountIn) == amountOut）→ 直接返回 amountIn（链上
    ///   逐位一致，避免平台区下探 1 wei）；
    /// - 受限 → 平台区上沿，与链上偏差 < 2.2e9 wei（远低于 dust，对账兜底）；
    /// - ladder 为空 / quote(amountIn) < amountOut（异常）→ 回退 amountIn。
    ///
    /// 与 `quote_forward_pos_exact` / `quote_reverse_exact` 同语义（EVM
    /// uint256 截断），pos 取应用本笔 swap 前的当前位置。
    fn ladder_input_for_output(&self, swap: &CaliberSwapEvent, forward: bool) -> U256 {
        if swap.amount_out.is_zero() {
            return U256::ZERO;
        }
        let (ladder, pos) = if forward {
            (&self.ladder.ladder_a_to_b, self.ladder.pos_forward)
        } else {
            (&self.ladder.ladder_a_to_b, self.ladder.pos_reverse)
        };
        if ladder.is_empty() {
            return swap.amount_in;
        }
        let cap = U256::MAX;
        let quote = |x: U256| {
            if forward {
                quote_forward_pos_exact(
                    ladder,
                    self.ladder.field0,
                    self.ladder.field1,
                    self.ladder.fee_rate,
                    self.ladder.window,
                    self.ladder.scale,
                    pos,
                    cap,
                    x,
                )
            } else {
                quote_reverse_exact(
                    ladder,
                    self.ladder.field0,
                    self.ladder.field1,
                    self.ladder.fee_rate,
                    self.ladder.window,
                    self.ladder.scale,
                    pos,
                    cap,
                    x,
                )
            }
        };
        let q_in = quote(swap.amount_in);
        if q_in == swap.amount_out {
            return swap.amount_in;
        }
        if q_in < swap.amount_out {
            // 异常（空 ladder 已在上方处理，这里覆盖 quote 上限等异常）：保守全额入账
            return swap.amount_in;
        }
        // q(amount_in) > amount_out：求平台区上沿 = min{x | quote(x) > amount_out} - 1
        let mut lo = U256::ZERO;
        let mut hi = swap.amount_in;
        while lo < hi {
            let mid = lo + (hi - lo) / U256::from(2u64);
            if quote(mid) > swap.amount_out {
                hi = mid;
            } else {
                lo = mid + U256::from(1u64);
            }
        }
        lo.saturating_sub(U256::from(1u64))
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

/// 当前 Unix 时间戳（秒）。链上过期判定使用 `block.timestamp`，本地报价路径
/// 没有块上下文，用墙钟近似（XLayer 秒级出块，误差可忽略；如需更保守可在
/// 上游对判定结果预留安全缓冲）。
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

/// 精确复刻链上 `quote(token_x -> token_y)` 正向报价（有状态，pos 版本）
///
/// 链上正向报价同样维护"当前位置" `pos`（= `cfg+7` 的 **low96**，即该 pair
/// 已从 token_x 侧被兑换掉的累计 y 量，见 `docs/caliber_prop_internal.md` §4）：
/// - 跳过 `pos` 已完全消费的段；`pos` 所在段按段内剩余量 `R = y_i - offset`
///   报价，并用当前位置插值的斜率 `a_eff`（EVM 截断）
/// - 逐段 `P = 1e6 * 2 * R / (a_eff + a_next)`、`th = ceil(P * 1e9 * scale / field0)`；
///   `xp >= th` 时整段消费 `R`，否则段内
///   `part = r2 * 2 * R * a_eff / (1e6 * 2 * R + r2 * delta_eff)`，
///   `r2 = field0 * xp / (1e9 * scale)`、`delta_eff = a_eff - a_next`
/// - 尾段按 `a_last` 直线外推；输出上限 `min(out, reserve_y)`
///
/// **关键差异（2026-08-11 EVM trace + 48 数据点验证）**：正向的段内插值
/// `a_eff` 与反向不同——正向 `a_i` 随段递减（`a_i = 1e6 - (x_i + field1)`），
/// 合约用**正数 floor 减法** `a_eff = a_i - trunc((a_i - a_next) * offset / y_i)`，
/// 而非反向的负数截断形式 `a_i + trunc((a_next - a_i) * offset / y_i)`；两者在
/// `(a_i - a_next) * offset` 不能整除 `y_i` 时相差 1（`a_eff` 正向偏大 1），
/// 段内报价因此系统性偏大（实测 -17 ~ -308 输出偏差，事故块取证定位）。
/// 若误用反向形式，`delta_eff` 会少 1，段内输出整体偏小。
///
/// `pos` 为 0 时退化为 `quote_forward_exact`（链上在 pos 过期/无效时的行为）。
pub fn quote_forward_pos_exact(
    ladder: &[LadderPoint],
    field0: U256,
    field1: U256,
    fee_rate: U256,
    window: U256,
    scale: U256,
    pos: U256,
    reserve_y: U256,
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
        let x_next = if i + 1 < n {
            ladder[i + 1].amount_in
        } else {
            x_i + window
        };
        let a_i = MILLION - (x_i + field1);
        let a_next = MILLION - (x_next + field1);
        // 正向用正数 floor 减法（见函数文档的 +1 差异说明）
        let a_eff = a_i - evm_div((a_i - a_next) * offset, y_i);
        let delta_eff = a_eff - a_next;
        let p = evm_div(MILLION * TWO * r, a_eff + a_next);
        let th = evm_div(p * BILLION * scale + field0 - U256::from(1), field0);
        if xp >= th {
            acc += r;
            xp -= th;
        } else {
            let r2 = evm_div(field0 * xp, BILLION * scale);
            let part = evm_div(r2 * TWO * r * a_eff, MILLION * TWO * r + r2 * delta_eff);
            acc += part;
            return min_u256(acc, reserve_y);
        }
    }
    // pos 超过全部段或 xp 未耗尽：按末段 a 直线外推
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
/// - 正向：链上 quote 是"当前位置感知"的（`cfg+7` low96），本地直接在
///   `pos_forward + consumed_out` 位置对本次输入报价，用当前剩余储备封顶
///   （与链上 `min(out, reserveY)` 一致）；段内分数线性插值不可加，不能
///   用旧版 `quote(consumed_in + amount_in) - consumed_out` 全量重报。
/// - 反向：保持已验证公式（112 条链上 quote 零偏差），
///   `total_out = quote_reverse(pos_reverse, consumed_in + amount_in)` 封顶
///   快照总储备，`amount_out = total_out - consumed_out`。
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

    let amount_out = if forward {
        // 正向：当前链上位置 + 本次模拟已消费的输出量，对本次输入直接报价；
        // 封顶用当前剩余储备（模拟 swap 使 reserve 递减，与链上实时一致）。
        let pos = state.pos_forward + *consumed_out;
        quote_forward_pos_exact(
            ladder,
            state.field0,
            state.field1,
            state.fee_rate,
            state.window,
            state.scale,
            pos,
            *reserve_out,
            amount_in,
        )
    } else {
        // 反向：保持已验证公式（封顶快照总储备，全量重报减已消费）。
        let total_in = *consumed_in + amount_in;
        let total_reserve = *reserve_out + *consumed_out;
        let total_out = quote_reverse_exact(
            ladder,
            state.field0,
            state.field1,
            state.fee_rate,
            state.window,
            state.scale,
            state.pos_reverse,
            total_reserve,
            total_in,
        );
        if *consumed_out > total_out {
            return Err(AMMError::Msg("caliber: insufficient liquidity".to_string()));
        }
        total_out - *consumed_out
    };

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
        // Caliber 合约确实 emit Swap 事件（2026-08-11 实测），但不走通用日志管道：
        // 由 flashblocks 提取通道按 `caliber_contracts` 地址预筛单独解析并
        // `apply_chain_swap` 应用，此处返回空避免双重应用（同一事件被
        // `apply_logs_for_block_timed` 再应用一次会重复扣减储备/pos）。
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
        // 同时校验报价时效性：链上 quote() 在 block.timestamp > deadline + validity_window
        // 或暂停时直接 revert（StalePrices/暂停错误），本地必须把过期/暂停 pair 视为
        // 不可报价，否则会算出"幻影利润"导致上链回滚（事故 tx 0x7dbc...b047a）。
        if ladder.is_empty() || self.ladder.is_unquotable(unix_now()) {
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
        // 与 simulate_swap 一致的时效性/空 ladder 守卫：过期/暂停/无曲线时
        // 直接返回 0（不可报价），不修改任何 consumed/储备状态。
        if ladder.is_empty() || self.ladder.is_unquotable(unix_now()) {
            return Ok(U256::ZERO);
        }
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

        // 快照有效时刷新完整 ladder 并重置 consumed（旧值无意义）；
        // 快照因过期/暂停返回空 ladder 时保留最近一次有效 ladder + consumed，
        // 使后续实时更新（刷新 deadline）能立即恢复报价，而不是等下一轮对账。
        if !snap.ladder.is_empty() {
            self.ladder.ladder_a_to_b = Arc::new(snap.ladder.clone());
            self.ladder.ladder_b_to_a = Arc::new(snap.ladder);
            self.ladder.consumed_in_ab = U256::ZERO;
            self.ladder.consumed_out_ab = U256::ZERO;
            self.ladder.consumed_in_ba = U256::ZERO;
            self.ladder.consumed_out_ba = U256::ZERO;
        }
        // 报价参数与时效性参数始终刷新（过期/暂停由报价路径在报价时判定）
        self.ladder.field0 = snap.field0;
        self.ladder.field1 = snap.field1;
        self.ladder.fee_rate = snap.fee_rate;
        self.ladder.window = snap.window;
        self.ladder.scale = snap.scale;
        self.ladder.pos_reverse = snap.pos_reverse;
        self.ladder.pos_forward = snap.pos_forward;
        self.ladder.deadline = snap.deadline;
        self.ladder.validity_window = snap.validity_window;
        self.ladder.paused = snap.paused;

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
    /// 反向报价位置（cfg+7 mid96，仅当 cfg+7.block == 当前块时有效，否则 0）
    pos_reverse: U256,
    /// 正向报价位置（cfg+7 low96，仅当 cfg+7.block == 当前块时有效，否则 0）
    pos_forward: U256,
    /// data+0 完整 64 位 deadline（tsY<<32|tsX），报价过期时间戳
    deadline: u64,
    /// 全局 slot2 有效期（本合约 20s）
    validity_window: u64,
    /// 全局/单 pair 暂停态快照
    paused: bool,
}

/// 单次 JSON-RPC batch 请求中的 `eth_getStorageAt` 数量上限。
///
/// 实测生产 `rpc.xlayer.tech` 的 batch 上限为 11（12 即拒绝
/// `too many RPC calls in batch request`），取安全值 10。
const STORAGE_BATCH_SIZE: usize = 10;

/// caliber 存储读取专用 HTTP RPC。
///
/// XLayer 生产 WS 网关（`wss://ws.xlayer.tech`）不开放 `eth_getStorageAt`
/// （`-32601: rpc method is not whitelisted`），而初始化/周期对账必须直读
/// 合约 storage `cfg`/`data`/`ladder` 槽位；因此本模块的存储读取统一走
/// 该 HTTP RPC。实时报价更新仍由 flashblocks 原始交易流驱动，不受影响。
///
/// ⚠️ 临时方案（2026-08-07）：为快速解决生产 WS 网关拒绝 `eth_getStorageAt`
/// 的问题，此处硬编码了 `rpc.xlayer.tech` 公共端点。后续迭代应改为：
/// 1. 由上层（dex-arbitrage chain 配置 `http_rpc_url`）显式传入 HTTP RPC，而不是
///    在库内硬编码公共端点（公共端点有 rate limit，且多环境不可移植）；
/// 2. 或改用合约自带 view 函数（`getPoolBalances(pairId)` / `quote()`，WS 上
///    `eth_call` 可用）替代直读存储槽，从而完全摆脱对 `eth_getStorageAt` 的依赖。
const CALIBER_STORAGE_HTTP_RPC: &str = "https://rpc.xlayer.tech";

/// 懒初始化的 HTTP provider（进程内复用，避免每次对账重连）。
static CALIBER_STORAGE_PROVIDER: OnceLock<DynProvider> = OnceLock::new();

fn caliber_storage_provider() -> Result<&'static DynProvider, AMMError> {
    if let Some(provider) = CALIBER_STORAGE_PROVIDER.get() {
        return Ok(provider);
    }
    let url: alloy::transports::http::reqwest::Url = CALIBER_STORAGE_HTTP_RPC
        .parse()
        .map_err(|e| AMMError::Msg(format!("caliber: invalid storage rpc url: {e}")))?;
    let provider: DynProvider = ProviderBuilder::new().connect_http(url).erased();
    let _ = CALIBER_STORAGE_PROVIDER.set(provider);
    Ok(CALIBER_STORAGE_PROVIDER
        .get()
        .expect("caliber storage provider just initialized"))
}

/// batch 回退告警只输出一次（避免每周期每 chunk 刷屏）
static BATCH_FALLBACK_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn warn_batch_fallback(e: &AMMError) {
    if !BATCH_FALLBACK_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            error = ?e,
            "caliber: JSON-RPC batch rejected by RPC gateway; falling back to per-slot eth_getStorageAt (logged once)"
        );
    } else {
        tracing::debug!(
            error = ?e,
            "caliber: JSON-RPC batch failed, using per-slot fallback"
        );
    }
}

/// 判断错误是否属于 RPC 限流/过载类（HTTP 429 / JSON-RPC -32016 over rate limit）。
///
/// 限流时继续逐槽回退只会把请求量放大 ~8 倍（72+27+2 个 per-slot
/// `eth_getStorageAt`）并进一步加剧限流，必须直接放弃本轮、等待下一轮
/// 对账（配合对账任务的指数退避）。非限流错误（如网关拒绝 batch 的
/// `-32601 method not whitelisted`）仍走逐槽回退兜底。
fn is_rate_limited_err(e: &impl std::fmt::Debug) -> bool {
    let msg = format!("{e:?}");
    msg.contains("429") || msg.contains("rate limit") || msg.contains("-32016")
}

/// 存储读取块高钳制告警只输出一次（避免每次 resync/coverage 刷屏）。
static STORAGE_CLAMP_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn warn_storage_clamp(requested_block: u64, storage_head: u64) {
    if !STORAGE_CLAMP_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            requested_block,
            storage_head,
            "caliber: requested block ahead of storage RPC head; clamping snapshot read to storage head (logged once)"
        );
    } else {
        tracing::debug!(
            requested_block,
            storage_head,
            "caliber: clamping snapshot read to storage head"
        );
    }
}

/// 批量存储读取的固定请求间隔（毫秒）。
///
/// ⚠️ 临时节流（2026-08-10）：`rpc.xlayer.tech` 公共 HTTP 端点对
/// `eth_getStorageAt` 限流严格（`-32016 over rate limit`），初始化/周期对账的
/// 多个 batch 在极短窗口内连发易叠加触发限流；这里在每批 HTTP 请求前固定
/// sleep，保证请求间隔均匀（≈ 响应时间 + 200ms），摊平瞬时 RPS。
/// 后续接入独立/高配额 HTTP 端点后可移除。
const STORAGE_BATCH_SLEEP_MS: u64 = 200;

/// 固定节流：每批批量存储 HTTP 请求前 sleep `STORAGE_BATCH_SLEEP_MS`。
async fn throttle_storage_batch() {
    tokio::time::sleep(std::time::Duration::from_millis(STORAGE_BATCH_SLEEP_MS)).await;
}

/// 存储读取块高钳制：caliber 存储读取走硬编码 HTTP RPC
/// （`caliber_storage_provider`），该节点头部可能落后于调用方传入的块高
/// （如 maintenance Resync/coverage 传入的 flashblocks 乐观头），直接查询会
/// 触发 `-32019 block is out of range`。这里把数字块钳制到 HTTP 节点头部：
/// 历史块（≤ 头部）原样保留（`initial_block` 回填/回放的块语义不变），
/// 超前块降级为 HTTP 已收录的块，从根上消除 -32019。实时报价仍由
/// flashblocks 交易驱动，存储快照的语义始终是"当前链上状态"。
/// 头部查询失败时按原块继续，不引入新的失败路径。
async fn clamp_block_to_storage_head(block: BlockId) -> BlockId {
    let BlockId::Number(alloy::eips::BlockNumberOrTag::Number(num)) = block else {
        return block;
    };
    let Ok(http_provider) = caliber_storage_provider() else {
        return block;
    };
    match http_provider.get_block_number().await {
        Ok(head) if head < num => {
            warn_storage_clamp(num, head);
            BlockId::Number(alloy::eips::BlockNumberOrTag::Number(head))
        }
        _ => block,
    }
}

/// 通过单次 JSON-RPC batch 读取多个存储槽（一个 HTTP 请求）。
///
/// 与逐槽 `eth_getStorageAt` 完全等价（同一 `block`、同一序列化参数），
/// 仅把 N 次 RPC 往返折叠为 1 次。所有请求必须指向同一区块。
async fn storage_at_batch<N, P>(
    _provider: &P,
    reads: &[(Address, B256)],
    block: BlockId,
) -> Result<Vec<U256>, AMMError>
where
    N: Network,
    P: Provider<N>,
{
    // 存储读取统一走硬编码 HTTP RPC（WS 网关不开放 eth_getStorageAt）。
    // 先尝试 JSON-RPC batch（一个 HTTP 请求）；若网关拒绝 batch
    // （如 -32601 method not whitelisted、大小超限截断等），回退逐槽读取。
    // batch 只是优化路径，逐槽是可靠基线，保证任意网关下可用。
    let http_provider = caliber_storage_provider()?;
    let mut batch = alloy::rpc::client::BatchRequest::new(http_provider.client());
    let mut waiters = Vec::with_capacity(reads.len());
    for (address, slot) in reads {
        let key = U256::from_be_bytes(slot.0);
        let waiter = batch
            .add_call::<_, B256>("eth_getStorageAt", &(*address, key, block))
            .map_err(|e| AMMError::Msg(format!("caliber: batch add_call failed: {e}")))?;
        waiters.push(waiter);
    }

    let mut out = Vec::with_capacity(reads.len());
    let mut batch_ok = true;
    throttle_storage_batch().await;
    if let Err(e) = batch.send().await {
        batch_ok = false;
        if is_rate_limited_err(&e) {
            return Err(AMMError::Msg(format!(
                "caliber: batch get_storage_at rate limited: {e}"
            )));
        }
        warn_batch_fallback(&AMMError::Msg(format!("caliber: batch send failed: {e}")));
    }
    if batch_ok {
        for waiter in waiters {
            match waiter.await {
                Ok(value) => out.push(U256::from_be_bytes(value.0)),
                Err(e) => {
                    batch_ok = false;
                    if is_rate_limited_err(&e) {
                        return Err(AMMError::Msg(format!(
                            "caliber: batch get_storage_at rate limited: {e}"
                        )));
                    }
                    warn_batch_fallback(&AMMError::Msg(format!(
                        "caliber: batch get_storage_at failed: {e}"
                    )));
                    break;
                }
            }
        }
    }
    if !batch_ok {
        out.clear();
        for (address, slot) in reads {
            let key = U256::from_be_bytes(slot.0);
            let value = http_provider
                .get_storage_at(*address, key)
                .block_id(block)
                .await
                .map_err(|e| AMMError::Msg(format!("caliber: get_storage_at failed: {e}")))?;
            out.push(value);
        }
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

/// 全局/单 pair 暂停判定（EVM trace 确认）：
/// - `SLOAD(3) & 0xff != 0` → 全局暂停 revert 0x8507a90d
/// - `SLOAD(cfg+6) byte@0x40 != 0` → per-pair 暂停 revert 0xb69ec3f0
fn pair_paused(raw: &RawPairSlots, global_paused: U256) -> bool {
    !(global_paused & U256::from(0xff)).is_zero()
        || !((raw.cfg6 >> U256::from(0x40)) & U256::from(0xff)).is_zero()
}

/// 完整 64 位报价过期时间戳：`((data0 >> 128) & u32) << 32 | ((data0 >> 96) & u32)`
/// （即 `tsY << 32 | tsX`）。tsY 非零时 deadline 为巨大值，链上永不过期。
fn pair_deadline64(raw: &RawPairSlots) -> U256 {
    (((raw.data0 >> U256::from(128)) & U256::from(u32::MAX)) << U256::from(32))
        | ((raw.data0 >> U256::from(96)) & U256::from(u32::MAX))
}

/// 复刻链上 quote() 的过期/暂停判断（EVM trace 确认）：
/// - `deadline64 + validity_window < block.timestamp` → revert 0x2af96ae8（StalePrices）
/// - 暂停 → revert 0x8507a90d / 0xb69ec3f0
///
/// 注意：当 data0 高 32 位（tsY）非零时，deadline 变为 64 位巨大值，链上永不
/// 过期——这是合约的实际行为，本地必须同样处理。
fn pair_stale(
    raw: &RawPairSlots,
    validity_window: U256,
    global_paused: U256,
    block_ts: U256,
) -> bool {
    let expired = block_ts > pair_deadline64(raw) + validity_window;
    pair_paused(raw, global_paused) || expired
}

/// 由原始槽位值构建 `CaliberSnapshot`（单池与批量路径共用）。
///
/// 存储布局（XLayer 合约 `0x154586B2479b9a11e3d4db90024Dc0e26F097312`，经字节码逆向确认）：
/// - `cfg = keccak256(pairId || 6)`：`+1` token1（byte@0xa0=dec_x，byte@0xa8=dec_y），
///   `+2` ladder 长度，`+3` window，`+4` reserveX，`+5` reserveY，`+6` 低 64 位 = fee，
///   `+7` = [block:32][0:64][mid96:96][low96:96]（仅当 block == 当前块时有效：
///   反向读 mid96、正向读 low96，方向切换时另一字段归零）
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

    // 报价位置：cfg+7 = [block:32][0:64][mid96:96][low96:96]，高 64 位
    // 低 32 位为最近更新区块。链上（EVM trace 确认）仅在 cfg+7.block ==
    // 当前执行块时使用真实 pos，否则按 pos=0（从段 0 整段）计算：
    // - 反向（token_y → token_x）读 mid96（bits 96..191）
    // - 正向（token_x → token_y）读 low96（bits 0..95）
    // 每次 swap 只写当前方向的 pos 字段并将另一方向归零，因此两个字段
    // 必须分别门控，不能合并。
    let pos_block = raw.cfg7 >> U256::from(192);
    let pos_mask = (U256::from(1) << U256::from(96)) - U256::from(1);
    let pos_reverse = if pos_block == cur_block {
        (raw.cfg7 >> U256::from(96)) & pos_mask
    } else {
        U256::ZERO
    };
    let pos_forward = if pos_block == cur_block {
        raw.cfg7 & pos_mask
    } else {
        U256::ZERO
    };
    // 完整 64 位 deadline（tsY<<32|tsX）+ 暂停态：时效性参数必须随快照持久化，
    // 由报价路径（simulate_swap）在每次报价时判定，不能只在快照构建时消费。
    let paused = pair_paused(&raw, global_paused);
    let deadline64 = pair_deadline64(&raw).to::<u64>();

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
        pos_reverse,
        pos_forward,
        deadline: deadline64,
        validity_window: validity_window.to::<u64>(),
        paused,
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
    // 存储读取统一钳制到 HTTP 节点头部，避免请求超前块触发 -32019
    // （Resync/coverage 传入的是 flashblocks 乐观头，HTTP 节点可能落后）。
    let block = clamp_block_to_storage_head(block).await;

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

    // 存储读取统一钳制到 HTTP 节点头部（见 clamp_block_to_storage_head）。
    let block = clamp_block_to_storage_head(block).await;

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

    /// 构造一个带有效 ladder 的测试池（pair2 真实参数，正向报价非零）。
    fn test_pool_with_ladder() -> CaliberPropPool {
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
            reserve_a: U256::from(6210305049u64),
            reserve_b: U256::from(1760056227u64),
            ladder: Default::default(),
            price_a_in_b: 1.0,
            price_b_in_a: 1.0,
        };
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
        pool.ladder.ladder_a_to_b = Arc::new(ladder.clone());
        pool.ladder.ladder_b_to_a = Arc::new(ladder);
        pool.ladder.field0 = U256::from(75111231784u64);
        pool.ladder.field1 = U256::from(283u64);
        pool.ladder.fee_rate = U256::from(200u64);
        pool.ladder.window = U256::from(500u64);
        pool.ladder.scale = U256::from(1000u64);
        pool.ladder.pos_reverse = U256::ZERO;
        pool.ladder.pos_forward = U256::ZERO;
        pool.ladder.deadline = unix_now() + 3600; // 默认新鲜
        pool.ladder.validity_window = 20;
        pool
    }

    #[test]
    fn test_simulate_swap_stale_returns_zero_fresh_quotes() {
        let now = unix_now();
        let mut pool = test_pool_with_ladder();

        // 新鲜：deadline 在未来 → 正常报价（非 0）
        let fresh = pool
            .simulate_swap(
                pool.token_a.address,
                pool.token_b.address,
                U256::from(1_000_000_000u64),
            )
            .unwrap();
        assert!(fresh > U256::ZERO, "fresh quote should be non-zero");

        // 过期：deadline 在过去 → 不可报价（返回 0，避免幻影利润上链回滚）
        pool.ladder.deadline = now - 3600;
        let stale = pool
            .simulate_swap(
                pool.token_a.address,
                pool.token_b.address,
                U256::from(1_000_000_000u64),
            )
            .unwrap();
        assert_eq!(stale, U256::ZERO, "stale pair must be unquotable");
    }

    #[test]
    fn test_simulate_swap_mut_stale_returns_zero_without_mutation() {
        let now = unix_now();
        let mut pool = test_pool_with_ladder();
        pool.ladder.deadline = now - 3600;
        pool.ladder.consumed_in_ab = U256::from(12345u64);
        pool.ladder.consumed_out_ab = U256::from(67890u64);

        let out = pool
            .simulate_swap_mut(
                pool.token_a.address,
                pool.token_b.address,
                U256::from(1_000_000_000u64),
            )
            .unwrap();
        assert_eq!(out, U256::ZERO, "stale pair must be unquotable");
        assert_eq!(
            pool.ladder.consumed_in_ab,
            U256::from(12345u64),
            "consumed must not mutate"
        );
        assert_eq!(
            pool.ladder.consumed_out_ab,
            U256::from(67890u64),
            "consumed must not mutate"
        );
    }

    #[test]
    fn test_apply_batch_update_revives_stale_pool() {
        let now = unix_now();
        let mut pool = test_pool_with_ladder();
        pool.ladder.deadline = now - 3600; // 过期
        assert_eq!(
            pool.simulate_swap(
                pool.token_a.address,
                pool.token_b.address,
                U256::from(1_000_000_000u64)
            )
            .unwrap(),
            U256::ZERO
        );

        // 实时更新刷新 deadline → ladder 保留自最近有效快照，立即恢复报价
        pool.apply_batch_update(
            &CaliberBatchUpdate {
                pair_id: pool.pair_id,
                price: U256::from(75_111_231_784u64),
                flags: 283,
                deadline: now + 3600,
            },
            1,
        );
        let revived = pool
            .simulate_swap(
                pool.token_a.address,
                pool.token_b.address,
                U256::from(1_000_000_000u64),
            )
            .unwrap();
        assert!(
            revived > U256::ZERO,
            "updated pair should be quotable again"
        );
    }

    #[test]
    fn test_pair_deadline64_ts_y_nonzero_never_expires() {
        let mk = |data0: U256| RawPairSlots {
            cfg1: U256::ZERO,
            n: U256::ZERO,
            window: U256::ZERO,
            reserve_x: U256::ZERO,
            reserve_y: U256::ZERO,
            cfg6: U256::ZERO,
            cfg7: U256::ZERO,
            data0,
        };
        let validity = U256::from(20u64);
        let block_ts = U256::from(unix_now());
        let ts_x = U256::from(1u64) << 96; // 仅 tsX（更新路径，tsY=0）

        // tsY 非零 → deadline64 巨大 → 永不过期
        let ts_y = U256::from(0xffff_ffffu64) << 128;
        assert!(!pair_stale(
            &mk(ts_y | ts_x),
            validity,
            U256::ZERO,
            block_ts
        ));

        // 仅 tsX → 过期
        assert!(pair_stale(&mk(ts_x), validity, U256::ZERO, block_ts));

        // 暂停 → stale
        assert!(pair_stale(
            &mk(ts_y | ts_x),
            validity,
            U256::from(1u64),
            block_ts
        ));
    }

    #[test]
    fn test_legacy_snapshot_zero_validity_is_conservatively_stale() {
        let now = unix_now();
        let mut pool = test_pool_with_ladder();
        // 旧快照反序列化：validity_window=0（serde default），deadline 已过 → 保守视为过期
        pool.ladder.deadline = now - 1;
        pool.ladder.validity_window = 0;
        assert!(pool.ladder.is_unquotable(now));
        assert_eq!(
            pool.simulate_swap(
                pool.token_a.address,
                pool.token_b.address,
                U256::from(1_000_000_000u64)
            )
            .unwrap(),
            U256::ZERO
        );
    }

    #[test]
    fn test_apply_snapshot_keeps_ladder_when_stale() {
        let now = unix_now();
        let mut pool = test_pool_with_ladder();
        let kept_len = pool.ladder.ladder_a_to_b.len();
        assert!(kept_len > 0);

        // 过期快照（空 ladder + 过去 deadline）：保留最近有效 ladder，刷新时效性参数
        let stale_snap = CaliberSnapshot {
            reserve_a: U256::from(1u64),
            reserve_b: U256::from(2u64),
            ladder: Vec::new(),
            field0: U256::from(999u64),
            field1: U256::from(3u64),
            fee_rate: U256::from(200u64),
            window: U256::from(500u64),
            scale: U256::from(1000u64),
            pos_reverse: U256::ZERO,
            pos_forward: U256::ZERO,
            deadline: now - 3600,
            validity_window: 20,
            paused: false,
        };
        pool.apply_snapshot(stale_snap);
        assert_eq!(
            pool.ladder.ladder_a_to_b.len(),
            kept_len,
            "stale snapshot must keep last valid ladder"
        );
        assert_eq!(pool.ladder.deadline, now - 3600);
        assert_eq!(pool.ladder.validity_window, 20);
        // 保留的 ladder + 过期 deadline → 不可报价
        assert_eq!(
            pool.simulate_swap(
                pool.token_a.address,
                pool.token_b.address,
                U256::from(1_000_000_000u64)
            )
            .unwrap(),
            U256::ZERO
        );

        // 新鲜快照（非空 ladder）：替换 ladder 并重置 consumed
        let fresh_snap = CaliberSnapshot {
            reserve_a: U256::from(1u64),
            reserve_b: U256::from(2u64),
            ladder: vec![LadderPoint {
                amount_in: U256::from(1u64),
                amount_out: U256::from(2u64),
            }],
            field0: U256::from(999u64),
            field1: U256::from(3u64),
            fee_rate: U256::from(200u64),
            window: U256::from(500u64),
            scale: U256::from(1000u64),
            pos_reverse: U256::ZERO,
            pos_forward: U256::ZERO,
            deadline: now + 3600,
            validity_window: 20,
            paused: false,
        };
        pool.ladder.consumed_in_ab = U256::from(777u64);
        pool.apply_snapshot(fresh_snap);
        assert_eq!(
            pool.ladder.ladder_a_to_b.len(),
            1,
            "fresh snapshot must replace ladder"
        );
        assert_eq!(
            pool.ladder.consumed_in_ab,
            U256::ZERO,
            "consumed must reset on fresh snapshot"
        );
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

    #[test]
    fn test_refresh_prices_uses_marginal_price() {
        // 现货价 = 链上边际价格 field0 * (1e6 - (x0 + field1)) / 1e15（docs §5），
        // 而不是首段斜率 y0/x0（旧实现，与真实价格可差多个数量级）。
        // 用真实 xSOL 对账基线（块 66309105）：ladder=[(10,2e8),(50,9e8),(300,1e9)]，
        // field0=75111231784, field1=283 → 边际价 ≈ 75.09；
        // 首段斜率 y0/x0=2e7，×10^(18-6) 归一化后 = 2e19，错 17 个数量级。
        let contract = Address::repeat_byte(0xBB);
        let pair_id = B256::from([0x22u8; 32]);
        let mut pool = CaliberPropPool {
            contract_address: contract,
            pair_id,
            virtual_address: CaliberPropPool::virtual_address_from_pair_id(pair_id, contract),
            token_x: Address::from([1u8; 20]),
            token_y: Address::from([2u8; 20]),
            token_a: Token::new_with_decimals(Address::from([1u8; 20]), 18),
            token_b: Token::new_with_decimals(Address::from([2u8; 20]), 6),
            created_block: 0,
            last_synced_block: 0,
            reserve_a: U256::from(6210305049u64),
            reserve_b: U256::from(1760056227u64),
            ladder: Default::default(),
            price_a_in_b: 1.0,
            price_b_in_a: 1.0,
        };
        pool.ladder.ladder_a_to_b = Arc::new(vec![
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
        ]);
        pool.ladder.field0 = U256::from(75111231784u64);
        pool.ladder.field1 = U256::from(283u64);

        pool.refresh_prices();

        let expected = 75111231784f64 * (1_000_000f64 - (10f64 + 283f64)) / 1e15;
        assert!(
            (pool.price_a_in_b - expected).abs() < 1e-9,
            "price_a_in_b={} expected={}",
            pool.price_a_in_b,
            expected
        );
        assert!((pool.price_b_in_a - 1.0 / expected).abs() < 1e-12);
    }

    /// 实时更新（batchUpdateParameters）必须立即刷新现货缓存：
    /// field0/field1 是边际价公式的输入，若只写不读 refresh_prices，
    /// spot 会滞后到下一轮对账（最长 30s），下游 price filter 用过时价格。
    #[test]
    fn test_apply_batch_update_refreshes_spot_price() {
        let mut pool = test_pool_with_ladder();
        pool.refresh_prices();
        let before = pool.price_a_in_b;
        assert!(before > 0.0);

        let update = CaliberBatchUpdate {
            pair_id: pool.pair_id,
            price: U256::from(2_000_000_000_000u64), // 模拟新报价 field0
            flags: 283,
            deadline: unix_now() + 3600,
        };
        pool.apply_batch_update(&update, 12345);

        // 期望 = field0 * (1e6 - (x0 + field1)) / 1e15（x0=10, field1=283）
        let expected = 2_000_000_000_000f64 * (1_000_000f64 - 293f64) / 1e15;
        assert_ne!(
            pool.price_a_in_b, before,
            "spot price must refresh on batch update"
        );
        assert!(
            (pool.price_a_in_b - expected).abs() < 1e-6,
            "price_a_in_b={} expected={}",
            pool.price_a_in_b,
            expected
        );
        assert!((pool.price_b_in_a - 1.0 / expected).abs() < 1e-12);
    }

    /// 回归：XLayer 块 67564091 真实 pair（USDT0/wNVDAx，token_x=wNVDAx != token_a）。
    ///
    /// 旧实现把边际价 "1 token_x = X token_y" 直接存进 price_a_in_b，当
    /// token_x == token_b（本 pair 即如此）时方向被存反：spot_price(USDT0, wNVDAx)
    /// 返回 225.06（a/b），而同 pair 的 V3 池同调用返回 0.004458（b/a），方向语义
    /// 相反，TwoCycle 预筛 case 翻转、只评估 Caliber→V3 错误方向，漏掉竞争者吃到的
    /// V3→Caliber 套利（tx 0xa7d5157714582cd74af70886e10afd2aeaa5d0f825dbd37a97751dbff7001f8e）。
    #[test]
    fn test_spot_price_orientation_real_xlayer_pair_67564091() {
        use std::str::FromStr;

        let usdt0 = Address::from_str("0x779Ded0c9e1022225f8E0630b35a9b54bE713736").unwrap();
        let wnvda = Address::from_str("0xa8ddb5Cd96b5222AFe198316E9A57CAA642850D5").unwrap();

        // 块 67564091 真实状态（竞争者 tx 17 之前，与回放 A1 一致）：
        // token_x=wNVDAx(18dp), token_y=USDT0(6dp), ladder=[(10,2e8),(50,9e8),(300,1e9)],
        // field0=225133337972, field1=298, fee=200, win=500, scale=1e12。
        let mut pool = CaliberPropPool {
            contract_address: Address::ZERO,
            pair_id: B256::ZERO,
            virtual_address: Address::ZERO,
            token_x: wnvda,
            token_y: usdt0,
            token_a: Token::new_with_decimals(usdt0, 6),
            token_b: Token::new_with_decimals(wnvda, 18),
            created_block: 0,
            last_synced_block: 0,
            reserve_a: U256::from(1752257155u64),
            reserve_b: "141552681951730783366".parse::<U256>().unwrap(),
            ladder: Default::default(),
            price_a_in_b: 1.0,
            price_b_in_a: 1.0,
        };
        pool.ladder.ladder_a_to_b = Arc::new(vec![
            LadderPoint {
                amount_in: U256::from(10u64),
                amount_out: U256::from(200_000_000u64),
            },
            LadderPoint {
                amount_in: U256::from(50u64),
                amount_out: U256::from(900_000_000u64),
            },
            LadderPoint {
                amount_in: U256::from(300u64),
                amount_out: U256::from(1_000_000_000u64),
            },
        ]);
        pool.ladder.field0 = U256::from(225133337972u64);
        pool.ladder.field1 = U256::from(298u64);
        pool.ladder.fee_rate = U256::from(200u64);
        pool.ladder.window = U256::from(500u64);
        pool.ladder.scale = "1000000000000".parse::<U256>().unwrap();
        pool.ladder.pos_reverse = U256::ZERO;
        pool.ladder.pos_forward = U256::ZERO;
        pool.ladder.deadline = unix_now() + 3600;
        pool.ladder.validity_window = 20;

        pool.refresh_prices();

        // 边际价 = field0 * (1e6 - (x0 + field1)) / 1e15 = 225.0639969039046
        // = "1 token_x(wNVDAx) = 225.0639969039046 token_y(USDT0)" = a/b。
        let price_xy = 225133337972f64 * (1_000_000f64 - (10f64 + 298f64)) / 1e15;
        // price_a_in_b = price of a(USDT0) in terms of b(wNVDAx) = b/a
        assert!(
            (pool.price_a_in_b - 1.0 / price_xy).abs() < 1e-12,
            "price_a_in_b={}",
            pool.price_a_in_b
        );
        assert!(
            (pool.price_b_in_a - price_xy).abs() < 1e-9,
            "price_b_in_a={}",
            pool.price_b_in_a
        );

        // trait 契约：spot_price(base, quote) = price of base in terms of quote。
        // 修复后与同 pair 的 V3 池方向一致：spot(USDT0, wNVDAx) 都是 quote/base。
        let p_a_in_b = pool.spot_price(usdt0, wnvda).unwrap();
        let p_b_in_a = pool.spot_price(wnvda, usdt0).unwrap();
        assert!(
            (p_a_in_b - 1.0 / price_xy).abs() < 1e-12,
            "spot(USDT0,wNVDAx)={p_a_in_b}"
        );
        assert!(
            (p_b_in_a - price_xy).abs() < 1e-9,
            "spot(wNVDAx,USDT0)={p_b_in_a}"
        );

        // TwoCycle 预筛回归：V3 块 67564091 spot(USDT0,wNVDAx)=0.0044583977，
        // case_a = p_v3 > p_cal * spread(1.000012) 必须成立 → 选中 V3→Caliber 方向。
        let p_v3 = 0.004458397727798574f64;
        assert!(
            p_v3 > p_a_in_b * 1.000012,
            "case_a must hold: p_v3={p_v3} p_cal={p_a_in_b}"
        );
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

    /// 正向 pos 版本公式：48 数据点（块 67650064 anvil fork 链上 quote 实测，
    /// 2026-08-11 验证，零偏差）。
    ///
    /// 该 pair（b2d5c47f…）ladder=[(10,2e8),(50,9e8),(300,1e9)]，
    /// field0=219438865054 field1=296 fee=200 win=500 scale=1e12
    /// reserve_y=436263828（cfg+5 @ 67650063）。POS 为 cfg+7 low96
    /// （正向累计已兑换 y），AMTS 为输入（W→U，1e6 基数）。
    #[test]
    fn test_quote_forward_pos_exact_vectors_accident_block() {
        let ladder = vec![
            LadderPoint {
                amount_in: U256::from(10u64),
                amount_out: U256::from(200_000_000u64),
            },
            LadderPoint {
                amount_in: U256::from(50u64),
                amount_out: U256::from(900_000_000u64),
            },
            LadderPoint {
                amount_in: U256::from(300u64),
                amount_out: U256::from(1_000_000_000u64),
            },
        ];
        let field0 = U256::from(219_438_865_054u64);
        let field1 = U256::from(296u64);
        let fee_rate = U256::from(200u64);
        let window = U256::from(500u64);
        let scale = U256::from(1_000_000_000_000u64);
        let ry = U256::from(436_263_828u64);

        let amts: [u64; 6] = [
            100_000_000_000_000_000,
            500_000_000_000_000_000,
            977_689_766_888_449_551,
            1_200_000_000_000_000_000,
            1_900_532_488_745_085_420,
            2_500_000_000_000_000_000,
        ];
        let poss: [(u64, [u64; 6]); 8] = [
            (
                0,
                [
                    21_932_735,
                    109_662_717,
                    214_429_979,
                    263_186_326,
                    416_820_481,
                    436_263_828,
                ],
            ),
            (
                300_000_000,
                [
                    21_931_246,
                    109_654_894,
                    214_413_805,
                    263_166_114,
                    416_787_531,
                    436_263_828,
                ],
            ),
            (
                416_820_481,
                [
                    21_930_522,
                    109_651_278,
                    214_406_741,
                    263_157_448,
                    416_773_828,
                    436_263_828,
                ],
            ),
            (
                500_000_000,
                [
                    21_930_017,
                    109_648_753,
                    214_401_803,
                    263_151_385,
                    416_764_219,
                    436_263_828,
                ],
            ),
            (
                700_000_000,
                [
                    21_928_810,
                    109_642_710,
                    214_389_967,
                    263_136_848,
                    416_741_112,
                    436_263_828,
                ],
            ),
            (
                1_050_000_000,
                [
                    21_926_660,
                    109_631_569,
                    214_365_974,
                    263_106_050,
                    416_685_482,
                    436_263_828,
                ],
            ),
            (
                1_300_000_000,
                [
                    21_924_106,
                    109_618_132,
                    214_339_437,
                    263_073_418,
                    416_633_642,
                    436_263_828,
                ],
            ),
            (
                1_900_000_000,
                [
                    21_917_525,
                    109_585_223,
                    214_275_142,
                    262_995_434,
                    416_520_296,
                    436_263_828,
                ],
            ),
        ];

        for (pos, expected) in poss {
            for (amt, exp) in amts.iter().zip(expected) {
                let out = quote_forward_pos_exact(
                    &ladder,
                    field0,
                    field1,
                    fee_rate,
                    window,
                    scale,
                    U256::from(pos),
                    ry,
                    U256::from(*amt),
                );
                assert_eq!(out, U256::from(exp), "fwd pos={pos} amt={amt}");
            }
        }

        // pos=0 退化为无状态公式：与 quote_forward_exact 逐位一致
        for amt in amts {
            assert_eq!(
                quote_forward_pos_exact(
                    &ladder,
                    field0,
                    field1,
                    fee_rate,
                    window,
                    scale,
                    U256::ZERO,
                    ry,
                    U256::from(amt),
                ),
                quote_forward_exact(
                    &ladder,
                    field0,
                    field1,
                    fee_rate,
                    window,
                    scale,
                    ry,
                    U256::from(amt),
                ),
                "pos=0 退化一致性 amt={amt}"
            );
        }
    }

    /// 事故块 67650064 端到端回放（取证 2026-08-09，现 bug 根因）：
    ///
    /// 块内两笔正向 swap（W→U）后，链上 cfg+7 low96=435976752、
    /// cfg+5=287076。此时再 quote(W→U, A2=977689766888449551)，
    /// 链上 = 287076（= 当前剩余储备封顶）；旧实现忽略 low96 pos 且按
    /// 快照总储备封顶，给出 214429979（pos=0 的无状态全量报价），
    /// 导致本地产生"幻影利润"误报套利计划。
    #[test]
    fn test_simulate_swap_forward_pos_block_end_accident() {
        let contract = address!("0x154586B2479b9a11e3d4db90024Dc0e26F097312");
        let pair_id = b256!("b2d5c47f635aa119fc5e911aa881db33bc77b61a1872d035d6122869e24a9673");
        let w = address!("0xa8ddb5cd96b5222afe198316e9a57caa642850d5");
        let u = address!("0x779ded0c9e1022225f8e0630b35a9b54be713736");
        let mut pool = CaliberPropPool {
            contract_address: contract,
            pair_id,
            virtual_address: CaliberPropPool::virtual_address_from_pair_id(pair_id, contract),
            token_x: w,
            token_y: u,
            token_a: Token::new_with_decimals(u, 6),
            token_b: Token::new_with_decimals(w, 18),
            created_block: 0,
            last_synced_block: 0,
            reserve_a: U256::from(287_076u64), // cfg+5 @ 块末（token_a=U）
            reserve_b: "151028739041011564335".parse::<U256>().unwrap(), // cfg+4 @ 块末（token_b=W）
            ladder: Default::default(),
            price_a_in_b: 0.0,
            price_b_in_a: 0.0,
        };
        pool.ladder.ladder_a_to_b = Arc::new(vec![
            LadderPoint {
                amount_in: U256::from(10u64),
                amount_out: U256::from(200_000_000u64),
            },
            LadderPoint {
                amount_in: U256::from(50u64),
                amount_out: U256::from(900_000_000u64),
            },
            LadderPoint {
                amount_in: U256::from(300u64),
                amount_out: U256::from(1_000_000_000u64),
            },
        ]);
        pool.ladder.ladder_b_to_a = Arc::new((*pool.ladder.ladder_a_to_b).clone());
        pool.ladder.field0 = U256::from(219_438_865_054u64);
        pool.ladder.field1 = U256::from(296u64);
        pool.ladder.fee_rate = U256::from(200u64);
        pool.ladder.window = U256::from(500u64);
        pool.ladder.scale = U256::from(1_000_000_000_000u64);
        // 块末 cfg+7：low96=435976752（两笔正向 swap 累计），mid96=0
        pool.ladder.pos_forward = U256::from(435_976_752u64);
        pool.ladder.pos_reverse = U256::ZERO;
        pool.ladder.deadline = u64::MAX; // 永不过期
        pool.ladder.validity_window = 20;

        let out = pool
            .simulate_swap(w, u, U256::from(977_689_766_888_449_551u64))
            .expect("quote");
        assert_eq!(
            out,
            U256::from(287_076u64),
            "块末正向 quote 应与链上 cfg+5 封顶一致"
        );

        // 对照组：旧实现用快照总储备（reserve+consumed）封顶时，pos=0
        // 的无状态全量报价 214429979 不会因当前剩余储备封顶被截断——
        // 本地模拟路径已改为按当前剩余储备封顶，这里验证无状态报价本身。
        let total_reserve = U256::from(436_263_828u64); // 块起始 cfg+5
        let no_pos_total = quote_forward_exact(
            &pool.ladder.ladder_a_to_b,
            pool.ladder.field0,
            pool.ladder.field1,
            pool.ladder.fee_rate,
            pool.ladder.window,
            pool.ladder.scale,
            total_reserve,
            U256::from(977_689_766_888_449_551u64),
        );
        assert_eq!(
            no_pos_total,
            U256::from(214_429_979u64),
            "旧实现（忽略 low96 + 快照总储备封顶）的幻影报价"
        );
    }

    /// 实时 swap 事件应用：正向/反向储备与 pos 更新、方向切换归零、
    /// pairId 不匹配静默忽略。
    #[test]
    fn test_apply_chain_swap_updates_reserve_and_pos() {
        let contract = address!("0x154586B2479b9a11e3d4db90024Dc0e26F097312");
        let pair_id = b256!("b2d5c47f635aa119fc5e911aa881db33bc77b61a1872d035d6122869e24a9673");
        let w = address!("0xa8ddb5cd96b5222afe198316e9a57caa642850d5");
        let u = address!("0x779ded0c9e1022225f8e0630b35a9b54be713736");
        let mut pool = CaliberPropPool {
            contract_address: contract,
            pair_id,
            virtual_address: CaliberPropPool::virtual_address_from_pair_id(pair_id, contract),
            token_x: w,
            token_y: u,
            token_a: Token::new_with_decimals(u, 6),
            token_b: Token::new_with_decimals(w, 18),
            created_block: 0,
            last_synced_block: 0,
            reserve_a: U256::from(436_263_828u64), // 块起始 cfg+5（token_a=U）
            reserve_b: "149040856769846885495".parse::<U256>().unwrap(), // 块起始 cfg+4
            ladder: Default::default(),
            price_a_in_b: 0.0,
            price_b_in_a: 0.0,
        };
        pool.ladder.deadline = u64::MAX;
        pool.ladder.validity_window = 20;
        // 与事故块一致的手续费率（U→W 反向入账扣 200/1e6）
        pool.ladder.fee_rate = U256::from(200u64);

        // 块 67650064 tx#15：W→U ain=1900532488745085420 aout=416820481
        pool.apply_chain_swap(
            &CaliberSwapEvent {
                contract,
                tx_index: 15,
                pair_id,
                token_in: w,
                token_out: u,
                amount_in: U256::from(1_900_532_488_745_085_420u64),
                amount_out: U256::from(416_820_481u64),
            },
            67_650_064,
        );
        assert_eq!(
            pool.reserve_a,
            U256::from(19_443_347u64),
            "tx15 后 reserve_a(U)"
        );
        assert_eq!(
            pool.reserve_b,
            "149040856769846885495".parse::<U256>().unwrap()
                + U256::from(1_900_532_488_745_085_420u64),
            "tx15 后 reserve_b(W)"
        );
        assert_eq!(
            pool.ladder.pos_forward,
            U256::from(416_820_481u64),
            "tx15 后 low96 pos"
        );
        assert_eq!(pool.ladder.pos_reverse, U256::ZERO);
        assert_eq!(pool.last_synced_block, 67_650_064);

        // 块内 tx#22：同向 W→U，low96 累计（链上块末 = 435976752）
        pool.apply_chain_swap(
            &CaliberSwapEvent {
                contract,
                tx_index: 22,
                pair_id,
                token_in: w,
                token_out: u,
                amount_in: U256::from(87_349_782_419_593_420u64),
                amount_out: U256::from(19_156_271u64),
            },
            67_650_064,
        );
        assert_eq!(
            pool.ladder.pos_forward,
            U256::from(435_976_752u64),
            "tx22 后 low96 pos 累计"
        );
        assert_eq!(
            pool.reserve_a,
            U256::from(287_076u64),
            "tx22 后 reserve_a(U)"
        );

        // 反向 swap（U→W）：pos_reverse 累计"扣费后的 y 输入"
        // （amountIn - floor(amountIn * fee / 1e6)，链上 cfg+7 mid96 语义），
        // pos_forward 归零。
        pool.apply_chain_swap(
            &CaliberSwapEvent {
                contract,
                tx_index: 30,
                pair_id,
                token_in: u,
                token_out: w,
                amount_in: U256::from(1_000_000u64),
                amount_out: U256::from(500_000u64),
            },
            67_650_064,
        );
        assert_eq!(
            pool.ladder.pos_reverse,
            U256::from(999_800u64),
            "反向 pos_reverse = in - in*200/1e6"
        );
        assert_eq!(pool.ladder.pos_forward, U256::ZERO, "方向切换归零另一字段");
        assert_eq!(pool.reserve_a, U256::from(287_076u64 + 1_000_000u64));
        assert_eq!(
            pool.reserve_b,
            "149040856769846885495".parse::<U256>().unwrap()
                + U256::from(1_900_532_488_745_085_420u64)
                + U256::from(87_349_782_419_593_420u64)
                - U256::from(500_000u64)
        );

        // pairId 不匹配 → 静默忽略（fail-safe）
        let before = (pool.reserve_a, pool.reserve_b, pool.ladder.pos_forward);
        pool.apply_chain_swap(
            &CaliberSwapEvent {
                contract,
                tx_index: 40,
                pair_id: b256!("deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"),
                token_in: w,
                token_out: u,
                amount_in: U256::from(1u64),
                amount_out: U256::from(1u64),
            },
            67_650_064,
        );
        assert_eq!(
            (pool.reserve_a, pool.reserve_b, pool.ladder.pos_forward),
            before
        );
    }

    /// `ladder_input_for_output`（输入侧储备入账增量）语义验证（事故块
    /// 67650064 真实 ladder）：
    /// - 未受限（amount_out == quote(amount_in)）→ 全额返回 amount_in；
    /// - 受限（amount_out < quote(amount_in)，如 router minOut）→ 返回
    ///   "平台区上沿"（最大的 x 使 quote(x) == amount_out），与链上入账
    ///   逐位接近（取证 tx#22 链上入账 87,349,782,419,593,420，二分上沿
    ///   偏差 < 1e10 wei，远低于 dust，对账兜底）。
    #[test]
    fn test_apply_chain_swap_ladder_input_for_output() {
        let contract = address!("0x154586B2479b9a11e3d4db90024Dc0e26F097312");
        let pair_id = b256!("b2d5c47f635aa119fc5e911aa881db33bc77b61a1872d035d6122869e24a9673");
        let w = address!("0xa8ddb5cd96b5222afe198316e9a57caa642850d5");
        let u = address!("0x779ded0c9e1022225f8e0630b35a9b54be713736");
        let mut pool = CaliberPropPool {
            contract_address: contract,
            pair_id,
            virtual_address: CaliberPropPool::virtual_address_from_pair_id(pair_id, contract),
            token_x: w,
            token_y: u,
            token_a: Token::new_with_decimals(u, 6),
            token_b: Token::new_with_decimals(w, 18),
            created_block: 0,
            last_synced_block: 0,
            reserve_a: U256::ZERO,
            reserve_b: U256::ZERO,
            ladder: Default::default(),
            price_a_in_b: 0.0,
            price_b_in_a: 0.0,
        };
        pool.ladder.ladder_a_to_b = Arc::new(vec![
            LadderPoint {
                amount_in: U256::from(10u64),
                amount_out: U256::from(200_000_000u64),
            },
            LadderPoint {
                amount_in: U256::from(50u64),
                amount_out: U256::from(900_000_000u64),
            },
            LadderPoint {
                amount_in: U256::from(300u64),
                amount_out: U256::from(1_000_000_000u64),
            },
        ]);
        pool.ladder.ladder_b_to_a = Arc::new((*pool.ladder.ladder_a_to_b).clone());
        pool.ladder.field0 = U256::from(219_438_865_054u64);
        pool.ladder.field1 = U256::from(296u64);
        pool.ladder.fee_rate = U256::from(200u64);
        pool.ladder.window = U256::from(500u64);
        pool.ladder.scale = U256::from(1_000_000_000_000u64);
        pool.ladder.deadline = u64::MAX;
        pool.ladder.validity_window = 20;

        let mk_swap = |tin: Address, tout: Address, ain: u64, aout: u64| CaliberSwapEvent {
            contract,
            tx_index: 0,
            pair_id,
            token_in: tin,
            token_out: tout,
            amount_in: U256::from(ain),
            amount_out: U256::from(aout),
        };

        // 未受限正向：amount_out == quote(amount_in) → 全额入账 amountIn
        let ain = 977_689_766_888_449_551u64;
        let q = quote_forward_pos_exact(
            &pool.ladder.ladder_a_to_b,
            pool.ladder.field0,
            pool.ladder.field1,
            pool.ladder.fee_rate,
            pool.ladder.window,
            pool.ladder.scale,
            U256::ZERO,
            U256::MAX,
            U256::from(ain),
        );
        let input = pool.ladder_input_for_output(&mk_swap(w, u, ain, q.as_limbs()[0] as u64), true);
        assert_eq!(input, U256::from(ain), "未受限正向应全额入账");

        // 未受限反向：同上（pos_reverse=0）
        let rin = 1_000_000u64;
        let rq = quote_reverse_exact(
            &pool.ladder.ladder_a_to_b,
            pool.ladder.field0,
            pool.ladder.field1,
            pool.ladder.fee_rate,
            pool.ladder.window,
            pool.ladder.scale,
            U256::ZERO,
            U256::MAX,
            U256::from(rin),
        );
        let input = pool.ladder_input_for_output(&mk_swap(u, w, rin, rq.as_limbs()[0] as u64), false);
        assert_eq!(input, U256::from(rin), "未受限反向应全额入账");

        // 受限正向（块 67650064 tx#22）：pos=416820481、事件
        // in=526057675902332428、out=19156271，链上 cfg+4 仅入账
        // 87,349,782,419,593,420（产生该输出的 ladder 输入，超出部分
        // 停留合约余额不进储备）。二分上沿应与之接近。
        pool.ladder.pos_forward = U256::from(416_820_481u64);
        let chain_recorded = U256::from(87_349_782_419_593_420u64);
        let restricted = CaliberSwapEvent {
            contract,
            tx_index: 22,
            pair_id,
            token_in: w,
            token_out: u,
            amount_in: U256::from(526_057_675_902_332_428u64),
            amount_out: U256::from(19_156_271u64),
        };
        let input = pool.ladder_input_for_output(&restricted, true);
        assert!(
            input <= restricted.amount_in,
            "受限时入账量不得超过事件输入"
        );
        let diff = if input > chain_recorded {
            input - chain_recorded
        } else {
            chain_recorded - input
        };
        assert!(
            diff < U256::from(10_000_000_000u64),
            "受限入账应与链上记录接近（二分偏差 < 1e10 wei），got {input}, chain {chain_recorded}"
        );
        assert_eq!(
            quote_forward_pos_exact(
                &pool.ladder.ladder_a_to_b,
                pool.ladder.field0,
                pool.ladder.field1,
                pool.ladder.fee_rate,
                pool.ladder.window,
                pool.ladder.scale,
                U256::from(416_820_481u64),
                U256::MAX,
                input,
            ),
            restricted.amount_out,
            "入账量对应的报价应恰为事件输出"
        );
    }

    /// Swap 日志 ABI 解码（块 67650064 tx#15 真实日志：W→U）。
    #[test]
    fn test_decode_caliber_swap_log_real_event() {
        let topics = vec![
            CALIBER_SWAP_EVENT,
            b256!("b2d5c47f635aa119fc5e911aa881db33bc77b61a1872d035d6122869e24a9673"),
            b256!("000000000000000000000000311350ded40088b8504bb67a7d5974e9da287bd1"),
        ];
        let data = alloy::hex::decode(
            "000000000000000000000000a8ddb5cd96b5222afe198316e9a57caa642850d5000000000000000000000000779ded0c9e1022225f8e0630b35a9b54be7137360000000000000000000000000000000000000000000000001a600c3aa3bd69ec0000000000000000000000000000000000000000000000000000000018d82d010000000000000000000000000000000000000000000000000000000000000002",
        )
        .unwrap();
        let ev = decode_caliber_swap_log(&topics, &data).expect("decode");
        assert_eq!(ev.pair_id, topics[1]);
        assert_eq!(
            ev.token_in,
            address!("0xa8ddb5cd96b5222afe198316e9a57caa642850d5")
        );
        assert_eq!(
            ev.token_out,
            address!("0x779ded0c9e1022225f8e0630b35a9b54be713736")
        );
        assert_eq!(ev.amount_in, U256::from(1_900_532_488_745_085_420u64));
        assert_eq!(ev.amount_out, U256::from(416_820_481u64));

        // 错误签名 / 截断 data → None（fail-safe）
        let bad_topics = vec![B256::ZERO, topics[1]];
        assert_eq!(decode_caliber_swap_log(&bad_topics, &data), None);
        assert_eq!(decode_caliber_swap_log(&topics, &data[..64]), None);
        assert_eq!(decode_caliber_swap_log(&topics, &[]), None);
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

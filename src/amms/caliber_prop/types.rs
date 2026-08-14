use alloy::primitives::U256;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Ladder 定价曲线的一个段点
///
/// Caliber propAMM 使用分段定价模型。每个 LadderPoint 表示
/// 一个定价段的关键参数：`amount_in` 是该段的 x 定位参数（配合
/// field1/window 参与斜率计算），`amount_out` 是该段结束时的累计输出量。
///
/// 链上 `quote()` 通过段参数 + field0/field1/fee/window/scale 精确计算，
/// 不是简单的线性插值（详见 `docs/caliber_prop_internal.md`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LadderPoint {
    /// 段定位参数 x_i（原始量纲，未缩放）
    pub amount_in: U256,
    /// 该段结束时的累计输出量 y_i（原始量纲，未缩放）
    pub amount_out: U256,
}

/// 池子的 Ladder 快照 + 消费追踪 + 精确报价参数
///
/// `ladder_a_to_b` / `ladder_b_to_a` 均保存合约存储中的**同一份**
/// 原始 ladder（token_x → token_y 方向）。双向报价共用这份 ladder，
/// 通过 forward/reverse 两套公式 + `scale` 计算，而非对两个方向分别采样。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaliberLadderState {
    /// token_a → token_b 方向的定价阶梯（= 合约原始 ladder）
    /// Arc 共享：swap 模拟只读 ladder（仅 reserve 标量写回），Clone 退化为 O(1)。
    pub ladder_a_to_b: Arc<Vec<LadderPoint>>,
    /// token_b → token_a 方向的定价阶梯（= 合约原始 ladder）
    pub ladder_b_to_a: Arc<Vec<LadderPoint>>,
    /// a→b 方向已消费的输入量（区间累计）
    pub consumed_in_ab: U256,
    /// a→b 方向已消费的输出量（区间累计）
    pub consumed_out_ab: U256,
    /// b→a 方向已消费的输入量（区间累计）
    pub consumed_in_ba: U256,
    /// b→a 方向已消费的输出量（区间累计）
    pub consumed_out_ba: U256,
    /// 精确报价参数 field0（data 槽低 64 位）
    pub field0: U256,
    /// 精确报价参数 field1（data 槽 bits 64..96）
    pub field1: U256,
    /// 费率（1e6 基数，200 = 2 bps）
    pub fee_rate: U256,
    /// 最后一段之后的合成尾段长度（window）
    pub window: U256,
    /// scale = 10^(dec_token_x - dec_token_y)，用于链上 quote 公式
    pub scale: U256,
    /// 反向报价（token_y → token_x）的当前位置（链上 `cfg+7` bits 96..191
    /// = mid96，即该 pair 在 token_y → token_x 方向已累计的"扣费后输入量"
    /// amountIn_y - floor(amountIn_y * fee / 1e6)，2026-08-11 链上取证：
    /// 82,580,656 in → mid96=82,564,140 = in - in*200/1e6，与
    /// `quote_reverse_exact` 的 pos（y 单位）语义一致；不是累计输出量）。
    ///
    /// 链上合约只在 `cfg+7` 的 block 字段 == 当前执行块时才使用真实 pos，
    /// 否则按 pos=0（整段）计算；本字段在 `fetch_exact_snapshot` 中已完成
    /// 该判断（无效时为 0），`quote_reverse_exact` 直接使用。
    /// 旧字段名 `pos`（2026-08-09 前为反向 pos）通过 `#[serde(alias)]` 兼容。
    #[serde(alias = "pos", default)]
    pub pos_reverse: U256,
    /// 正向报价（token_x → token_y）的当前位置（链上 `cfg+7` bits 0..95
    /// = low96，即该 pair 在 token_x → token_y 方向已被累计兑换的输出量）。
    ///
    /// 与 `pos_reverse` 相同的 block 门控语义：仅当 `cfg+7.block` == 当前
    /// 执行块时有效，否则为 0。由快照路径与实时 swap 事件共同维护，
    /// `quote_forward_pos_exact` 直接使用。
    #[serde(default)]
    pub pos_forward: U256,
    /// 报价过期时间戳（完整 64 位 deadline = `(tsY << 32) | tsX`，data+0 槽
    /// bits 96..160）。`batchUpdateParameters` 更新写入时 tsY 置 0，故该路径下
    /// deadline 即更新交易的 deadline 参数；快照路径保留完整 64 位，tsY 非零时
    /// 链上永不过期，本地必须同样处理（不能只存 tsX）。
    /// 过期判定（链上语义）：`block.timestamp > deadline + validity_window` → revert。
    pub deadline: u64,
    /// 全局报价有效期 validity_window（合约 slot2，XLayer 本合约 = 20s）。
    ///
    /// 注意：这不是 `window`（per-pool cfg+3 合成尾段长度，本 pair 为 500s），
    /// 两者不能混用。实时更新 calldata 不含此值，只由快照路径刷新
    /// （协议低频修改，周期对账兜底）。
    #[serde(default)]
    pub validity_window: u64,
    /// 暂停态快照（全局 slot3 byte0 / per-pair cfg+6 byte@0x40 非零时暂停）。
    ///
    /// TODO(后续): 暂停类交易（`setPricingMode`/`setLocked`/`setWhitelistOnly`）
    /// 未纳入实时解析，暂停态只能由周期对账刷新（≤45s 滞后）；后续应在
    /// flashblocks 实时流中解析这些交易（与 `batchUpdateParameters` 同构）并
    /// 实时更新本字段，消除暂停滞后窗口。
    #[serde(default)]
    pub paused: bool,
}

impl CaliberLadderState {
    /// 链上 quote() 的不可报价判定（与 `pair_stale` 语义一致）：
    /// `block.timestamp > deadline + validity_window` 或暂停 → 上链 revert。
    /// 本地报价路径必须在每次报价时校验，避免已过期 pair 产生"幻影利润"上链回滚。
    pub fn is_unquotable(&self, now: u64) -> bool {
        self.paused || now > self.deadline.saturating_add(self.validity_window)
    }
}

/// Pool Extra 字段（序列化到 entity.Pool.Extra）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaliberExtra {
    /// 合约地址
    #[serde(rename = "a")]
    pub contract_address: String,
}

/// Pool StaticExtra 字段（序列化到 entity.Pool.StaticExtra）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaliberStaticExtra {
    /// 合约地址
    #[serde(rename = "a")]
    pub contract_address: String,
    /// token_a 地址
    #[serde(rename = "t0")]
    pub token_a: String,
    /// token_b 地址
    #[serde(rename = "t1")]
    pub token_b: String,
    /// token_a decimals
    #[serde(rename = "d0")]
    pub decimals_a: u8,
    /// token_b decimals
    #[serde(rename = "d1")]
    pub decimals_b: u8,
    /// 创建此池子时的区块号
    #[serde(rename = "b")]
    pub created_block: u64,
}

/// batchQuote 请求参数
#[derive(Debug, Clone)]
pub struct QuoteRequest {
    pub pair_id: [u8; 32],
    pub token_in: alloy::primitives::Address,
    pub token_out: alloy::primitives::Address,
    pub amount_in: U256,
}

/// batchQuote 返回结果
#[derive(Debug, Clone)]
pub struct QuoteResult {
    pub amount_out: U256,
    pub success: bool,
}

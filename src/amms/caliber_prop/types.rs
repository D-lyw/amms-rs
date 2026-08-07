use alloy::primitives::U256;
use serde::{Deserialize, Serialize};

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
    pub ladder_a_to_b: Vec<LadderPoint>,
    /// token_b → token_a 方向的定价阶梯（= 合约原始 ladder）
    pub ladder_b_to_a: Vec<LadderPoint>,
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
    /// 反向报价的当前位置（链上 `cfg+7` 的 pos 字段，bits 96..191）。
    ///
    /// 链上合约只在 `cfg+7` 的 block 字段 == 当前执行块时才使用真实 pos，
    /// 否则按 pos=0（整段）计算；本字段在 `fetch_exact_snapshot` 中已完成
    /// 该判断（无效时为 0），`quote_reverse_exact` 直接使用。
    pub pos: U256,
    /// 报价过期时间戳 tsX（data+0 槽 bits 96..128）。
    ///
    /// 与链上 `data+0` 对齐：`batchUpdateParameters` 的 deadline 参数写入此字段
    /// （同时 tsY 置 0），过期判断为 `block.timestamp > (tsY << 32 | tsX) + window`，
    /// tsY=0 时即退化为 `deadline + window`。实时交易更新与周期快照都会刷新该值。
    pub deadline: u64,
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

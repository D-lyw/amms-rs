use alloy::primitives::U256;
use serde::{Deserialize, Serialize};

/// Ladder 定价曲线的一个段点
///
/// Caliber propAMM 使用分段线性定价模型。每个 LadderPoint 表示
/// 从零开始的累计 AmountIn 和对应的累计 AmountOut。
///
/// 在两点之间的任意 AmountIn 通过线性插值计算 AmountOut。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LadderPoint {
    /// 累计输入量（从零开始）
    pub amount_in: U256,
    /// 累计输出量（从零开始）
    pub amount_out: U256,
}

/// 池子的 Ladder 快照 + 消费追踪
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaliberLadderState {
    /// token_a → token_b 方向的定价阶梯
    pub ladder_a_to_b: Vec<LadderPoint>,
    /// token_b → token_a 方向的定价阶梯
    pub ladder_b_to_a: Vec<LadderPoint>,
    /// a→b 方向已消费的输入量（区间累计）
    pub consumed_in_ab: U256,
    /// a→b 方向已消费的输出量（区间累计）
    pub consumed_out_ab: U256,
    /// b→a 方向已消费的输入量（区间累计）
    pub consumed_in_ba: U256,
    /// b→a 方向已消费的输出量（区间累计）
    pub consumed_out_ba: U256,
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

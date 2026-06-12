//! Curve NG 池类型定义

use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

/// Curve NG 池类型枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CurveNGPoolType {
    /// StableSwap-NG: 2-8 个锚定资产 (如稳定币、LSD)
    StableSwap,
    /// TwoCrypto-NG: 2 个波动资产
    TwoCrypto,
    /// TriCrypto-NG: 3 个波动资产
    TriCrypto,
}

impl CurveNGPoolType {
    /// 是否为 CryptoSwap 类型 (TwoCrypto 或 TriCrypto)
    pub fn is_crypto(&self) -> bool {
        matches!(self, Self::TwoCrypto | Self::TriCrypto)
    }

    /// 是否为 StableSwap 类型
    pub fn is_stable(&self) -> bool {
        matches!(self, Self::StableSwap)
    }
}

/// TwoCrypto 的实现分支
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum CurveNGTwoCryptoVariant {
    /// 标准 twocrypto-ng v2.1.0 路径
    #[default]
    StandardV210,
    /// v2.1.0d periphery 路径 (TwocryptoView + StableswapMath)
    /// 典型于 YieldBasis 这组特殊池（见 docs.yieldbasis.com）
    PeripheryV210d,
}

/// Curve NG 索引参数签名类型（用于 coins/balances/get_dy）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum CurveIndexSignature {
    #[default]
    Unknown,
    Uint256,
    Int128,
}

/// Curve NG 池状态
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurveNGPool {
    /// 池子地址
    pub address: Address,
    /// 最后同步区块
    pub last_synced_block: u64,
    /// 池子类型
    pub pool_type: CurveNGPoolType,
    /// 代币数量
    pub n_coins: u8,
    /// 代币地址列表
    pub coins: Vec<Address>,
    /// 各代币余额
    pub balances: Vec<U256>,
    /// 各代币精度
    pub decimals: Vec<u8>,
    /// 费率/汇率 (用于 rate provider, 如 wstETH)
    pub rates: Vec<U256>,

    // === StableSwap 参数 ===
    /// 放大系数 A (StableSwap)
    pub amp: Option<U256>,
    /// 手续费 (1e10 = 100%)
    pub fee: U256,
    /// 管理费分成
    /// 管理费分成
    pub admin_fee: U256,
    /// 动态费率乘数 (Off-peg fee multiplier)
    pub offpeg_fee_multiplier: U256,
    /// Ramp A: initial precise A (includes A_PRECISION)
    pub initial_a_precise: Option<U256>,
    /// Ramp A: future precise A (includes A_PRECISION)
    pub future_a_precise: Option<U256>,
    /// Ramp A: initial timestamp
    pub initial_a_time: Option<U256>,
    /// Ramp A: future timestamp
    pub future_a_time: Option<U256>,
    /// 是否支持 stored_rates()（初始化能力探测结果）
    #[serde(default = "default_true")]
    pub supports_stored_rates: bool,
    /// 是否支持 offpeg_fee_multiplier()（初始化能力探测结果）
    #[serde(default = "default_true")]
    pub supports_offpeg_fee_multiplier: bool,
    /// coins() 索引签名
    #[serde(default)]
    pub coins_index_signature: CurveIndexSignature,
    /// balances() 索引签名
    #[serde(default)]
    pub balances_index_signature: CurveIndexSignature,
    /// get_dy() 索引签名
    #[serde(default)]
    pub get_dy_index_signature: CurveIndexSignature,
    /// 能力模型版本号，0 表示旧快照未探测
    #[serde(default)]
    pub capability_version: u8,

    // === CryptoSwap 额外参数 ===
    /// 价格缩放因子 (CryptoSwap)
    pub price_scale: Option<Vec<U256>>,
    /// 内部预言机价格 (CryptoSwap)
    pub price_oracle: Option<Vec<U256>>,
    /// 最后交易价格 (CryptoSwap)
    pub last_prices: Option<Vec<U256>>,
    /// D 不变量缓存 (CryptoSwap)
    pub d: Option<U256>,
    /// gamma 参数 (CryptoSwap)
    pub gamma: Option<U256>,
    /// 虚拟价格
    pub virtual_price: Option<U256>,

    // === CryptoSwap 动态费率参数 ===
    /// 中间费率
    pub mid_fee: Option<U256>,
    /// 外部费率
    pub out_fee: Option<U256>,
    /// 费用 Gamma
    pub fee_gamma: Option<U256>,
    /// 允许的额外利润
    pub allowed_extra_profit: Option<U256>,
    /// 调整步长
    pub adjustment_step: Option<U256>,
    /// 移动平均半衰期
    pub ma_half_time: Option<U256>,

    // === TwoCrypto 变体识别/参数 ===
    /// TwoCrypto 的具体实现分支
    pub twocrypto_variant: CurveNGTwoCryptoVariant,
    /// TwoCrypto periphery VIEW 地址（仅 v2.1.0d 使用）
    pub twocrypto_view: Option<Address>,
    /// TwoCrypto periphery MATH 地址（仅 v2.1.0d 使用）
    pub twocrypto_math: Option<Address>,
    /// TwoCrypto 合约版本字符串
    pub twocrypto_version: Option<String>,
    /// TwoCrypto precisions() 返回值（优先于 decimals 推导）
    pub twocrypto_precisions: Option<Vec<U256>>,
    /// TwoCrypto last_timestamp（用于 _calc_D_ramp 判定）
    pub twocrypto_last_timestamp: Option<U256>,
    /// TwoCrypto future_A_gamma_time（用于 _calc_D_ramp 判定）
    pub twocrypto_future_a_gamma_time: Option<U256>,

    /// 缓存的现货价格 (base_token, quote_token) -> price
    #[serde(skip)]
    pub spot_prices: std::collections::HashMap<(Address, Address), f64>,
}

impl CurveNGPool {
    /// 创建新的空池子实例
    pub fn new(address: Address, pool_type: CurveNGPoolType) -> Self {
        Self {
            address,
            last_synced_block: 0,
            pool_type,
            n_coins: 0,
            coins: Vec::new(),
            balances: Vec::new(),
            decimals: Vec::new(),
            rates: Vec::new(),
            amp: None,
            fee: U256::ZERO,
            admin_fee: U256::ZERO,
            offpeg_fee_multiplier: U256::ZERO,
            initial_a_precise: None,
            future_a_precise: None,
            initial_a_time: None,
            future_a_time: None,
            supports_stored_rates: true,
            supports_offpeg_fee_multiplier: true,
            coins_index_signature: CurveIndexSignature::Unknown,
            balances_index_signature: CurveIndexSignature::Unknown,
            get_dy_index_signature: CurveIndexSignature::Unknown,
            capability_version: 0,
            price_scale: None,
            price_oracle: None,
            last_prices: None,
            d: None,
            gamma: None,
            virtual_price: None,
            mid_fee: None,
            out_fee: None,
            fee_gamma: None,
            allowed_extra_profit: None,
            adjustment_step: None,
            ma_half_time: None,
            twocrypto_variant: CurveNGTwoCryptoVariant::StandardV210,
            twocrypto_view: None,
            twocrypto_math: None,
            twocrypto_version: None,
            twocrypto_precisions: None,
            twocrypto_last_timestamp: None,
            twocrypto_future_a_gamma_time: None,
            spot_prices: std::collections::HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn test_new_pool_capability_defaults() {
        let pool = CurveNGPool::new(
            address!("0000000000000000000000000000000000000001"),
            CurveNGPoolType::StableSwap,
        );

        assert!(pool.supports_stored_rates);
        assert!(pool.supports_offpeg_fee_multiplier);
        assert_eq!(pool.coins_index_signature, CurveIndexSignature::Unknown);
        assert_eq!(pool.balances_index_signature, CurveIndexSignature::Unknown);
        assert_eq!(pool.get_dy_index_signature, CurveIndexSignature::Unknown);
        assert_eq!(pool.capability_version, 0);
    }
}

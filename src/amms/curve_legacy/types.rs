//! Curve Legacy 池类型定义

use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Curve Legacy 池类型枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum CurveLegacyPoolType {
    /// StableSwap: 锚定资产池 (3pool, stETH/ETH 等)
    #[default]
    StableSwap,
    /// CryptoSwap V2: 波动资产池 (tricrypto2 等)
    CryptoSwap,
}

/// 已知的 Legacy 池配置
#[derive(Debug, Clone)]
pub struct KnownLegacyPool {
    pub address: Address,
    pub name: &'static str,
    pub pool_type: CurveLegacyPoolType,
    pub n_coins: u8,
}

/// Legacy StableSwap 子类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum LegacyStableSwapType {
    #[default]
    Plain,
    Lending,
    Meta,
}

/// Curve Legacy Base Pool 的只读共享视图
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CurveLegacyBaseView {
    pub address: Address,
    pub last_synced_block: u64,
    pub pool_type: CurveLegacyPoolType,
    pub stable_type: LegacyStableSwapType,
    pub n_coins: u8,
    pub coins: Vec<Address>,
    pub balances: Vec<U256>,
    pub decimals: Vec<u8>,
    pub rates: Vec<U256>,
    pub amp: Option<U256>,
    pub uses_a_precision: bool,
    pub fee: U256,
    pub admin_fee: U256,
    pub total_supply: U256,
}

/// Curve Legacy 兑换路径解析结果
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CurveLegacySwapRoute {
    Direct { i: usize, j: usize },
    BaseToBase { i: usize, j: usize },
    MetaToBase { meta_i: usize, base_j: usize },
    BaseToMeta { base_i: usize, meta_j: usize },
}

/// Curve Legacy 池状态
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CurveLegacyPool {
    /// 池子地址
    pub address: Address,
    /// 最后同步区块
    pub last_synced_block: u64,
    /// 池子类型 (Stable / Crypto)
    pub pool_type: CurveLegacyPoolType,
    /// StableSwap 子类型 (Plain / Lending / Meta)
    pub stable_type: LegacyStableSwapType,
    /// 是否为 Meta Pool
    pub is_meta: bool,

    /// 代币数量
    pub n_coins: u8,
    /// 代币地址列表
    pub coins: Vec<Address>,
    /// Underlying Coins。对于 Meta Pool，其顺序必须与 exchange_underlying(i, j) 一致。
    pub underlying_coins: Vec<Address>,

    /// 各代币余额
    pub balances: Vec<U256>,
    /// 各代币精度
    pub decimals: Vec<u8>,
    /// 费率 (1e18 scale, used for lending pools)
    pub rates: Vec<U256>,

    // === StableSwap 参数 ===
    /// 放大系数 A
    pub amp: Option<U256>,
    /// 是否使用 A_PRECISION (新版 Vyper 0.3.x 池子使用 100，旧版 0.2.x 不使用)
    pub uses_a_precision: bool,
    /// 手续费 (1e10 = 100%)
    pub fee: U256,
    /// 管理费分成
    pub admin_fee: U256,

    // === Metapool 参数 ===
    /// 基础池地址
    pub base_pool_address: Option<Address>,
    /// Base Pool LP Token 地址 / placeholder 对应 token
    pub base_lp_token: Option<Address>,
    /// 基础池虚拟价格 (1e18 scale)
    pub base_virtual_price: Option<U256>,
    /// 池子 LP 总供应量
    pub total_supply: Option<U256>,
    /// Base Pool 代币数量
    pub base_n_coins: u8,
    /// Metapool 中 LP Token 的索引
    pub base_token_index: Option<usize>,
    /// Meta Pool 内部使用的 Base Pool 只读视图
    #[serde(skip)]
    pub base_pool_view: Option<Arc<CurveLegacyBaseView>>,

    // === CryptoSwap 额外参数 ===
    /// 价格缩放因子
    pub price_scale: Option<Vec<U256>>,
    /// D 不变量缓存
    pub d: Option<U256>,
    /// gamma 参数
    pub gamma: Option<U256>,

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

    /// 缓存的现货价格 (base_token, quote_token) -> price
    #[serde(skip)]
    pub spot_prices: std::collections::HashMap<(Address, Address), f64>,
}

impl CurveLegacyPool {
    /// 创建新的空池子实例
    pub fn new(address: Address, pool_type: CurveLegacyPoolType) -> Self {
        Self {
            address,
            last_synced_block: 0,
            pool_type,
            stable_type: LegacyStableSwapType::Plain,
            is_meta: false,
            n_coins: 0,
            coins: Vec::new(),
            underlying_coins: Vec::new(),
            balances: Vec::new(),
            decimals: Vec::new(),
            rates: Vec::new(),
            amp: None,
            uses_a_precision: false, // 默认为旧版，初始化时会检测
            fee: U256::ZERO,
            admin_fee: U256::ZERO,

            base_pool_address: None,
            base_lp_token: None,
            base_virtual_price: None,
            total_supply: None,
            base_n_coins: 0,
            base_token_index: None,
            base_pool_view: None,

            price_scale: None,
            d: None,
            gamma: None,
            mid_fee: None,
            out_fee: None,
            fee_gamma: None,
            allowed_extra_profit: None,
            adjustment_step: None,
            ma_half_time: None,
            spot_prices: std::collections::HashMap::new(),
        }
    }
}

impl CurveLegacyBaseView {
    /// Materialize the embedded base-pool snapshot into a first-class CurveLegacyPool.
    ///
    /// This lets MetaPool dependencies participate in the normal StateSpace lifecycle:
    /// log syncing, async refreshes, drift probing, and maintenance resyncs.
    pub fn to_curve_legacy_pool(&self) -> CurveLegacyPool {
        CurveLegacyPool {
            address: self.address,
            last_synced_block: self.last_synced_block,
            pool_type: self.pool_type,
            stable_type: self.stable_type,
            is_meta: false,
            n_coins: self.n_coins,
            coins: self.coins.clone(),
            underlying_coins: Vec::new(),
            balances: self.balances.clone(),
            decimals: self.decimals.clone(),
            rates: self.rates.clone(),
            amp: self.amp,
            uses_a_precision: self.uses_a_precision,
            fee: self.fee,
            admin_fee: self.admin_fee,
            base_pool_address: None,
            base_lp_token: None,
            base_virtual_price: None,
            total_supply: Some(self.total_supply),
            base_n_coins: 0,
            base_token_index: None,
            base_pool_view: None,
            price_scale: None,
            d: None,
            gamma: None,
            mid_fee: None,
            out_fee: None,
            fee_gamma: None,
            allowed_extra_profit: None,
            adjustment_step: None,
            ma_half_time: None,
            spot_prices: HashMap::new(),
        }
    }
}

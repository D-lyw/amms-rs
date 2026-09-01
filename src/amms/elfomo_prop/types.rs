//! ElfomoFi propAMM 类型定义：合约 ABI 与 orderbook 档位结构。

use alloy::primitives::U256;
use alloy::sol;
use serde::{Deserialize, Serialize};

// ============================================================================
// Contract ABI（XLayer 实测：Router / Factory / Pool）
// ============================================================================

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IElfomoFiRouter {
        function getSupportedPairs() external view returns (address[2][] memory pairs);

        function getAmountOut(
            address fromToken,
            address toToken,
            uint256 fromAmount
        ) external view returns (uint256 toAmount);

        function getAmountIn(
            address fromToken,
            address toToken,
            uint256 toAmount
        ) external view returns (uint256 fromAmount);

        function swap(
            address fromToken,
            address toToken,
            int256 specifiedAmount,
            uint256 limitAmount,
            address receiver,
            uint256 partnerId
        ) external returns (uint256);
    }
}

sol! {
    #[allow(missing_docs)]
    #[derive(Debug, PartialEq, Eq)]
    struct ElfomoOrderbookLevel {
        uint256 size;
        uint256 price;
    }

    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IElfomoFiFactory {
        /// 返回两个 (size, price) 数组：
        /// - `fromToLevels`：fromToken→toToken 方向（size = 输入量）
        /// - `toFromLevels`：toToken→fromToken 方向（size = 输出量）
        function getOrderbook(
            address fromToken,
            address toToken
        )
            external
            view
            returns (
                ElfomoOrderbookLevel[] memory fromToLevels,
                ElfomoOrderbookLevel[] memory toFromLevels
            );
    }
}

// ============================================================================
// 数据结构
// ============================================================================

/// 单档 orderbook 档位（size = 档位容量，price = 1e24 定点价格）。
///
/// 语义（链上逐位对拍锁定，见 docs/2026-09-01_elfomo_prop_xlayer_research.md §3.1）：
/// - `from→to`（arr0）：size = 该档最大**输入**量，
///   输出 = `floor(take × price / 1e24)`，`take = min(剩余输入, size)`。
/// - `to→from`（arr1）：size = 该档最大**输出**量，
///   所需输入 = `ceil(size × price / 1e24)`。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrderbookLevel {
    pub size: U256,
    pub price: U256,
}

impl OrderbookLevel {
    pub const fn new(size: U256, price: U256) -> Self {
        Self { size, price }
    }
}

/// 订单簿快照：两侧档位 + 金库余额背书 + 价格种子。
///
/// `price_seed` 是 Pool slot1 高 32 位（`a`），orderbook 是
/// `(price_seed, vault_usdt0, vault_xeth)` 的**读时纯函数**（见
/// `ElfomoFiPropPool::build_orderbook`）。档位字段仅为缓存/对拍，
/// 本地报价一律按种子+金库余额实时重算，保证与链上一致。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrderbookSnapshot {
    /// fromToken→toToken 方向档位（size = 输入量）
    pub from_to_levels: Vec<OrderbookLevel>,
    /// toToken→fromToken 方向档位（size = 输出量）
    pub to_from_levels: Vec<OrderbookLevel>,
    /// 金库 USDT0 余额（正向输出封顶）
    pub vault_usdt0: U256,
    /// 金库 xETH 余额（反向输出封顶；s1+s2+s3 == 此值）
    pub vault_xeth: U256,
    /// 价格种子 `a`（Pool slot1 >> 32；updatePrices calldata 直接携带）
    #[serde(default)]
    pub price_seed: U256,
}

/// 本地档位消耗状态（随 ElfomoTrade 事件消耗，L2 快照整档回正）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LevelConsumed {
    /// 每档已消耗量（语义与该侧 size 一致：正向 = 输入量，反向 = 输出量）
    pub from_to: Vec<U256>,
    pub to_from: Vec<U256>,
}

impl LevelConsumed {
    pub fn new(n_from_to: usize, n_to_from: usize) -> Self {
        Self {
            from_to: vec![U256::ZERO; n_from_to],
            to_from: vec![U256::ZERO; n_to_from],
        }
    }
}

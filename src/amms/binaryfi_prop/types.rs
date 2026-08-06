//! BinaryFi propAMM 类型定义：合约 ABI、批量快照结构、费率分数。

use alloy::primitives::U256;
use alloy::sol;
use serde::{Deserialize, Serialize};

// ============================================================================
// Contract ABI
// ============================================================================

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IBinaryFiPropPool {
        function getAssets() external view returns (address[] memory);

        function quote(
            address recipient,
            address tokenIn,
            address tokenOut,
            uint256 amountIn
        ) external view returns (uint256 amountOut);
    }
}

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IBinaryFiEngine {
        function getAssetReserves()
            external
            view
            returns (address[] memory assets, uint256[] memory reserves);
    }
}

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    GetBinaryFiPropStateBatchRequest,
    "src/amms/abi/GetBinaryFiPropStateBatchRequest.json",
}

sol! {
    #[allow(missing_docs)]
    #[derive(Debug, PartialEq)]
    struct QuoteResult {
        uint256 amountOut;
        bool success;
    }
    #[allow(missing_docs)]
    #[derive(Debug, PartialEq)]
    struct Snapshot {
        address[] assets;
        uint8[] decimals;
        uint256[] scales;
        uint256[] poolBalances;
        uint256[] vaultReserves;
        uint256[] quotePairs;
        QuoteResult[] quotes;
    }
}

// ============================================================================
// 数据结构
// ============================================================================

/// 定向线性费率（num/den 分数）
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rate {
    pub num: U256,
    pub den: U256,
}

impl Rate {
    pub const fn zero() -> Self {
        Self {
            num: U256::ZERO,
            den: U256::ZERO,
        }
    }

    pub fn is_zero(&self) -> bool {
        self.num.is_zero() || self.den.is_zero()
    }
}

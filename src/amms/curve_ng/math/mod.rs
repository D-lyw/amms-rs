//! Curve NG 数学计算模块
//!
//! 包含两种核心算法：
//! - `stableswap`: StableSwap 不变量 (用于锚定资产)
//! - `cryptoswap`: CryptoSwap 不变量 (用于波动资产, 通用 N-coin)

pub mod cryptoswap;
pub mod stableswap;
pub mod twocrypto_v210d;

//! Fee-on-transfer (FoT) token 支持
//!
//! # 扣税语义（链上取证确认：XLS token，XLayer 交易 0xf90b9c...）
//!
//! 扣税发生在 **swap 之后、token transfer hook 内部**，且 **只在 from = pair（池子）
//! 转出时扣税**（即用户从池子买入该 token 时）：
//!   - from = pool（买）：池子 swap math 输出 gross，transfer hook 扣税，
//!     接收方到手 net = gross × (10000 - fee_bps) / 10000（向下取整）
//!   - to = pool（卖）：**不扣税**，池子收到全额（Swap.amountIn / reserve 增量均为全额，
//!     已由 0x3d49 池收到 636752046480362035065 全额验证）
//!   - EOA → EOA：**不扣税**（fork 实测 0.1 XLS 全额到账）
//!   - 合约 → pool（如执行合约偿还 flash loan）：**不扣税**（t1 中竞争者合约
//!     0x0223... 转 XLS 给池子全额到账）
//!
//! 因此池子 reserve / Sync 事件保持 **gross 口径**，模拟时：
//!   - 输入侧（in_token 是 FoT）：金额全额入池，不做扣税处理
//!   - 输出侧（out_token 是 FoT）：swap math 输出 gross，返回给调用方的是扣税后 net
//!
//! # 注册方式
//!
//! FoT 参数 **不会自动检测**，必须在初始化阶段通过 [`register_fot_token`]
//! 显式注册（或直接在 Token 序列化数据中标注 `fot_tax` 字段）。
//! 优先级：Token 上显式标注的 `fot_tax` > registry 运行时注入。
//!
//! # 注意
//!
//! 当前 [`FotTaxType::FlatRate`] 表示"池子转出时扣税"（单侧），**不**表示每次
//! transfer 都扣税。若未来遇到无条件扣税（买卖同税率）的 token，需新增独立变体
//! 并同步实现输入侧扣税逻辑，不能复用本变体。

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};

use super::Token;

/// FoT 扣税类型
///
/// 不同的扣税 token 逻辑差异很大（买卖分离税率、仅卖出扣税、反射分红等），
/// 注册时必须指定具体类型，方便后续扩展支持多种扣税逻辑。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FotTaxType {
    /// 池子转出时扣税（输出侧 FoT，单侧税）
    ///
    /// 扣税只发生在 **from = pair（池子）转出该 token 时**（用户从池子买入），
    /// 即 swap math 输出 gross 后、transfer hook 内扣税，接收方到手
    /// `net = gross - floor(gross × fee_bps / 10000)`。
    ///
    /// **不**表示每次 transfer 都扣税：to = pool（卖出）、EOA→EOA、
    /// 合约→pool 均不扣税（链上取证确认，见模块文档）。
    /// 若未来遇到无条件扣税（买卖同税率每次 transfer 都扣）的 token，
    /// 需新增独立变体并实现输入侧扣税逻辑，不能复用本变体。
    ///
    /// `fee_bps` 以 basis points 计（1 bps = 0.01%），例如 3% 税 = 300。
    FlatRate {
        fee_bps: u64,
    },
    // 未来扩展（示例，变体命名用 snake_case 与 serde rename_all 保持一致）：
    // /// 买入/卖出分离税率
    // BuySell { buy_fee_bps: u64, sell_fee_bps: u64 },
    // /// 仅卖出扣税
    // SellOnly { sell_fee_bps: u64 },
    // /// 反射型：每笔 transfer 按比例向持有者分红
    // Reflection { fee_bps: u64 },
}

impl FotTaxType {
    /// 税率分母（万分之一）
    pub const BASIS: u64 = 10_000;

    /// 该税种下，gross 金额实际到手 net
    ///
    /// 链上语义（取证确认）：合约先算 `tax = floor(gross × fee_bps / 10000)`，
    /// 再转出 `net = gross - tax`。等价于 `ceil(gross × (10000-fee) / 10000)`，
    /// 与直接 `floor(gross × (10000-fee) / 10000)` 可能差 1 wei。
    pub fn net_of_gross(&self, gross: U256) -> U256 {
        match self {
            FotTaxType::FlatRate { fee_bps } => {
                if *fee_bps >= Self::BASIS {
                    return U256::ZERO;
                }
                let tax = gross * U256::from(*fee_bps) / U256::from(Self::BASIS);
                gross - tax
            }
        }
    }

    /// 反算：接收方到手 net 所需的最小 gross 金额
    ///
    /// 满足 `net_of_gross(gross_up(net)) >= net`，且是满足该条件的最小值：
    /// `gross_up(net) = floor((net-1) × 10000 / (10000 - fee_bps)) + 1`
    /// （net = 0 时返回 0）。
    ///
    /// 用于 exact-out 场景：池子 math 必须先输出 gross，transfer 扣税后
    /// 接收方才能拿到 net，因此 amount_out 需先 gross-up 再进 `get_amount_in`。
    pub fn gross_up(&self, net: U256) -> U256 {
        match self {
            FotTaxType::FlatRate { fee_bps } => {
                if *fee_bps >= Self::BASIS || net.is_zero() {
                    return U256::ZERO;
                }
                let numerator = (net - U256::from(1u8)) * U256::from(Self::BASIS);
                let denominator = U256::from(Self::BASIS - *fee_bps);
                numerator / denominator + U256::from(1u8)
            }
        }
    }
}

/// 全局 FoT token 注册表（初始化阶段写入，运行期只读）
static FOT_REGISTRY: OnceLock<RwLock<HashMap<Address, FotTaxType>>> = OnceLock::new();

fn registry() -> &'static RwLock<HashMap<Address, FotTaxType>> {
    FOT_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 注册一个 FoT token（**必须在初始化阶段调用**，amms 不会自动检测）
pub fn register_fot_token(token: Address, tax: FotTaxType) {
    registry().write().unwrap().insert(token, tax);
}

/// 查询 token 的 FoT 税率（未注册返回 `None`）
pub fn fot_tax(token: Address) -> Option<FotTaxType> {
    registry().read().unwrap().get(&token).copied()
}

/// 初始化阶段应用到 Token：若已注册，注入 `fot_tax` 字段
///
/// 若 Token 上已有显式标注的 `fot_tax`（如反序列化数据），则保持不变。
pub fn apply_to_token(token: &mut Token) {
    if token.fot_tax.is_none() {
        if let Some(tax) = fot_tax(token.address) {
            token.fot_tax = Some(tax);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::uint;

    // 链上取证精确值（XLS token，3% 税，交易 0xf90b9c...）：
    //   gross 656445408742641273262 → net 636752046480362035065
    const GROSS: U256 = uint!(656445408742641273262_U256);
    const NET: U256 = uint!(636752046480362035065_U256);

    #[test]
    fn test_flat_rate_net_matches_onchain_forensics() {
        let tax = FotTaxType::FlatRate { fee_bps: 300 };
        assert_eq!(tax.net_of_gross(GROSS), NET);
        // 链上语义：tax = floor(gross×fee/10000)，与 floor(gross×9700/10000) 可能差 1
        let floor_direct = GROSS * U256::from(9700u16) / U256::from(10000u16);
        assert_eq!(NET, floor_direct + U256::from(1u8));
    }

    #[test]
    fn test_flat_rate_gross_up_inverse() {
        let tax = FotTaxType::FlatRate { fee_bps: 300 };
        // net -> gross_up -> net 应回到原值
        let gross = tax.gross_up(NET);
        assert_eq!(tax.net_of_gross(gross), NET);
        // 且是最小解：gross-1 已不够
        assert!(tax.net_of_gross(gross - U256::from(1u8)) < NET);
    }

    #[test]
    fn test_flat_rate_gross_up_minimal() {
        let tax = FotTaxType::FlatRate { fee_bps: 300 };
        // net=97: gross=99 即满足（net_of_gross(99)=99-floor(2.97)=97），100 不是最小
        let gross = tax.gross_up(U256::from(97u8));
        assert_eq!(gross, U256::from(99u8));
        assert_eq!(tax.net_of_gross(gross), U256::from(97u8));
        assert_eq!(tax.net_of_gross(U256::from(98u8)), U256::from(96u8));
        // net=0 -> 0
        assert_eq!(tax.gross_up(U256::ZERO), U256::ZERO);
    }

    #[test]
    fn test_flat_rate_full_fee_returns_zero() {
        let tax = FotTaxType::FlatRate { fee_bps: 10_000 };
        assert_eq!(tax.net_of_gross(GROSS), U256::ZERO);
        assert_eq!(tax.gross_up(GROSS), U256::ZERO);
    }

    #[test]
    fn test_registry_roundtrip() {
        let token = Address::repeat_byte(0xab);
        register_fot_token(token, FotTaxType::FlatRate { fee_bps: 300 });
        assert_eq!(fot_tax(token), Some(FotTaxType::FlatRate { fee_bps: 300 }));

        let mut t = Token::new_with_decimals(token, 18);
        apply_to_token(&mut t);
        assert_eq!(t.fot_tax, Some(FotTaxType::FlatRate { fee_bps: 300 }));

        // 已有显式标注时 registry 不覆盖
        let mut t2 = Token {
            fot_tax: Some(FotTaxType::FlatRate { fee_bps: 500 }),
            ..Token::new_with_decimals(token, 18)
        };
        apply_to_token(&mut t2);
        assert_eq!(t2.fot_tax, Some(FotTaxType::FlatRate { fee_bps: 500 }));
    }

    #[test]
    fn test_serde_roundtrip() {
        let tax = FotTaxType::FlatRate { fee_bps: 300 };
        let json = serde_json::to_string(&tax).unwrap();
        assert_eq!(json, r#"{"type":"flat_rate","fee_bps":300}"#);
        let back: FotTaxType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, tax);
    }
}

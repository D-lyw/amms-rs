use alloy::primitives::U256;
use serde::{Deserialize, Serialize};

/// V4 hook 收费建模（白名单制，只建模影响用户本地 swap 模拟结果的逻辑）。
///
/// 设计约束（对应 v4-core 记账语义）：
/// - 池子核心做市状态（sqrtPrice / tick / liquidity）一律按“无 hook 的核心
///   swap”演化；本模型只调整用户侧的净得 / 实付。
/// - 一个 hook 是否被支持由上层（索引 / 池加载）按地址白名单判定：白名单内且
///   完成本模型参数装配的池子才能进入模拟；未建模的带 hook 池子必须被过滤，
///   否则本地模拟会漏掉 hook 对 swap 结果的影响。
///
/// 当前建模类别：
/// - [`AfterSwapProportional`]：afterSwap 按核心 swap“非指定侧”毛额收取比例
///   费用（Flaunch 式 Internal Swap Pool / PonsV2MemeHook 类）。
///   对齐证据：v4-core `Hooks.afterSwap` 将 hook 返回 delta 记到 hook 地址并
///   从 caller delta 中扣除（`swapDelta = coreDelta - hookDelta`），因此：
///   - exact-input：用户净得 = 毛输出 − Σ floor(毛输出 × c_i / 10000)
///   - exact-output：用户实付 = 毛输入 + Σ floor(毛输入 × c_i / 10000)
///   费率按“组件”表达而不是单值：PonsV2MemeHook 在链上把 hookFeeBps 与
///   creatorTaxBps 分开各自向下取整后再相加（feeAmount / taxAmount 分开
///   计提），合并成一个 bps 单次截断会在进位时差 1 wei；逐组件截断与其
///   链上舍入语义完全一致（已由真实主网 fork 对照测试验证）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum V4HookFee {
    #[default]
    /// 无 hook 或白名单内已审计为“不改变 swap 结果”的 hook。
    None,
    /// afterSwap 比例收费。
    /// 语义以 pool 的 zeroForOne 方向表达：zero_for_one_bps 是用户支付
    /// currency0（swap currency0→currency1）时的费率组件集合。
    AfterSwapProportional {
        /// 费率组件（bps）列表：zeroForOne 方向（用户以 currency0 换 currency1）。
        zero_for_one_bps: Vec<u16>,
        /// 费率组件（bps）列表：oneForZero 方向（用户以 currency1 换 currency0）。
        one_for_zero_bps: Vec<u16>,
    },
}

impl V4HookFee {
    /// 指定方向的费率组件切片（空 = 该方向不收费）。
    #[inline]
    pub fn components(&self, zero_for_one: bool) -> &[u16] {
        match self {
            V4HookFee::None => &[],
            V4HookFee::AfterSwapProportional {
                zero_for_one_bps,
                one_for_zero_bps,
            } => {
                if zero_for_one {
                    zero_for_one_bps
                } else {
                    one_for_zero_bps
                }
            }
        }
    }

    /// 指定方向合计费率 bps（0 表示该方向不收费）。
    #[inline]
    pub fn rate_bps(&self, zero_for_one: bool) -> u32 {
        self.components(zero_for_one)
            .iter()
            .map(|c| *c as u32)
            .sum()
    }

    #[inline]
    pub fn is_none(&self) -> bool {
        matches!(self, V4HookFee::None)
    }

    /// 是否需要在本地模拟中调整用户侧金额。
    #[inline]
    pub fn applies(&self, zero_for_one: bool) -> bool {
        !self.components(zero_for_one).is_empty()
    }

    /// 按组件逐个向下取整求和：Σ floor(gross × c_i / 10000)，与链上
    /// feeAmount / taxAmount 分开计提后的总扣费完全一致。
    #[inline]
    pub fn total_fee(&self, zero_for_one: bool, gross: U256) -> U256 {
        let mut total = U256::ZERO;
        for c in self.components(zero_for_one) {
            if *c != 0 && !gross.is_zero() {
                total += gross * U256::from(*c) / U256::from(10_000u64);
            }
        }
        total
    }

    /// exact-input：毛输出 → 用户净得（毛输出上扣除各组件费用，向下取整）。
    #[inline]
    pub fn apply_exact_in(&self, zero_for_one: bool, gross_out: U256) -> U256 {
        gross_out.saturating_sub(self.total_fee(zero_for_one, gross_out))
    }

    /// exact-output：毛输入 → 用户实付（毛输入上追加各组件费用）。
    #[inline]
    pub fn apply_exact_out(&self, zero_for_one: bool, gross_in: U256) -> U256 {
        gross_in + self.total_fee(zero_for_one, gross_in)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn total_fee_floors_each_component_separately() {
        let fee = V4HookFee::AfterSwapProportional {
            zero_for_one_bps: vec![100, 250],
            one_for_zero_bps: vec![],
        };
        // 999000999000999×100/10000=9990009990009（floor），
        // 999000999000999×250/10000=24975024975024（floor）→ 和 = 34965034965033；
        // 合并 350bps 单次截断会得 34965034965034（差 1）。
        let gross = U256::from(999_000_999_000_999u128);
        assert_eq!(
            fee.total_fee(true, gross),
            U256::from(34_965_034_965_033u128)
        );
        assert_eq!(
            fee.apply_exact_in(true, gross),
            U256::from(964_035_964_035_966u128)
        );
        assert_eq!(
            fee.apply_exact_out(true, gross),
            U256::from(1_033_966_033_966_032u128)
        );
    }
}

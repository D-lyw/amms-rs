//! 低流动性（"dust"）池的强制保留机制
//!
//! # 背景
//!
//! tick 型集中流动性变体（uniswap_v3 / uniswap_v4 / pancake_v3 /
//! pancake_infinity / aerodrome_slipstream）在 `init_batch` 阶段按
//! `has_sufficient_liquidity` 丢弃"流动性不足"的池子。该判据是**近似**的
//! （阈值只由 token decimals 推导，与池子实际 TVL 无关），会把仍有真实仓位、
//! 只是规模极小的池子一并丢掉——而这类池子恰是低 TVL 错价套利（dust 套利）
//! 的场所，被丢掉后在运行期完全不可见（事件流不订阅、swap 无法模拟），
//! 属于静默失效。
//!
//! # 职责边界
//!
//! 本模块只提供**机制**，不含任何具体链 / 协议 / 池子数据：由使用方在
//! `init_batch` **之前**调用 [`set_force_retain`] 注册需要放行的地址集合。
//! 默认空集——不注册时行为与既有实现完全一致。
//!
//! # 地址口径
//!
//! UniswapV4 / PancakeInfinity 的池子没有独立合约地址，其
//! `AutomatedMarketMaker::address()` 即 `pool_id` 前 20 字节（`StateSpace`
//! 的 `HashMap<Address, AMM>` key 同此约定）。注册表一律按该虚拟地址匹配，
//! 使用方无需区分池子类型。
//!
//! # 使用方式
//!
//! ```ignore
//! amms::amms::liquidity_gate::set_force_retain(dust_addresses);
//! // 之后才调用 StateSpaceBuilder::sync()
//! ```

use std::collections::HashSet;
use std::sync::{OnceLock, RwLock};

use alloy::primitives::Address;

/// 全局强制保留集合（初始化阶段写入，运行期只读）
static FORCE_RETAIN: OnceLock<RwLock<HashSet<Address>>> = OnceLock::new();

fn registry() -> &'static RwLock<HashSet<Address>> {
    FORCE_RETAIN.get_or_init(|| RwLock::new(HashSet::new()))
}

/// 覆盖式设置强制保留集合（幂等；重复调用以最后一次为准）。
///
/// **必须在 `init_batch` 之前调用**：amms 不会自动推断哪些池子需要保留。
/// 注册后的地址在 5 个 tick 型变体的 `has_sufficient_liquidity` 处被无条件放行。
pub fn set_force_retain<I>(pools: I)
where
    I: IntoIterator<Item = Address>,
{
    let mut guard = registry().write().unwrap();
    guard.clear();
    guard.extend(pools);
    // 启动日志必须看得见：静默失效（注册了却没生效）比过滤本身更危险。
    tracing::info!(
        target: "amms::liquidity_gate",
        count = guard.len(),
        "Force-retain set replaced: these pools bypass the init liquidity gate"
    );
}

/// 追加单个地址（不清空既有集合）。
pub fn add_force_retain(pool: Address) {
    registry().write().unwrap().insert(pool);
}

/// 清空集合（回到默认行为：按 `has_sufficient_liquidity` 过滤）。
pub fn clear_force_retain() {
    registry().write().unwrap().clear();
}

/// 该地址是否被强制保留（未注册 / 空集时恒为 false）。
pub fn is_force_retained(pool: &Address) -> bool {
    registry().read().unwrap().contains(pool)
}

/// 当前集合快照（排序后，用于启动自检 / 日志对账）。
pub fn force_retained() -> Vec<Address> {
    let mut pools: Vec<Address> = registry().read().unwrap().iter().copied().collect();
    pools.sort();
    pools
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 注册表是进程级全局状态；并行测试会互相覆盖，故串行化。
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn addr(last: u8) -> Address {
        let mut bytes = [0u8; 20];
        bytes[0] = 0xd0;
        bytes[19] = last;
        Address::from(bytes)
    }

    /// 注册表是进程级全局状态，整个往返放在单个测试内串行驱动，
    /// 避免并行测试互相干扰。
    #[test]
    fn force_retain_registry_roundtrip() {
        let _guard = TEST_LOCK.lock().unwrap();
        let a = addr(1);
        let b = addr(2);
        let c = addr(3);

        clear_force_retain();
        assert!(!is_force_retained(&a), "默认空集：不得放行任何池子");

        set_force_retain([a]);
        assert!(is_force_retained(&a));
        assert!(!is_force_retained(&b), "未注册地址不得被放行");

        // 覆盖语义：第二次 set 不是追加
        set_force_retain([b, c]);
        assert!(is_force_retained(&b));
        assert!(is_force_retained(&c));
        assert!(!is_force_retained(&a), "覆盖式 set 必须清掉旧集合");

        add_force_retain(a);
        let mut expect = vec![a, b, c];
        expect.sort();
        assert_eq!(force_retained(), expect);

        clear_force_retain();
        assert!(force_retained().is_empty());
        assert!(!is_force_retained(&a));
    }

    /// 端到端：真实池对象走阈值判定 → 注册后绕过 → 清空后恢复。
    #[test]
    fn force_retain_bypasses_v3_liquidity_threshold() {
        let _guard = TEST_LOCK.lock().unwrap();

        use crate::amms::amm::AutomatedMarketMaker;
        use crate::amms::uniswap_v3::UniswapV3Pool;
        use crate::amms::Token;

        let pool_addr = addr(9);
        let mut pool = UniswapV3Pool::new(pool_addr);
        pool.token_a = Token {
            address: addr(10),
            decimals: 6,
            ..Default::default()
        };
        pool.token_b = Token {
            address: addr(11),
            decimals: 18,
            ..Default::default()
        };
        // 阈值 = isqrt(100 * 10^6 * 10^14) = 1e11；4.8e10 低于阈值且无 ticks → dust
        pool.liquidity = 48_000_000_000;

        clear_force_retain();
        assert!(
            !pool.has_sufficient_liquidity(),
            "低于阈值的池子默认应被过滤"
        );

        set_force_retain([pool_addr]);
        assert!(
            pool.has_sufficient_liquidity(),
            "注册后必须无条件放行（阈值判定被绕过）"
        );

        clear_force_retain();
        assert!(!pool.has_sufficient_liquidity(), "清空后应回到默认判定");
    }
}

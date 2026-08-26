//! 回归：CurveLegacy CryptoSwap `simulate_swap_mut` 在余额变更后必须重算存储的 D 不变量。
//!
//! 真实事故（Ethereum tricrypto2 `0xD51a44d3`，块 25836953/54，用户 tx `0x521eb6..`）：
//! - 链上基准 `get_dy(2,1,131413713975510)` = 410 (WBTC raw)；
//! - `simulate_swap_mut(USDT→WETH, 108655825)` 后，修复前 `simulate_swap(WETH→WBTC, …)` = 493（+20% 虚假利润）；
//! - 修复后必须仍是 410（与链上一致）。

use alloy::primitives::{address, Address, U256};
use amms::amms::amm::AutomatedMarketMaker;
use amms::amms::curve_legacy::{CurveLegacyPool, CurveLegacyPoolType};
use eyre::Result;

const WETH: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
const WBTC: Address = address!("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");
const USDT: Address = address!("0xdAC17F958D2ee523a2206206994597C13D831ec7");

/// tricrypto2 @ 25836953 的链上真实状态（amms init 逐字段核对一致）。
fn tricrypto2_fixture() -> CurveLegacyPool {
    let mut pool = CurveLegacyPool::new(
        address!("0xD51a44d3FaE010294C616388b506AcdA1bfAAE46"),
        CurveLegacyPoolType::CryptoSwap,
    );
    pool.n_coins = 3;
    pool.coins = vec![USDT, WBTC, WETH];
    pool.decimals = vec![6, 8, 18];
    pool.balances = vec![
        U256::from(3_852_708_854_134u64),
        U256::from(4_850_303_711u64),
        U256::from(1_548_962_430_077_171_915_384u128),
    ];
    pool.amp = Some(U256::from(1_707_629u64));
    pool.gamma = Some(U256::from(11_809_167_828_997u64));
    pool.d = Some(U256::from(11_459_081_564_139_165_251_247_804u128));
    pool.price_scale = Some(vec![
        U256::from(78_618_626_283_469_519_212_553u128),
        U256::from(2_448_864_658_202_515_383_101u128),
    ]);
    pool.mid_fee = Some(U256::from(3_000_000u64));
    pool.out_fee = Some(U256::from(30_000_000u64));
    pool.fee_gamma = Some(U256::from(500_000_000_000_000u64));
    pool
}

#[test]
fn tricrypto2_simulate_swap_mut_keeps_d_invariant() -> Result<()> {
    let mut pool = tricrypto2_fixture();

    // 链上基准：get_dy(2,1,131413713975510) = 410
    let arb_in = U256::from(131_413_713_975_510u128);
    assert_eq!(pool.simulate_swap(WETH, WBTC, arb_in)?, U256::from(410u64));

    // 用户 pending swap（0x521eb6.. 的 USDT→WETH 腿）
    let user_out = pool.simulate_swap_mut(USDT, WETH, U256::from(108_655_825u64))?;
    assert_eq!(user_out, U256::from(43_991_457_017_091_559u128));

    // 回归：修复前此处返回 493（虚假 +20%），修复后必须仍是 410
    assert_eq!(
        pool.simulate_swap(WETH, WBTC, arb_in)?,
        U256::from(410u64),
        "simulate_swap_mut 后 CryptoSwap D 不变量失配，产生虚假价格（真实事故 493 vs 410）"
    );

    // 自洽性：再次重算 D 不应改变任何后续模拟结果
    let before = pool.simulate_swap(WETH, WBTC, arb_in)?;
    pool.recalculate_crypto_state()?;
    let after = pool.simulate_swap(WETH, WBTC, arb_in)?;
    assert_eq!(before, after);

    Ok(())
}

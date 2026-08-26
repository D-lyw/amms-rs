//! 回归：CurveNG TwoCrypto `simulate_swap_mut` 在余额变更后必须重算存储的 D 不变量。
//!
//! 与 CurveLegacy tricrypto2 同类问题：`simulate_cryptoswap` 的 TwoCrypto 分支使用
//! 存储的 `self.d`，若 `simulate_swap_mut` 只改 balances 不刷新 D，后续模拟会得到
//! 失配价格（可放大为虚假套利）。

use alloy::primitives::{address, Address, U256};
use amms::amms::amm::AutomatedMarketMaker;
use amms::amms::curve_ng::{CurveNGPool, CurveNGPoolType};
use eyre::Result;

const TOKEN_A: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"); // WETH
const TOKEN_B: Address = address!("0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0"); // wstETH

/// wstETH/WETH 风格 TwoCrypto 池（参数取自常见 twocrypto-ng 部署量级）。
fn twocrypto_fixture() -> CurveNGPool {
    let mut pool = CurveNGPool::new(
        address!("0x7EC81Ef12057008c0BB6B540127f88f917b4fC6c"),
        CurveNGPoolType::TwoCrypto,
    );
    pool.n_coins = 2;
    pool.coins = vec![TOKEN_A, TOKEN_B];
    pool.decimals = vec![18, 18];
    pool.balances = vec![
        U256::from(50_000_000_000_000_000_000_000_000u128), // 50k WETH
        U256::from(45_000_000_000_000_000_000_000_000u128), // 45k wstETH
    ];
    pool.amp = Some(U256::from(40_000u64));
    pool.gamma = Some(U256::from(145_000_000_000_000u128)); // 1.45e14
    pool.price_scale = Some(vec![U256::from(1_090_000_000_000_000_000u128)]); // 1.09
    pool.mid_fee = Some(U256::from(3_000_000u64));
    pool.out_fee = Some(U256::from(30_000_000u64));
    pool.fee_gamma = Some(U256::from(500_000_000_000_000u64));
    pool
}

#[test]
fn twocrypto_simulate_swap_mut_keeps_d_invariant() -> Result<()> {
    let mut pool = twocrypto_fixture();
    // 从 xp 推导自洽 D（模拟 init 后存储 D 与余额一致的状态）
    pool.recalculate_d()?;

    let dx = U256::from(100_000_000_000_000_000_000u128); // 100 (18 decimals)
    let out = pool.simulate_swap(TOKEN_A, TOKEN_B, dx)?;

    // 模拟一笔 swap（余额变更 + 存储 D 必须同步刷新）
    let mut pool2 = pool.clone();
    let out_mut = pool2.simulate_swap_mut(TOKEN_A, TOKEN_B, dx)?;
    assert_eq!(out_mut, out);

    // 修复前：pool2.d 仍是旧 D → 与当前余额失配；
    // 修复后：pool2.d == newton_d(当前 xp)
    let mut ref_pool = pool2.clone();
    ref_pool.d = None;
    ref_pool.recalculate_d()?;
    assert_eq!(
        pool2.d, ref_pool.d,
        "simulate_swap_mut 后存储 D 必须与当前余额一致（CurveNG TwoCrypto）"
    );

    // 输出自洽：再次 recalculate_d 不应改变反向模拟结果
    let before = pool2.simulate_swap(TOKEN_B, TOKEN_A, out_mut)?;
    pool2.recalculate_d()?;
    let after = pool2.simulate_swap(TOKEN_B, TOKEN_A, out_mut)?;
    assert_eq!(before, after);

    Ok(())
}

//! 跳步（`tick_span_table`）在**真实 v4 池**上的差分验收。
//!
//! 断言：开启跳步后，`simulate_swap` / `simulate_swap_mut` /
//! `simulate_swap_exact_out` 的输出与**推进后的池状态**，与关闭跳步时逐位相同。
//! 同时打印一次冷/热走步耗时，作为"确实变快"的证据。
//!
//! ```bash
//! cargo test --release --test tick_span_table_differential -- --nocapture
//! ```

use std::str::FromStr;
use std::time::Instant;

use alloy::primitives::{
    address,
    aliases::{I24, U24},
    Address, U256,
};
use amms::amms::amm::AutomatedMarketMaker;
use amms::amms::tick_span_table;
use amms::amms::uniswap_v4::IPoolManager::PoolKey;
use amms::amms::uniswap_v4::UniswapV4Pool;
use amms::amms::Token;
use uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick;

const MANAGER: Address = address!("0000000000000000000000000000000000000abc");
const CURRENCY0: Address = address!("4ae46a509f6b1d9056937ba4500cb143933d2dc8");
const CURRENCY1: Address = address!("779ded0c9e1022225f8e0630b35a9b54be713736");

/// 事故形状：`tick_spacing = 1`、活跃流动性被单笔宽区间头寸锁住、
/// 当前 tick 落在长空 word 段里 ⇒ 一次模拟要走 ~268 个空 word。
fn build_dust_pool() -> UniswapV4Pool {
    let key = PoolKey {
        currency0: CURRENCY0,
        currency1: CURRENCY1,
        fee: U24::from(9u64),
        tickSpacing: I24::try_from(1).unwrap(),
        hooks: Address::ZERO,
    };
    let mut pool = UniswapV4Pool::new(MANAGER, key);
    pool.token_a = Token::new_with_decimals(CURRENCY0, 6);
    pool.token_b = Token::new_with_decimals(CURRENCY1, 6);
    pool.token_a.chain_id = 196;
    pool.token_b.chain_id = 196;
    pool.tick_spacing = 1;
    let tick = -68_530i32;
    let liquidity = 999_616u128;
    pool.modify_position(
        tick.saturating_sub(887_272),
        tick.saturating_add(887_272),
        liquidity as i128,
    )
    .unwrap();
    pool.tick = tick;
    pool.sqrt_price = get_sqrt_ratio_at_tick(tick).unwrap();
    pool.liquidity = liquidity;
    pool
}

fn amounts() -> Vec<U256> {
    // 覆盖 1 wei ~ 1e15：足够小 ⇒ 金额受限步提前终止；足够大 ⇒ 走满长空 word 段。
    vec![
        U256::from(1u8),
        U256::from(1_000u64),
        U256::from(124_522u64),
        U256::from(964_642u64),
        U256::from(96_464_200u64),
        U256::from(10u64).pow(U256::from(12u8)),
        U256::from(10u64).pow(U256::from(15u8)),
    ]
}

/// 把 `Result` 拍平成可比较的形态（Err 的文案也参与比对）。
fn call<F: FnOnce() -> Result<U256, amms::amms::error::AMMError>>(f: F) -> Result<U256, String> {
    f().map_err(|e| format!("{e:?}"))
}

#[test]
fn v4_dust_swap_is_bit_exact_with_and_without_span_table() {
    let pool = build_dust_pool();
    let list = amounts();

    // ---- 参考：完全关闭跳步 ----
    tick_span_table::set_enabled(false);
    tick_span_table::clear();
    let mut reference_in = vec![];
    let mut reference_out = vec![];
    let mut reference_state = vec![];
    for a in &list {
        reference_in.push(call(|| pool.simulate_swap(CURRENCY1, CURRENCY0, *a)));
        // 反向（one_for_zero）也要对齐
        reference_out.push(call(|| pool.simulate_swap(CURRENCY0, CURRENCY1, *a)));
        reference_out.push(call(|| {
            pool.simulate_swap_exact_out(CURRENCY1, CURRENCY0, *a)
        }));
        let mut clone = pool.clone();
        reference_state.push(
            call(|| clone.simulate_swap_mut(CURRENCY1, CURRENCY0, *a))
                .map(|out| (out, clone.tick, clone.sqrt_price, clone.liquidity)),
        );
    }

    // ---- 开启跳步：冷 + 热（多轮，确保命中缓存路径也被覆盖）----
    tick_span_table::set_enabled(true);
    tick_span_table::clear();
    for round in 0..4 {
        for (i, a) in list.iter().enumerate() {
            let got_in = call(|| pool.simulate_swap(CURRENCY1, CURRENCY0, *a));
            assert_eq!(
                got_in, reference_in[i],
                "round={round} amount={a} zfo 输出不一致"
            );
            let got_out = call(|| pool.simulate_swap(CURRENCY0, CURRENCY1, *a));
            assert_eq!(
                got_out,
                reference_out[i * 2],
                "round={round} amount={a} ofz 输出不一致"
            );
            let got_ex = call(|| pool.simulate_swap_exact_out(CURRENCY1, CURRENCY0, *a));
            assert_eq!(
                got_ex,
                reference_out[i * 2 + 1],
                "round={round} amount={a} exact_out 输出不一致"
            );
            let mut clone = pool.clone();
            let got_state = call(|| clone.simulate_swap_mut(CURRENCY1, CURRENCY0, *a))
                .map(|out| (out, clone.tick, clone.sqrt_price, clone.liquidity));
            assert_eq!(
                got_state, reference_state[i],
                "round={round} amount={a} simulate_swap_mut 推进状态不一致"
            );
        }
    }

    // ---- 冷/热耗时对比（仅打印 + 宽松断言）----
    let long = U256::from_str("964642").unwrap();
    tick_span_table::clear();
    let t0 = Instant::now();
    let cold_out = call(|| pool.simulate_swap(CURRENCY1, CURRENCY0, long));
    let cold = t0.elapsed();
    let t1 = Instant::now();
    let warm_out = call(|| pool.simulate_swap(CURRENCY1, CURRENCY0, long));
    let warm = t1.elapsed();
    assert_eq!(cold_out, warm_out);
    println!("[span] dust 长走步：cold={cold:?} warm={warm:?}");
    assert!(
        warm * 3 < cold,
        "热路径应显著快于冷路径：cold={cold:?} warm={warm:?}"
    );
}

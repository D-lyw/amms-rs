//! 模拟走步观测（`amms::amms::sim_stats`）自证测试。
//!
//! 构造两种形状的 V4 池，验证「per-sim 步数 / 跨空 word 数 / 耗时」在快照里可见：
//!
//! - **dust 形状**（复刻 XLayer 196 事故池的几何）：`tick_spacing = 1`、活跃流动性
//!   被单笔宽区间头寸锁住、当前 tick 落在长空 word 段里 ⇒ 一次模拟要走 ~268 个
//!   tick word 才把价格推到目标。
//! - **深池形状**：已初始化 tick 就在当前 word 内 ⇒ 1 步结束。
//!
//! 断言的是**步数**（与构建 profile 无关，debug/release 一致），耗时只打印。
//!
//! ```bash
//! cargo test --test sim_stats_walk -- --nocapture
//! # 真实耗时（ns 级）需 release：
//! cargo test --release --test sim_stats_walk -- --nocapture
//! ```

use std::time::Instant;

use std::str::FromStr;

use alloy::primitives::{
    address,
    aliases::{I24, U24},
    Address, U256,
};
use amms::amms::amm::AutomatedMarketMaker;
use amms::amms::sim_stats::{self, PoolSimStat};
use amms::amms::uniswap_v4::IPoolManager::PoolKey;
use amms::amms::uniswap_v4::UniswapV4Pool;
use amms::amms::Token;
use uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick;

const MANAGER: Address = address!("0000000000000000000000000000000000000abc");
/// 事故池的虚拟地址（= pool_id 前 20 字节），仅作可读性锚点。
const CURRENCY0: Address = address!("4ae46a509f6b1d9056937ba4500cb143933d2dc8");
const CURRENCY1: Address = address!("779ded0c9e1022225f8e0630b35a9b54be713736");

fn build_pool(tick: i32, liquidity: u128, range: i32) -> UniswapV4Pool {
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
    // 单笔宽区间头寸：把活跃流动性锁在当前 tick 附近，但下一个已初始化 tick
    // 远在 range 个 tick 之外 —— 这正是 dust 池"长空 word 段"的成因。
    pool.modify_position(
        tick.saturating_sub(range),
        tick.saturating_add(range),
        liquidity as i128,
    )
    .unwrap();
    pool.tick = tick;
    pool.sqrt_price = get_sqrt_ratio_at_tick(tick).unwrap();
    pool.liquidity = liquidity;
    pool
}

fn stats_for(pool: &UniswapV4Pool) -> PoolSimStat {
    let stats = sim_stats::take_snapshot();
    assert_eq!(stats.len(), 1, "应只观测到一个池: {stats:?}");
    assert_eq!(stats[0].key.pool, pool.address());
    stats[0]
}

/// dust 形状：一次模拟应走 >250 步，且步数与跨空 word 数同量级。
#[test]
fn dust_shape_walk_is_observable() {
    let _g = serial();
    sim_stats::set_enabled(true);
    sim_stats::reset();

    let liquidity = 999_616u128;
    let tick = -68_530i32;
    let pool = build_pool(tick, liquidity, 887_272);

    // 964,642 raw USDT0（currency1 进、currency0 出）——事故 txIndex 31 的输入量级，
    // 恰好把价格从 tick -68,530 推到 tick ≈ -58，即跨过 268 个空 word。
    let amount_in = U256::from(964_642u64);
    let started = Instant::now();
    let out = pool.simulate_swap(CURRENCY1, CURRENCY0, amount_in).unwrap();
    let wall = started.elapsed();

    let s = stats_for(&pool);
    sim_stats::set_enabled(false);

    println!(
        "[dust] out={out} steps={} empty_words={} wall={:?} sim={}ns",
        s.max_steps, s.max_empty_words, wall, s.total_ns
    );
    println!("{}", sim_stats::render(&[s], 1));

    assert_eq!(s.sims, 1);
    assert_eq!(s.errors, 0);
    assert_eq!(s.exact_in_sims, 1);
    assert_eq!(s.exact_out_sims, 0);
    assert!(
        s.max_steps > 250,
        "dust 形状应走 >250 步（几何上 ~268），实际 {}",
        s.max_steps
    );
    assert!(
        s.max_empty_words > 250,
        "跨空 word 数应与步数同量级，实际 {}",
        s.max_empty_words
    );
    // 单笔模拟的耗时应当被看见（不做绝对断言，debug/release 差异大）。
    assert!(s.total_ns > 0);
}

/// 单步成本分解：把 dust 形状的一次 268 步模拟拆成三块，看谁在吃时间。
///
/// - `bitmap`  ：268 次 `next_initialized_tick_within_one_word`（HashMap 探测 + 位运算）
/// - `sqrt`    ：268 次 `get_sqrt_ratio_at_tick`（magic-constant 取整开方）
/// - `step`    ：268 次 `compute_swap_step`（U256 乘除取整）
/// - `full`    ：一次完整的 `simulate_swap`（真实口径）
///
/// 取 N 次的最小值（min 比均值更能代表"没有调度抖动时的成本"）。
/// ```bash
/// cargo test --release --test sim_stats_walk single_step_cost_breakdown -- --nocapture
/// ```
#[test]
fn single_step_cost_breakdown() {
    use alloy::primitives::aliases::I256;
    use std::collections::HashMap;
    use uniswap_v3_math::swap_math::compute_swap_step;
    use uniswap_v3_math::tick_bitmap::next_initialized_tick_within_one_word;

    let _g = serial();
    sim_stats::set_enabled(false);

    const STEPS: usize = 268;
    const N: usize = 40;

    let pool = build_pool(-68_530, 999_616, 887_272);
    let bitmap: &HashMap<i16, U256> = &pool.tick_bitmap;
    let start_tick = -68_530i32;
    let spacing = 1i32;

    let mut best = [f64::MAX; 4];
    for _ in 0..N {
        // (a) 纯走 word
        let t = Instant::now();
        let mut tick = start_tick;
        for _ in 0..STEPS {
            let (next, _init) =
                next_initialized_tick_within_one_word(bitmap, tick, spacing, true).unwrap();
            tick = next;
        }
        best[0] = best[0].min(t.elapsed().as_nanos() as f64 / STEPS as f64);

        // (b) 每步的 sqrt 取整
        let t = Instant::now();
        let mut acc = U256::ZERO;
        for i in 0..STEPS {
            acc ^= get_sqrt_ratio_at_tick(start_tick + i as i32).unwrap();
        }
        std::hint::black_box(acc);
        best[1] = best[1].min(t.elapsed().as_nanos() as f64 / STEPS as f64);

        // (c) 每步的 swap_step 数学
        let sqrt_a = get_sqrt_ratio_at_tick(start_tick).unwrap();
        let sqrt_b = get_sqrt_ratio_at_tick(start_tick + 1).unwrap();
        let t = Instant::now();
        let mut acc = U256::ZERO;
        for i in 0..STEPS {
            let (sp, ai, ao, f) = compute_swap_step(
                sqrt_a,
                sqrt_b,
                999_616u128,
                I256::from_raw(U256::from(964_642u64 - i as u64)),
                9,
            )
            .unwrap();
            acc ^= sp ^ ai ^ ao ^ f;
        }
        std::hint::black_box(acc);
        best[2] = best[2].min(t.elapsed().as_nanos() as f64 / STEPS as f64);

        // (d) 完整模拟
        let t = Instant::now();
        let mut p = pool.clone();
        std::hint::black_box(
            p.simulate_swap_mut(CURRENCY1, CURRENCY0, U256::from(964_642u64))
                .unwrap(),
        );
        best[3] = best[3].min(t.elapsed().as_nanos() as f64 / STEPS as f64);
    }

    println!(
        "[breakdown] ns/step  bitmap={:.0}  sqrt={:.0}  swap_step={:.0}  |  full={:.0}  \
         (full total = {:.3}ms)",
        best[0],
        best[1],
        best[2],
        best[3],
        best[3] * STEPS as f64 / 1e6
    );
}

/// **折叠（B/R1）是否精准？** 直接用同一份数学跑两遍：逐 word 走 vs 一步折到
/// 下一个已初始化 tick，再与链上成交对拍。恒定流动性、无 tick 跨越（事故几何），
/// 所以两者只差**取整次数**。
///
/// 结论（本测试钉死）：折叠输出 **29,748,698**，比链上 29,748,180 多 **518 wei**
/// —— 逐 word 取整被抹掉了。所以"折叠 + 精准对齐链上"不可兼得。
#[test]
fn folding_is_not_bit_exact_518_wei() {
    use alloy::primitives::aliases::I256;
    use std::collections::HashMap;
    use uniswap_v3_math::swap_math::compute_swap_step;
    use uniswap_v3_math::tick_bitmap::next_initialized_tick_within_one_word;
    use uniswap_v3_math::tick_math::{MAX_SQRT_RATIO, MAX_TICK, MIN_TICK};

    /// 合通用：跨 word 找到下一个已初始化 tick（fold = true 时用它）。
    fn next_initialized_tick_any_word(
        bitmap: &HashMap<i16, U256>,
        tick: i32,
        spacing: i32,
        lte: bool,
    ) -> (i32, bool) {
        let mut t = tick;
        loop {
            let (next, init) =
                next_initialized_tick_within_one_word(&bitmap.clone(), t, spacing, lte).unwrap();
            if init {
                return (next, true);
            }
            if lte {
                if next <= MIN_TICK {
                    return (MIN_TICK, false);
                }
                t = next - spacing;
            } else {
                if next >= MAX_TICK {
                    return (MAX_TICK, false);
                }
                t = next + spacing;
            }
        }
    }

    let _g = serial();
    sim_stats::set_enabled(false);
    let pool = build_pool(-68_530, 999_616, 887_272);

    let run = |fold: bool| -> (U256, u32, i32, U256) {
        let mut sqrt_price = U256::from_str_radix("2575468425351407710666126886", 10).unwrap();
        let mut tick = -68_530i32;
        let liquidity = 999_616u128;
        let limit = MAX_SQRT_RATIO - U256::from(1u8);
        let mut remaining = I256::from_raw(U256::from(964_642u64));
        let mut out = I256::ZERO;
        let mut steps = 0u32;
        while remaining != I256::ZERO && sqrt_price != limit {
            let (tick_next, _init) = if fold {
                next_initialized_tick_any_word(&pool.tick_bitmap, tick, 1, false)
            } else {
                next_initialized_tick_within_one_word(&pool.tick_bitmap, tick, 1, false).unwrap()
            };
            steps += 1;
            let sqrt_next = get_sqrt_ratio_at_tick(tick_next).unwrap();
            let target = if sqrt_next > limit { limit } else { sqrt_next };
            let (sp, ain, aout, fee) =
                compute_swap_step(sqrt_price, target, liquidity, remaining, 9).unwrap();
            sqrt_price = sp;
            remaining = remaining
                .overflowing_sub(I256::from_raw(ain.overflowing_add(fee).0))
                .0;
            out -= I256::from_raw(aout);
            tick = tick_next;
        }
        ((-out).into_raw(), steps, tick, sqrt_price)
    };

    let (walk_out, walk_steps, _, _) = run(false);
    let (fold_out, fold_steps, _, _) = run(true);

    println!(
        "[fold] walk: out={walk_out} steps={walk_steps} | fold: out={fold_out} steps={fold_steps} \
         | delta={} wei (chain = 29748180)",
        fold_out - walk_out
    );

    assert_eq!(walk_out, U256::from(29_748_180u64), "逐 word 走 == 链上");
    assert_ne!(fold_out, walk_out, "折叠与逐 word 不可能逐位相等");
    assert_eq!(fold_out - walk_out, U256::from(518u64), "偏差 518 wei");
    assert!(fold_steps < walk_steps / 100, "折叠后步数应塌缩到个位数");
}

/// 单次模拟的**固定开销**：1~2 步的浅池被重复模拟 2000 次，看每次摊到多少。
///
/// 多跳联合优化器（`golden_iterations` / `probe_bands` / `beam_width`）会对
/// 同一批候选路径反复模拟，单批模拟次数是 1e4 量级 —— 所以"每次模拟的固定开销"
/// 与"每次模拟走多少步"是同一个量级的乘数。
#[test]
fn fixed_cost_per_sim_is_visible() {
    let _g = serial();
    const N: usize = 2000;

    // 关掉观测：纯模拟成本
    sim_stats::set_enabled(false);
    let mut pool = build_pool(0, 10u128.pow(18), 100);
    let t = Instant::now();
    for _ in 0..N {
        let mut p = pool.clone();
        std::hint::black_box(
            p.simulate_swap_mut(CURRENCY0, CURRENCY1, U256::from(1_000_000_000u64))
                .unwrap(),
        );
    }
    let off = t.elapsed().as_nanos() as f64 / N as f64;

    // 打开观测：额外开销
    sim_stats::set_enabled(true);
    sim_stats::reset();
    let t = Instant::now();
    for _ in 0..N {
        let mut p = pool.clone();
        std::hint::black_box(
            p.simulate_swap_mut(CURRENCY0, CURRENCY1, U256::from(1_000_000_000u64))
                .unwrap(),
        );
    }
    let on = t.elapsed().as_nanos() as f64 / N as f64;
    let s = sim_stats::take_snapshot();
    sim_stats::set_enabled(false);

    // 不可变路径（引擎主用法，无 clone）
    let t = Instant::now();
    for _ in 0..N {
        std::hint::black_box(
            pool.simulate_swap(CURRENCY0, CURRENCY1, U256::from(1_000_000_000u64))
                .unwrap(),
        );
    }
    let imm = t.elapsed().as_nanos() as f64 / N as f64;

    // 深拷贝一份池子的成本（V4 = Arc<bitmap> + HashMap<ticks>）
    let t = Instant::now();
    for _ in 0..N {
        std::hint::black_box(pool.clone());
    }
    let cl = t.elapsed().as_nanos() as f64 / N as f64;

    println!(
        "[fixed] 2-step sim: mut+clone={off:.0}ns  imm={imm:.0}ns  clone={cl:.0}ns  \
         imm+probe={on:.0}ns (+{:.0}%) | sims={} steps={}",
        (on / imm - 1.0) * 100.0,
        s[0].sims,
        s[0].max_steps
    );
    assert!(off > 0.0);
    let _ = &mut pool;
}

/// 深池对照：下一个已初始化 tick 就在当前 word 内 ⇒ 1~2 步。
#[test]
fn deep_shape_walk_is_one_step() {
    let _g = serial();
    sim_stats::set_enabled(true);
    sim_stats::reset();

    let pool = build_pool(0, 10u128.pow(18), 100);
    pool.simulate_swap(CURRENCY0, CURRENCY1, U256::from(1_000_000_000u64))
        .unwrap();

    let s = stats_for(&pool);
    sim_stats::set_enabled(false);
    println!(
        "[deep] steps={} empty_words={} sim={}ns",
        s.max_steps, s.max_empty_words, s.total_ns
    );
    assert!(s.max_steps <= 2, "深池应 1~2 步，实际 {}", s.max_steps);
}

/// 关闭时不得产生任何记录（生产默认态）。
#[test]
fn disabled_by_default_records_nothing() {
    let _g = serial();
    sim_stats::set_enabled(false);
    sim_stats::reset();

    let pool = build_pool(-68_530, 999_616, 887_272);
    pool.simulate_swap(CURRENCY1, CURRENCY0, U256::from(964_642u64))
        .unwrap();

    assert!(sim_stats::snapshot().is_empty(), "观测关闭时不应有任何记录");
}

/// 全局开关在测试间共享，串行化。
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// 事故 fixture 的**逐位对拍锚点**：状态 B 上的一次 exact-in 模拟应与链上
/// txIndex 31 的真实成交完全一致。
///
/// - 起始：`sqrt_price_x_96 = 2575468425351407710666126886`、`tick = -68530`、
///   `liquidity = 999616`、`lp_fee = 9`、`protocol_fee = 0`、`tick_spacing = 1`
///   （PoolKey 已由派生出的池地址反向确认，见 `dust_shape_walk_is_observable`）
/// - 输入 964,642 raw USDT0（currency1）⇒ 输出 **29,748,180** raw USDG、
///   结束 `tick = -58`、`liquidity = 999616`（链上真实结果）
/// - 同一次模拟走 **268** 步 —— 这就是 175ms 批次里的耗时形状
///
/// 注意：这 518 wei 的量级很关键。折叠跨空 word（把 268 次逐 word 取整收成
/// 一次）会把输出抬到 29,748,698（+518 wei ≈ 1.74e-5），**不再逐位对齐链上**；
/// 见 `folding_is_not_bit_exact_518_wei` 的实测对拍。所以"精准"这条路必须保留
/// 逐 word 取整，见 spans 累积表方案。
#[test]
fn incident_fixture_matches_chain_bit_exactly() {
    let _g = serial();
    sim_stats::set_enabled(true);
    sim_stats::reset();

    let mut pool = build_pool(-68_530, 999_616, 887_272);
    pool.sqrt_price = U256::from_str_radix("2575468425351407710666126886", 10).unwrap();
    pool.lp_fee = 9;
    pool.protocol_fee = 0;

    let mut applied = pool.clone();
    let out = applied
        .simulate_swap_mut(CURRENCY1, CURRENCY0, U256::from(964_642u64))
        .unwrap();

    let s = stats_for(&pool);
    sim_stats::set_enabled(false);

    println!(
        "[fixture] out={out} end_tick={} end_liquidity={} steps={} empty_words={} sim={}ns",
        applied.tick, applied.liquidity, s.max_steps, s.max_empty_words, s.total_ns
    );

    assert_eq!(
        out,
        U256::from(29_748_180u64),
        "必须与链上 txIndex 31 的成交逐位一致"
    );
    assert_eq!(applied.tick, -58, "结束 tick 应等于链上值");
    assert_eq!(applied.liquidity, 999_616, "流动性未跨已初始化 tick");
    assert_eq!(s.max_steps, 268, "事故的走步形状：268 步");
    assert_eq!(s.max_empty_words, 268);
}
/// 事故 fixture 的**费率敏感性对拍**（钉住 amms 侧口径）。
///
/// 状态 B（`sqrt_price_x_96 = 2575468425351407710666126886`、`tick = -68530`、
/// `liquidity = 999616`）+ 真实成交输入 964,642 raw USDT0：
///
/// - `lp_fee = 9, protocol_fee = 0` ⇒ amms 输出 **29,748,180**，与链上 txIndex 31
///   逐位一致。文档给的 fixture 是自洽的（本文件 `build_pool` 派生出的池地址
///   与事故池 `0xbd98…c4a1` 逐字节相同，见 `dust_shape_walk_is_observable`）。
/// - 叠加非零 protocol fee 会把输出压到 29,747,8xx —— 用于确认"多出来的 519 wei
///   只能来自跨空 word 的逐次取整，而不是费率口径差异"。
#[test]
fn incident_fixture_numeric_anchor() {
    let _g = serial();
    sim_stats::set_enabled(false);

    let start_sqrt = U256::from_str_radix("2575468425351407710666126886", 10).unwrap();
    let amount_in = U256::from(964_642u64);

    let mut expected = vec![];
    for (lp_fee, protocol_fee) in [
        (9u32, 0u32),
        (9, (500u32 << 12) | 500),
        (9, (520u32 << 12) | 520),
    ] {
        let mut pool = build_pool(-68_530, 999_616, 887_272);
        pool.sqrt_price = start_sqrt;
        pool.lp_fee = lp_fee;
        pool.protocol_fee = protocol_fee;
        let out = pool.simulate_swap(CURRENCY1, CURRENCY0, amount_in).unwrap();
        println!("[fixture] lp_fee={lp_fee} protocol_fee={protocol_fee:#x} out={out}");
        expected.push(out);
    }

    assert_eq!(
        expected[0],
        U256::from(29_748_180u64),
        "protocol_fee = 0 时必须逐位等于链上成交"
    );
    assert!(expected[0] > expected[1] && expected[1] > expected[2]);
}

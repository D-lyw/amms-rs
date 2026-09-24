//! 模拟热路径观测：per-pool / per-sim 的走步数与耗时聚合。
//!
//! 目的：线上出现"某一批检测耗时异常"时，直接拿到「哪个池、被模拟了多少次、
//! 每次走了多少 tick-word 步、跨了多少空 word、花了多少 ns、失败了几次」，
//! 不必再做链上取证。
//!
//! # 开销
//!
//! 默认**关闭**。关闭时热路径只多一次 relaxed 原子读 + 一个恒 false 的分支，
//! 步数/空 word 计数只写在栈上的寄存器里，`Drop` 不触碰任何全局状态。
//! 开启时每次模拟多一次分片互斥锁（~50ns 量级，相对单次模拟 100ns~100µs 可忽略），
//! 并按 `(chain_id, kind, pool)` 聚合，不保留 per-sim 原始记录。
//!
//! # 用法
//!
//! ```no_run
//! use amms::amms::sim_stats;
//!
//! // 生产：按批开启/关闭，取快照后打印最贵的池
//! sim_stats::set_enabled(true);
//! // ... 跑完一批检测 ...
//! tracing::info!(sim_top = %sim_stats::summary_top(5), "模拟热点");
//! // 或拿结构化数据自行聚合
//! let stats = sim_stats::take_snapshot();
//! sim_stats::set_enabled(false);
//! ```
//!
//! 也可用环境变量 `AMMS_SIM_STATS=1` 在进程启动时直接打开（首次 `enabled()`
//! 调用时读取一次）。
//!
//! 注意：`take_snapshot()` 会清空聚合表，所以它天然是个"批边界"。

use std::collections::HashMap;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use alloy::primitives::{Address, U256};

/// 分片数：按 key 的 hash 取模，降低多线程并发模拟时的锁竞争。
const SHARD_COUNT: usize = 16;

const LEVEL_UNINIT: u8 = 0;
const LEVEL_OFF: u8 = 1;
const LEVEL_ON: u8 = 2;

static LEVEL: AtomicU8 = AtomicU8::new(LEVEL_UNINIT);

type Shard = Mutex<HashMap<PoolSimKey, PoolSimStat>>;

/// 惰性分配：只有真正记录过样本（或显式 reset）时才建这 16 个 map。
static SHARDS: OnceLock<Box<[Shard]>> = OnceLock::new();

#[cold]
fn shards() -> &'static [Shard] {
    SHARDS.get_or_init(|| {
        (0..SHARD_COUNT)
            .map(|_| Mutex::new(HashMap::new()))
            .collect::<Vec<_>>()
            .into_boxed_slice()
    })
}

/// 观测键：链 + 协议类型 + 池标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PoolSimKey {
    /// `Token::chain_id`（池子 token_a 的 chain_id）。
    pub chain_id: u64,
    /// 协议标签，如 `"uniswap_v4"`。
    pub kind: &'static str,
    /// 池标识，与 `AutomatedMarketMaker::address()` 一致。
    /// 注意 V4 / PancakeInfinity 是 `pool_id` 的前 20 字节（StateSpace 的虚拟地址）。
    pub pool: Address,
}

/// 单个池的模拟聚合统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PoolSimStat {
    pub key: PoolSimKey,
    /// 模拟总次数。
    pub sims: u64,
    /// 返回 `Err` 的次数（含走到一半才失败的）。
    pub errors: u64,
    pub exact_in_sims: u64,
    pub exact_out_sims: u64,
    /// 累计 / 单次最大耗时（ns）。
    pub total_ns: u64,
    pub max_ns: u64,
    /// 累计 / 单次最大走步数（= tick 走步循环迭代次数）。
    pub total_steps: u64,
    pub max_steps: u32,
    /// 累计 / 单次最大跨过的空 word 数。
    pub total_empty_words: u64,
    pub max_empty_words: i32,
    /// 输入金额量级 `log2(amount)`（0 表示 0 或未知）。
    pub max_amount_log2: u8,
    /// 最近一次模拟开始时的活跃流动性与 tick。
    pub last_liquidity: u128,
    pub last_tick: i32,
}

impl PoolSimStat {
    /// 平均耗时（ns）；无样本时为 0。
    #[inline]
    pub fn avg_ns(&self) -> u64 {
        if self.sims == 0 {
            0
        } else {
            self.total_ns / self.sims
        }
    }

    /// 平均走步数。
    #[inline]
    pub fn avg_steps(&self) -> f64 {
        if self.sims == 0 {
            0.0
        } else {
            self.total_steps as f64 / self.sims as f64
        }
    }
}

/// 是否已开启观测。
///
/// 热路径友好：一次 relaxed 原子读。首次调用会尝试读取环境变量
/// `AMMS_SIM_STATS`（`1` / `true` / `on` 视为开启）。
#[inline]
pub fn enabled() -> bool {
    match LEVEL.load(Ordering::Relaxed) {
        LEVEL_ON => true,
        LEVEL_OFF => false,
        _ => init_from_env(),
    }
}

#[cold]
fn init_from_env() -> bool {
    let on = std::env::var("AMMS_SIM_STATS")
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "on" | "yes")
        })
        .unwrap_or(false);
    set_enabled(on);
    on
}

/// 显式开启 / 关闭观测。
pub fn set_enabled(on: bool) {
    LEVEL.store(if on { LEVEL_ON } else { LEVEL_OFF }, Ordering::Relaxed);
}

/// 清空聚合表（不取快照）。
pub fn reset() {
    let Some(shards) = SHARDS.get() else {
        return;
    };
    for shard in shards.iter() {
        if let Ok(mut map) = shard.lock() {
            map.clear();
        }
    }
}

/// 取快照但不清空。
pub fn snapshot() -> Vec<PoolSimStat> {
    let Some(shards) = SHARDS.get() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for shard in shards.iter() {
        if let Ok(map) = shard.lock() {
            out.extend(map.values().copied());
        }
    }
    sort_by_cost(&mut out);
    out
}

/// 取快照并清空（天然批边界）。按累计耗时降序。
pub fn take_snapshot() -> Vec<PoolSimStat> {
    let Some(shards) = SHARDS.get() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for shard in shards.iter() {
        if let Ok(mut map) = shard.lock() {
            out.extend(map.drain().map(|(_, v)| v));
        }
    }
    sort_by_cost(&mut out);
    out
}

/// 便捷入口：`take_snapshot()` + 只保留最贵的 `limit` 个 + 渲染成单行 ready 字符串。
pub fn summary_top(limit: usize) -> String {
    render(&take_snapshot(), limit)
}

/// 把快照渲染成便于直接写日志的多行字符串。
pub fn render(stats: &[PoolSimStat], limit: usize) -> String {
    let mut out = String::new();
    let sims: u64 = stats.iter().map(|s| s.sims).sum();
    let total_ns: u64 = stats.iter().map(|s| s.total_ns).sum();
    let _ = writeln!(
        out,
        "sim_stats pools={} sims={} total={}",
        stats.len(),
        sims,
        fmt_ns(total_ns)
    );
    for (i, s) in stats.iter().take(limit).enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let _ = write!(
            out,
            "  pool={:?} chain={} kind={} sims={} err={} total={} avg={} max={} \
             steps(sum/max/avg)={}/{}/{} empty_words(max)={} amount_log2={} liq={} tick={}",
            s.key.pool,
            s.key.chain_id,
            s.key.kind,
            s.sims,
            s.errors,
            fmt_ns(s.total_ns),
            fmt_ns(s.avg_ns()),
            fmt_ns(s.max_ns),
            s.total_steps,
            s.max_steps,
            s.avg_steps(),
            s.max_empty_words,
            s.max_amount_log2,
            s.last_liquidity,
            s.last_tick,
        );
    }
    out
}

fn sort_by_cost(out: &mut [PoolSimStat]) {
    out.sort_unstable_by(|a, b| b.total_ns.cmp(&a.total_ns));
}

fn fmt_ns(ns: u64) -> String {
    if ns >= 1_000_000_000 {
        format!("{:.2}s", ns as f64 / 1e9)
    } else if ns >= 1_000_000 {
        format!("{:.2}ms", ns as f64 / 1e6)
    } else if ns >= 1_000 {
        format!("{:.2}us", ns as f64 / 1e3)
    } else {
        format!("{ns}ns")
    }
}

#[inline]
fn shard_of(key: &PoolSimKey) -> usize {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    (h.finish() as usize) % SHARD_COUNT
}

/// 把一次模拟的结果并入聚合表。仅在探针开启时调用。
#[cold]
fn record(
    key: PoolSimKey,
    ns: u64,
    steps: u32,
    empty_words: i32,
    amount_log2: u8,
    liquidity: u128,
    tick: i32,
    exact_out: bool,
) {
    let Ok(mut map) = shards()[shard_of(&key)].lock() else {
        return;
    };
    let e = map.entry(key).or_insert_with(|| PoolSimStat {
        key,
        ..Default::default()
    });
    e.sims += 1;
    if exact_out {
        e.exact_out_sims += 1;
    } else {
        e.exact_in_sims += 1;
    }
    e.total_ns += ns;
    e.max_ns = e.max_ns.max(ns);
    e.total_steps += steps as u64;
    e.max_steps = e.max_steps.max(steps);
    e.total_empty_words += empty_words.max(0) as u64;
    e.max_empty_words = e.max_empty_words.max(empty_words);
    e.max_amount_log2 = e.max_amount_log2.max(amount_log2);
    e.last_liquidity = liquidity;
    e.last_tick = tick;
}

/// 由调用方在一次模拟返回 `Err` 时调用，把"失败但已经花了时间"的模拟也计进
/// 同一个池的 `errors`。`kind` / `chain_id` / `pool` 必须与该池模拟探针传入的
/// 取值一致，否则会被聚合成另一条记录。
///
/// 放在调用方（而不是模拟函数内部）是为了不侵入 13 处主循环里 80+ 个错误
/// 返回点；调用方本来就握着 `Result`。
pub fn note_error(kind: &'static str, chain_id: u64, pool: Address) {
    if !enabled() {
        return;
    }
    let key = PoolSimKey {
        chain_id,
        kind,
        pool,
    };
    let Ok(mut map) = shards()[shard_of(&key)].lock() else {
        return;
    };
    let e = map.entry(key).or_insert_with(|| PoolSimStat {
        key,
        ..Default::default()
    });
    e.errors += 1;
}

#[inline]
fn bit_len(amount: U256) -> u8 {
    amount.bit_len().min(255) as u8
}

/// 单次模拟的观测探针（RAII）。
///
/// 关闭时 `start` 为 `None`，`step()` 只递增栈上的计数器，`Drop` 直接返回。
/// 开启时在构造点取 `Instant::now()` 并**惰性**求值池标识（避免关闭时白算
/// `address()`），`Drop` 时把整次模拟的结果并入聚合表 —— 因此函数里所有
/// 提前返回（含 `?`）都会被记录，无需逐处埋点。
#[derive(Debug)]
pub(crate) struct SimProbe {
    kind: &'static str,
    chain_id: u64,
    pool: Address,
    liquidity: u128,
    tick: i32,
    start: Option<Instant>,
    steps: u32,
    empty_words: i32,
    amount_log2: u8,
    exact_out: bool,
}

impl SimProbe {
    #[inline]
    pub(crate) fn exact_in(
        kind: &'static str,
        chain_id: u64,
        amount: U256,
        liquidity: u128,
        tick: i32,
        pool: impl FnOnce() -> Address,
    ) -> Self {
        Self::new(kind, chain_id, amount, liquidity, tick, pool, false)
    }

    #[inline]
    pub(crate) fn exact_out(
        kind: &'static str,
        chain_id: u64,
        amount: U256,
        liquidity: u128,
        tick: i32,
        pool: impl FnOnce() -> Address,
    ) -> Self {
        Self::new(kind, chain_id, amount, liquidity, tick, pool, true)
    }

    #[inline]
    fn new(
        kind: &'static str,
        chain_id: u64,
        amount: U256,
        liquidity: u128,
        tick: i32,
        pool: impl FnOnce() -> Address,
        exact_out: bool,
    ) -> Self {
        if !enabled() {
            return Self {
                kind,
                chain_id: 0,
                pool: Address::ZERO,
                liquidity: 0,
                tick: 0,
                start: None,
                steps: 0,
                empty_words: 0,
                amount_log2: 0,
                exact_out,
            };
        }

        Self {
            kind,
            chain_id,
            pool: pool(),
            liquidity,
            tick,
            start: Some(Instant::now()),
            steps: 0,
            empty_words: 0,
            amount_log2: bit_len(amount),
            exact_out,
        }
    }

    /// 跳步时一次性补齐被折叠掉的迭代计数（仅观测用，不影响模拟结果）。
    #[inline]
    pub(crate) fn skip(&mut self, extra_steps: u32, extra_empty_words: u32) {
        if self.start.is_none() {
            return;
        }
        self.steps = self.steps.saturating_add(extra_steps);
        self.empty_words = self.empty_words.saturating_add(extra_empty_words as i32);
    }

    /// 记录一次走步循环迭代；`crossed_words` 为本步跨过的空 word 数
    /// （走到 initialized tick 时传 0）。
    #[inline]
    pub(crate) fn step(&mut self, crossed_words: u32) {
        self.steps = self.steps.saturating_add(1);
        if crossed_words > 0 {
            self.empty_words = self.empty_words.saturating_add(crossed_words as i32);
        }
    }
}

impl Drop for SimProbe {
    fn drop(&mut self) {
        let Some(start) = self.start else {
            return;
        };
        let ns = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        record(
            PoolSimKey {
                chain_id: self.chain_id,
                kind: self.kind,
                pool: self.pool,
            },
            ns,
            self.steps,
            self.empty_words,
            self.amount_log2,
            self.liquidity,
            self.tick,
            self.exact_out,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    /// 全局状态在测试间共享，涉及开关的测试串行化。
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn disabled_probe_records_nothing() {
        let _g = serial();
        set_enabled(false);
        reset();
        let pool = address!("00000000000000000000000000000000000000aa");
        {
            let mut probe = SimProbe::exact_in(
                "uniswap_v4",
                196,
                U256::from(1_000u64),
                999_616,
                -68_530,
                || pool,
            );
            probe.step(0);
            probe.step(1);
            probe.step(1);
        }
        assert!(snapshot().is_empty(), "关闭状态下不应产生任何记录");
    }

    #[test]
    fn enabled_probe_aggregates_steps_and_amount() {
        let _g = serial();
        set_enabled(false);
        reset();
        set_enabled(true);
        let pool = address!("00000000000000000000000000000000000000aa");
        for _ in 0..3 {
            let mut probe = SimProbe::exact_in(
                "uniswap_v4",
                196,
                U256::from(1_000_000u64),
                999_616,
                -68_530,
                || pool,
            );
            for _ in 0..5 {
                probe.step(1);
            }
            probe.step(0);
        }
        {
            let mut probe = SimProbe::exact_out("uniswap_v4", 196, U256::from(7u64), 1, 2, || pool);
            probe.step(0);
        }
        note_error("uniswap_v4", 196, pool);

        let stats = take_snapshot();
        assert_eq!(stats.len(), 1, "同一池应聚合成一条");
        let s = stats[0];
        assert_eq!(s.key.pool, pool);
        assert_eq!(s.key.chain_id, 196);
        assert_eq!(s.key.kind, "uniswap_v4");
        assert_eq!(s.sims, 4);
        assert_eq!(s.errors, 1);
        assert_eq!(s.exact_in_sims, 3);
        assert_eq!(s.exact_out_sims, 1);
        // 3 次 exact-in 各 6 步（5 空 word + 1 命中），1 次 exact-out 1 步
        assert_eq!(s.total_steps, 3 * 6 + 1);
        assert_eq!(s.max_steps, 6);
        assert_eq!(s.total_empty_words, 3 * 5);
        assert_eq!(s.max_empty_words, 5);
        assert_eq!(s.max_amount_log2, 20); // 1_000_000 < 2^20
        assert_eq!(s.last_liquidity, 1);
        assert_eq!(s.last_tick, 2);

        // take_snapshot 清空
        assert!(take_snapshot().is_empty());
        assert!(snapshot().is_empty());
        set_enabled(false);
    }

    #[test]
    fn summary_renders_top_by_total_ns() {
        let _g = serial();
        set_enabled(false);
        reset();
        set_enabled(true);
        let cheap = address!("00000000000000000000000000000000000000c1");
        let pricey = address!("00000000000000000000000000000000000000c2");
        for _ in 0..2 {
            {
                let mut p = SimProbe::exact_in("uniswap_v3", 1, U256::from(5u64), 1, 0, || cheap);
                p.step(0);
            } // p 在此 drop，耗时极小
            {
                let mut q = SimProbe::exact_in("uniswap_v3", 1, U256::from(5u64), 1, 0, || pricey);
                for _ in 0..900 {
                    q.step(1);
                }
                // 走步计数不耗时，这里显式烧掉一点时间，让排序结果确定。
                let t = Instant::now();
                while t.elapsed().as_micros() < 2_000 {
                    std::hint::black_box(0u64);
                }
            } // q 在此 drop
        }
        let stats = snapshot();
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].key.pool, pricey, "应按累计耗时降序");
        let text = render(&stats, 1);
        assert!(text.contains("pools=2"), "{text}");
        assert!(text.contains("sims=4"), "{text}");
        assert_eq!(text.lines().count(), 2);
        set_enabled(false);
        reset();
    }

    #[test]
    fn reset_clears_without_snapshot() {
        let _g = serial();
        set_enabled(false);
        reset();
        set_enabled(true);
        let pool = address!("00000000000000000000000000000000000000d1");
        {
            let mut p = SimProbe::exact_in("uniswap_v4", 1, U256::from(1u64), 0, 0, || pool);
            p.step(0);
        }
        assert_eq!(snapshot().len(), 1);
        reset();
        assert!(snapshot().is_empty());
        set_enabled(false);
    }
}

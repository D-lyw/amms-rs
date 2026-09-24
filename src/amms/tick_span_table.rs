//! tick 跨度表（tick span table）：UniswapV3 血统「逐 word 走步」的**逐位等价**跳步。
//!
//! # 为什么需要它
//!
//! UniswapV3/V4/PancakeV3/PancakeInfinity/Slipstream 的链上 swap 是**逐 tick word**
//! 迭代的：起点在某个空 word 段里时，每跨过一个 256-tick 的 word 就做一次
//! `compute_swap_step`（每次 `_get_amount_*_delta` 都向上取整）。dust 池子
//! （活跃流动性被锁在很远的区间里）一次模拟要跨上千个空 word ⇒ 单次模拟上百微秒。
//!
//! 关键点：**链上就是这么逐 word 取整的**，所以「直接折到下一个 initialized tick」
//! 这类近似会偏（实测事故池偏 +518 wei），不能用来替代。要既快又准，必须把
//! 逐 word 的结果**精确**算出来。
//!
//! # 为什么是精准的（不是近似）
//!
//! 引理 1（终止）：`uniswap_v3_math::swap_math::compute_swap_step` 在
//! `exact_in && sqrt_next != sqrt_target` 分支给出 `fee_amount = remaining - amount_in`，
//! 而各协议循环是 `remaining -= (amount_in + fee_amount)` ⇒ 该步把 remaining 恰好清零、
//! 循环当步终止。**所以「第一个非价格受限步」必然是最后一步**，被跳过的前缀一定全是
//! 「价格受限步」。
//!
//! 引理 2（常量）：价格受限步满足 `sqrt_next = target`，于是
//! `(amount_in, amount_out, fee, sqrt_after, tick_after)` 只由
//! `(sqrt_before, sqrt_after, liquidity, fee_pips)` 决定，与 remaining 无关
//! ⇒ 可以预先算成常量、前缀求和，对任意输入金额复用。
//!
//! 判据：`compute_swap_step` 走价格受限分支的条件是
//! `mul_div(remaining, 1e6 - fee, 1e6) >= amount_in`（floor 除法）。
//! 由于两者都是整数，等价于严格不等式 `remaining * (1e6 - fee) >= amount_in * 1e6`
//! （本模块用 checked_mul 实现；溢出则放弃跳步，交回原循环 —— 结果仍逐位相同）。
//!
//! 跳步 = 二分出「判据连续成立」的最远下标，把这几步的常量一次性累加进
//! `remaining / amount_calculated / sqrt_price / tick`，其余交回原循环。
//! 因此结果与逐 word 循环**逐位相同**，只是少了循环开销。
//!
//! # 缓存正确性（版本锚）
//!
//! 表条目持有创建时那张 `Arc<HashMap<..>>` 的**克隆**。池子侧任何写都必须走
//! `Arc::make_mut`（`Arc` 不能 DerefMut），而我们的克隆让 strong_count ≥ 2
//! ⇒ `make_mut` 必然 COW、指针必然改变 ⇒ **指针即版本**：bitmap 一变，旧条目
//! 自动失效，绝不会把新 initialized tick 当空 word 越过去。

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use alloy::primitives::U256;

/// UniswapV3 血统：一个 tick word 覆盖 256 个 compressed tick。
pub const TICKS_PER_WORD: i32 = 256;
/// 少于这么多步的走步不建表（建/查表开销大于省下的）。
pub const MIN_CHAIN_STEPS: usize = 6;
/// 单条链最多记录多少步（超出截断，尾部交回原循环）。
pub const MAX_CHAIN_STEPS: usize = 8192;
/// 已跨过多少个空 word 之后才尝试跳步（浅走步的池子完全不付查表开销）。
pub const GATE_EMPTY_WORDS: i32 = 2;
/// 少于这么多步不值得跳（省不下多少，却要付一次查表）。
pub const MIN_JUMP_STEPS: usize = 2;
/// 全局记录步数预算，约 32MB 上界。
const MAX_TOTAL_STEPS: usize = 200_000;
const SHARD_COUNT: usize = 16;
const MAX_ENTRIES_PER_SHARD: usize = 4;
const FEE_SCALE: u64 = 1_000_000;

static ENABLED: AtomicBool = AtomicBool::new(true);
static ENV_READ: OnceLock<()> = OnceLock::new();
static TOTAL_STEPS: AtomicUsize = AtomicUsize::new(0);

type Bitmap = HashMap<i16, U256>;

/// 是否启用跳步（可用 `AMMS_TICK_SPAN=0` 关闭；关闭后与原实现完全一致）。
#[inline]
pub fn enabled() -> bool {
    ENV_READ.get_or_init(|| {
        if let Ok(v) = std::env::var("AMMS_TICK_SPAN") {
            if v == "0" || v.eq_ignore_ascii_case("false") {
                ENABLED.store(false, Ordering::Relaxed);
            }
        }
    });
    ENABLED.load(Ordering::Relaxed)
}

/// 测试/排障用开关。生产默认开启。
#[inline]
pub fn set_enabled(v: bool) {
    ENABLED.store(v, Ordering::Relaxed);
}

/// 池子的一把「走步钥匙」：同一条链只与这些量有关。
///
/// 注意 `bitmap_ptr`（`Arc::as_ptr`）既是**池子身份**也是**bitmap 版本**：
/// 不同池子不会共用同一个 `Arc`；同一池子一旦写 bitmap 必然换指针（见模块注释）。
#[derive(Clone, Copy, Debug)]
pub struct RunKey {
    pub bitmap_ptr: usize,
    pub zero_for_one: bool,
    pub tick_spacing: i32,
    pub liquidity: u128,
    pub fee_pips: u32,
}

impl PartialEq for RunKey {
    #[inline]
    fn eq(&self, o: &Self) -> bool {
        self.bitmap_ptr == o.bitmap_ptr
            && self.zero_for_one == o.zero_for_one
            && self.tick_spacing == o.tick_spacing
            && self.liquidity == o.liquidity
            && self.fee_pips == o.fee_pips
    }
}
impl Eq for RunKey {}
impl Hash for RunKey {
    #[inline]
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.bitmap_ptr.hash(h);
        self.zero_for_one.hash(h);
        self.tick_spacing.hash(h);
        self.liquidity.hash(h);
        self.fee_pips.hash(h);
    }
}

impl RunKey {
    #[inline]
    pub fn new(
        bitmap: &Arc<Bitmap>,
        zero_for_one: bool,
        tick_spacing: i32,
        liquidity: u128,
        fee_pips: u32,
    ) -> Self {
        Self {
            bitmap_ptr: Arc::as_ptr(bitmap) as *const () as usize,
            zero_for_one,
            tick_spacing,
            liquidity,
            fee_pips,
        }
    }
}

/// 一步（word 边界 → word 边界）的常量。
#[derive(Clone, Copy, Debug)]
pub struct SpanStep {
    pub end_tick: i32,
    pub end_sqrt: U256,
    pub amount_in: U256,
    pub amount_out: U256,
    pub fee_amount: U256,
}

/// 一次跳步的结果（exact-in）。
#[derive(Clone, Copy, Debug)]
pub struct Jump {
    /// 跳过的走步数（≥ 2）。
    pub steps: u32,
    /// Σ(amount_in + fee)
    pub consumed: U256,
    /// Σ amount_out
    pub amount_out: U256,
    pub end_tick: i32,
    pub end_sqrt: U256,
}

struct SpanChain {
    /// 版本锚：持有创建时那一刻的 bitmap。
    _bitmap: Arc<Bitmap>,
    /// 链首步起点（boundary arrival 状态）。
    first_tick: i32,
    first_sqrt: U256,
    steps: Vec<SpanStep>,
    /// Σ(in+fee)，长度 = steps.len() + 1
    prefix_a: Vec<U256>,
    /// Σout
    prefix_out: Vec<U256>,
    /// envelope[i] = max(in_0..in_i)
    envelope: Vec<U256>,
    /// boundary arrival tick → 步下标（tick 唯一定位 boundary arrival 状态）
    index: HashMap<i32, u32>,
    /// 1e6 - fee_pips
    fee_denom: U256,
}

impl SpanChain {
    fn build(
        key: RunKey,
        bitmap: Arc<Bitmap>,
        first_tick: i32,
        first_sqrt: U256,
        steps: Vec<SpanStep>,
    ) -> Option<Self> {
        if steps.is_empty() {
            return None;
        }
        if key.fee_pips >= FEE_SCALE as u32 {
            // fee_pips = 1e6 ⇒ `compute_swap_step` 自己在 `1e6 - fee` 上除零报错，
            // 这里直接不建链（原地循环会给出同一个 Err，语义不变）。
            return None;
        }
        let fee_denom = U256::from(FEE_SCALE - key.fee_pips as u64);
        let mut chain = Self {
            _bitmap: bitmap,
            first_tick,
            first_sqrt,
            steps,
            prefix_a: Vec::new(),
            prefix_out: Vec::new(),
            envelope: Vec::new(),
            index: HashMap::new(),
            fee_denom,
        };
        chain.rebuild();
        Some(chain)
    }

    /// 重建前缀数组与下标（只在链变长/新建时调用）。
    fn rebuild(&mut self) {
        let n = self.steps.len();
        self.prefix_a = Vec::with_capacity(n + 1);
        self.prefix_out = Vec::with_capacity(n + 1);
        self.envelope = Vec::with_capacity(n);
        self.index = HashMap::with_capacity(n + 1);

        let mut acc_a = U256::ZERO;
        let mut acc_out = U256::ZERO;
        let mut env = U256::ZERO;
        let mut prev_end_tick = self.first_tick;
        self.prefix_a.push(acc_a);
        self.prefix_out.push(acc_out);
        self.index.insert(prev_end_tick, 0);
        for (i, s) in self.steps.iter().enumerate() {
            let a = s.amount_in.saturating_add(s.fee_amount);
            acc_a = acc_a.saturating_add(a);
            acc_out = acc_out.saturating_add(s.amount_out);
            self.prefix_a.push(acc_a);
            self.prefix_out.push(acc_out);
            if s.amount_in > env {
                env = s.amount_in;
            }
            self.envelope.push(env);
            prev_end_tick = s.end_tick;
            self.index.entry(prev_end_tick).or_insert(i as u32 + 1);
        }
    }
}

// ---------------------------------------------------------------------------
// 记录器：走步时顺带把「boundary → boundary」的步记下来，一次走步零额外开销。
// ---------------------------------------------------------------------------

pub struct Recorder {
    /// 链首步的起点状态与当时那把钥匙（记录期间 (L, fee, dir, bitmap) 恒定）。
    key: Option<RunKey>,
    first_tick: i32,
    first_sqrt: U256,
    steps: Vec<SpanStep>,
    /// 上一个**可链接步**是否把价格恰好落在 word 边界价上
    /// （即「下一步的起点是 boundary arrival 状态」）。
    at_boundary: bool,
    /// 总开关（关闭时完全不记录）。
    active: bool,
    /// 是否还在继续记录。**跳步之后必须停记**：跳过的步不在本趟链里，
    /// 继续记录会让链出现空洞（前缀和会把空洞当成 0 消费量）⇒ 误跳。
    recording: bool,
}

impl Recorder {
    #[inline]
    pub fn new(enabled: bool) -> Self {
        Self {
            key: None,
            first_tick: 0,
            first_sqrt: U256::ZERO,
            steps: Vec::new(),
            at_boundary: false,
            active: enabled,
            recording: enabled,
        }
    }

    /// 本趟走步发生了跳步：把已记录的**连续前缀**落地，并重新起一段。
    ///
    /// 跳过的步不在本趟记录里，若继续往同一条链里追加就会出现空洞
    /// （前缀和会把空洞当成零消费量）⇒ 必须在这里断开。
    /// `at_boundary_now`：跳步落点是否为 boundary arrival 状态（是 ⇒ 新链可从下一步开始记）。
    #[inline]
    pub fn flush(&mut self, bitmap: &Arc<Bitmap>, at_boundary_now: bool) {
        if self.active && !self.steps.is_empty() {
            self.publish(bitmap);
        }
        self.at_boundary = at_boundary_now;
    }

    #[inline]
    pub fn disabled() -> Self {
        Self::new(false)
    }

    /// 循环体末尾调用一次。
    ///
    /// - `start_tick` / `start_sqrt`：本步起点状态
    /// - `step`：本步常量（来自真实 `compute_swap_step`，含真实 `fee_amount`）
    /// - `chainable`：本步是「价格受限 + 未跨 initialized tick + 目标价未被 limit 改写
    ///   + tick 未被 clamp」的整 word 步
    /// - `landed_on_boundary`：本步结束时价格恰好等于 word 边界价
    #[inline]
    pub fn observe(
        &mut self,
        bitmap: &Arc<Bitmap>,
        key: RunKey,
        start_tick: i32,
        start_sqrt: U256,
        step: SpanStep,
        chainable: bool,
        landed_on_boundary: bool,
    ) {
        if !self.recording {
            return;
        }
        if chainable && self.at_boundary {
            if self.steps.is_empty() {
                self.key = Some(key);
                self.first_tick = start_tick;
                self.first_sqrt = start_sqrt;
            }
            if self.steps.len() < MAX_CHAIN_STEPS {
                self.steps.push(step);
            }
        } else if !self.steps.is_empty() {
            // 链断了（跨 initialized tick / 金额受限步 / 命中 limit 或 clamp）
            self.publish(bitmap);
        }
        self.at_boundary = chainable && landed_on_boundary;
    }

    /// 走步结束后落地。
    #[inline]
    pub fn finish(&mut self, bitmap: &Arc<Bitmap>) {
        if self.active && !self.steps.is_empty() {
            self.publish(bitmap);
        }
    }

    /// 链太短或超预算则丢弃。
    fn publish(&mut self, bitmap: &Arc<Bitmap>) {
        let Some(key) = self.key.take() else {
            self.steps.clear();
            return;
        };
        self.at_boundary = false;
        if self.steps.len() < MIN_CHAIN_STEPS {
            self.steps.clear();
            return;
        }
        if let Some(chain) = SpanChain::build(
            key,
            Arc::clone(bitmap),
            self.first_tick,
            self.first_sqrt,
            std::mem::take(&mut self.steps),
        ) {
            insert_or_extend(key, chain);
        }
    }
}

// ---------------------------------------------------------------------------
// 缓存
// ---------------------------------------------------------------------------

type Shard = Mutex<HashMap<RunKey, SpanChain>>;
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

#[inline]
fn shard_of(key: &RunKey) -> &'static Shard {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    &shards()[(hasher.finish() as usize) % SHARD_COUNT]
}

/// 追加 `n` 步到预算内；超预算返回 false（调用方放弃本次落地/扩展）。
fn reserve_steps(n: usize) -> bool {
    let mut cur = TOTAL_STEPS.load(Ordering::Relaxed);
    loop {
        if cur + n > MAX_TOTAL_STEPS {
            return false;
        }
        match TOTAL_STEPS.compare_exchange_weak(cur, cur + n, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => return true,
            Err(observed) => cur = observed,
        }
    }
}

/// 归还已落地的步数（驱逐 / 清空时必须调用，否则预算会被漏光）。
fn release_steps(n: usize) {
    TOTAL_STEPS.fetch_sub(
        n.min(TOTAL_STEPS.load(Ordering::Relaxed)),
        Ordering::Relaxed,
    );
}

fn live_steps(map: &HashMap<RunKey, SpanChain>) -> usize {
    map.values().map(|c| c.steps.len()).sum()
}

fn insert_or_extend(key: RunKey, chain: SpanChain) {
    let shard = shard_of(&key);
    let mut guard = match shard.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if guard.len() >= MAX_ENTRIES_PER_SHARD && !guard.contains_key(&key) {
        let freed = live_steps(&guard);
        guard.clear();
        release_steps(freed);
    }
    let new_len = chain.steps.len();
    let Some(existing) = guard.get_mut(&key) else {
        if reserve_steps(new_len) {
            guard.insert(key, chain);
        }
        return;
    };

    // 定位对齐下标：
    // 1) 新链起点已在既有链里 ⇒ 从该下标往后对齐；
    // 2) 新链起点正好接在既有链末尾 ⇒ 直接续接（跳步后继续记录会落在这一支）；
    // 3) 其它（跨过 net=0 的 initialized tick 后另起一段）⇒ 丢弃新链。
    //    宁可少跳，也不做「替换」造成抖动。
    let start = match existing.index.get(&chain.first_tick) {
        Some(&i) => i as usize,
        None => {
            if existing.steps.last().map(|s| s.end_tick) == Some(chain.first_tick) {
                existing.steps.len()
            } else {
                return;
            }
        }
    };
    let covered = existing.steps.len().saturating_sub(start);
    if chain.steps.len() <= covered {
        return;
    }
    #[cfg(debug_assertions)]
    {
        // 抽查重叠部分：同一起点、同一 bitmap、同一 (L, fee) ⇒ 必须逐位一致。
        let overlap = covered.min(chain.steps.len());
        let mut probes: Vec<usize> = Vec::new();
        if overlap > 0 {
            probes.push(0);
            if overlap > 2 {
                probes.push(overlap / 2);
                probes.push(overlap - 1);
            }
        }
        for i in probes {
            debug_assert_eq!(
                existing.steps[start + i].end_tick,
                chain.steps[i].end_tick,
                "span chain merge mismatch (tick)"
            );
            debug_assert_eq!(
                existing.steps[start + i].amount_in,
                chain.steps[i].amount_in,
                "span chain merge mismatch (amount_in)"
            );
        }
    }
    let added = chain.steps.len() - covered;
    if !reserve_steps(added) {
        return;
    }
    if start == existing.steps.len() {
        existing.steps.extend_from_slice(&chain.steps);
    } else {
        existing.steps.extend_from_slice(&chain.steps[covered..]);
    }
    existing.rebuild();
}

// ---------------------------------------------------------------------------
// 跳步（exact-in）
// ---------------------------------------------------------------------------

/// 尝试跳步。
///
/// - `tick`/`sqrt_price`：当前状态（必须与记录时的 boundary arrival 状态逐位相同）
/// - `amount_remaining`：exact-in 的正数剩余输入
/// - `sqrt_price_limit`：本次走步的价格上下限。链是在「limit 未 bind」的条件下录下来的，
///   判据里额外要求本次 limit **不落在跳步区间内**；否则原循环会在 limit 处停下，
///   而跳步会越过去 ⇒ 结果不一致。
///
/// 返回 `None` 表示「不跳」，调用方按原循环继续，结果与优化前逐位相同。
pub fn try_jump(
    bitmap: &Arc<Bitmap>,
    key: RunKey,
    tick: i32,
    sqrt_price: U256,
    amount_remaining: U256,
    sqrt_price_limit: U256,
) -> Option<Jump> {
    if !enabled() || amount_remaining.is_zero() {
        return None;
    }
    let guard = match shard_of(&key).lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let chain = guard.get(&key)?;
    debug_assert!(Arc::ptr_eq(&chain._bitmap, bitmap));
    let j = *chain.index.get(&tick)? as usize;
    if j >= chain.steps.len() {
        return None;
    }
    let expected_sqrt = if j == 0 {
        chain.first_sqrt
    } else {
        chain.steps[j - 1].end_sqrt
    };
    if expected_sqrt != sqrt_price {
        return None;
    }

    // 单调谓词（前缀性质，可二分）：
    //   p(m) = (R - ΔA[m]) * (1e6 - fee) >= envelope[m] * 1e6
    // ΔA[m] = prefix_a[m] - prefix_a[j]（单调增），envelope 单调不减
    // ⇒ p 单调不增 ⇒ 判据成立的下标集合是前缀。
    let n = chain.steps.len();
    let fee_denom = chain.fee_denom;
    let base_a = chain.prefix_a[j];
    let feasible = |m: usize| -> bool {
        let delta = match chain.prefix_a[m].checked_sub(base_a) {
            Some(d) => d,
            None => return false,
        };
        let left = match amount_remaining.checked_sub(delta) {
            Some(v) => v,
            None => return false,
        };
        match (
            left.checked_mul(fee_denom),
            chain.envelope[m].checked_mul(U256::from(FEE_SCALE)),
        ) {
            (Some(l), Some(r)) => l >= r,
            _ => false,
        }
    };

    if !feasible(j) {
        return None;
    }
    let mut lo = j;
    let mut hi = n - 1;
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if feasible(mid) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let k = lo;
    let steps = k - j + 1;
    if steps < MIN_JUMP_STEPS {
        return None;
    }
    let consumed = chain.prefix_a[k + 1].checked_sub(base_a)?;
    let amount_out = chain.prefix_out[k + 1].checked_sub(chain.prefix_out[j])?;
    let last = chain.steps[k];
    // limit 必须落在跳步区间之外：
    //   zero_for_one（价格下行）区间内最低价 = 末步 end_sqrt ⇒ 需 limit <= end_sqrt；
    //   反之（价格上行）区间内最高价 = 末步 end_sqrt ⇒ 需 limit >= end_sqrt。
    let limit_outside = if key.zero_for_one {
        sqrt_price_limit <= last.end_sqrt
    } else {
        sqrt_price_limit >= last.end_sqrt
    };
    if !limit_outside {
        return None;
    }
    Some(Jump {
        steps: steps as u32,
        consumed,
        amount_out,
        end_tick: last.end_tick,
        end_sqrt: last.end_sqrt,
    })
}

/// 清空缓存（测试/排障）。
pub fn clear() {
    for s in shards() {
        match s.lock() {
            Ok(mut g) => g.clear(),
            Err(p) => p.into_inner().clear(),
        }
    }
    TOTAL_STEPS.store(0, Ordering::Relaxed);
}

/// 当前缓存的链数 / 步数（排障用）。
pub fn stats() -> (usize, usize) {
    let mut entries = 0usize;
    let mut steps = 0usize;
    for s in shards() {
        let g = match s.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        entries += g.len();
        steps += live_steps(&g);
    }
    (entries, steps)
}

// ---------------------------------------------------------------------------
// 差分测试：参考走步（与原循环逐行同构）vs 跳步版，逐位比对
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amms::consts::U256_1;
    use alloy::primitives::I256;
    use std::collections::HashMap as Map;
    use uniswap_v3_math::swap_math::compute_swap_step;
    use uniswap_v3_math::tick_bitmap::next_initialized_tick_within_one_word;
    use uniswap_v3_math::tick_math::{
        get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, MAX_SQRT_RATIO, MAX_TICK, MIN_SQRT_RATIO,
        MIN_TICK,
    };

    struct RefPool {
        tick_bitmap: Arc<Bitmap>,
        ticks: Map<i32, i128>,
        liquidity: u128,
        fee: u32,
        tick_spacing: i32,
        tick: i32,
        sqrt_price: U256,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Outcome {
        amount_out: U256,
        end_tick: i32,
        end_liquidity: u128,
        remaining: U256,
        err: Option<String>,
        loops: u32,
    }

    impl Outcome {
        /// 语义逐位比较（不含循环次数：跳步本来就会少循环）。
        fn semantic_eq(&self, o: &Self) -> bool {
            self.amount_out == o.amount_out
                && self.end_tick == o.end_tick
                && self.end_liquidity == o.end_liquidity
                && self.remaining == o.remaining
                && self.err == o.err
        }
    }

    /// 与原 `simulate_swap`（exact-in）循环逐行同构；只有「跳步」开关不同。
    fn walk(p: &RefPool, zero_for_one: bool, amount_in: U256, use_table: bool) -> Outcome {
        let limit = if zero_for_one {
            MIN_SQRT_RATIO + U256_1
        } else {
            MAX_SQRT_RATIO - U256_1
        };
        walk_with_limit(p, zero_for_one, amount_in, use_table, limit)
    }

    /// 同 `walk`，但价格上下限由调用方给定（生产里恒为极值，仅测试用得到）。
    fn walk_with_limit(
        p: &RefPool,
        zero_for_one: bool,
        amount_in: U256,
        use_table: bool,
        sqrt_price_limit_x_96: U256,
    ) -> Outcome {
        let mut loops: u32 = 0;
        let res = walk_inner(
            p,
            zero_for_one,
            amount_in,
            use_table,
            sqrt_price_limit_x_96,
            &mut loops,
        );
        match res {
            Ok((amount_out, end_tick, end_liquidity, remaining)) => Outcome {
                amount_out,
                end_tick,
                end_liquidity,
                remaining,
                err: None,
                loops,
            },
            Err(e) => Outcome {
                amount_out: U256::ZERO,
                end_tick: 0,
                end_liquidity: 0,
                remaining: U256::ZERO,
                err: Some(e),
                loops,
            },
        }
    }

    fn walk_inner(
        p: &RefPool,
        zero_for_one: bool,
        amount_in: U256,
        use_table: bool,
        sqrt_price_limit_x_96: U256,
        loops: &mut u32,
    ) -> Result<(U256, i32, u128, U256), String> {
        let mut sqrt_price_x_96 = p.sqrt_price;
        let mut amount_calculated = I256::ZERO;
        let mut amount_specified_remaining = I256::from_raw(amount_in);
        let mut tick = p.tick;
        let mut liquidity = p.liquidity;

        let mut recorder = if use_table {
            Recorder::new(true)
        } else {
            Recorder::disabled()
        };
        let mut empty_words: i32 = 0;
        let mut jump_checked = false;

        while amount_specified_remaining != I256::ZERO && sqrt_price_x_96 != sqrt_price_limit_x_96 {
            *loops += 1;
            let tick_before = tick;
            let sqrt_before = sqrt_price_x_96;

            let (tick_next_raw, initialized) = next_initialized_tick_within_one_word(
                &p.tick_bitmap,
                tick,
                p.tick_spacing,
                zero_for_one,
            )
            .map_err(|e| format!("{e:?}"))?;
            let tick_next = tick_next_raw.clamp(MIN_TICK, MAX_TICK);
            let tick_clamped = tick_next != tick_next_raw;
            let sqrt_price_next_x96 =
                get_sqrt_ratio_at_tick(tick_next).map_err(|e| format!("{e:?}"))?;

            let swap_target_sqrt_ratio = if zero_for_one {
                if sqrt_price_next_x96 < sqrt_price_limit_x_96 {
                    sqrt_price_limit_x_96
                } else {
                    sqrt_price_next_x96
                }
            } else if sqrt_price_next_x96 > sqrt_price_limit_x_96 {
                sqrt_price_limit_x_96
            } else {
                sqrt_price_next_x96
            };

            let (sqrt_after, amount_in_step, amount_out, fee_amount);
            if liquidity == 0 {
                sqrt_after = swap_target_sqrt_ratio;
                amount_in_step = U256::ZERO;
                amount_out = U256::ZERO;
                fee_amount = U256::ZERO;
            } else {
                (sqrt_after, amount_in_step, amount_out, fee_amount) = compute_swap_step(
                    sqrt_price_x_96,
                    swap_target_sqrt_ratio,
                    liquidity,
                    amount_specified_remaining,
                    p.fee,
                )
                .map_err(|e| format!("{e:?}"))?;
            }
            sqrt_price_x_96 = sqrt_after;

            amount_specified_remaining = amount_specified_remaining
                .overflowing_sub(I256::from_raw(amount_in_step.overflowing_add(fee_amount).0))
                .0;
            amount_calculated -= I256::from_raw(amount_out);

            let landed = sqrt_price_x_96 == sqrt_price_next_x96;
            if landed {
                if initialized {
                    let mut liquidity_net = *p
                        .ticks
                        .get(&tick_next)
                        .ok_or_else(|| "TickDataMissing".to_string())?;
                    if zero_for_one {
                        liquidity_net = -liquidity_net;
                    }
                    liquidity = if liquidity_net < 0 {
                        if liquidity < (-liquidity_net as u128) {
                            return Err("LiquidityUnderflow".to_string());
                        } else {
                            liquidity - (-liquidity_net as u128)
                        }
                    } else {
                        liquidity + (liquidity_net as u128)
                    };
                }
                tick = if zero_for_one {
                    tick_next.wrapping_sub(1)
                } else {
                    tick_next
                };
            } else if sqrt_price_x_96 != sqrt_before {
                tick = get_tick_at_sqrt_ratio(sqrt_price_x_96).map_err(|e| format!("{e:?}"))?;
            }

            if !use_table {
                continue;
            }

            // ---- 跳步整合（与生产代码同一套调用）----
            let key = RunKey::new(
                &p.tick_bitmap,
                zero_for_one,
                p.tick_spacing,
                liquidity,
                p.fee,
            );
            let chainable = landed
                && !initialized
                && !tick_clamped
                && swap_target_sqrt_ratio == sqrt_price_next_x96
                && liquidity >= 1;
            recorder.observe(
                &p.tick_bitmap,
                key,
                tick_before,
                sqrt_before,
                SpanStep {
                    end_tick: tick,
                    end_sqrt: sqrt_price_x_96,
                    amount_in: amount_in_step,
                    amount_out,
                    fee_amount,
                },
                chainable,
                landed,
            );
            if chainable {
                empty_words += 1;
                if !jump_checked && empty_words >= GATE_EMPTY_WORDS {
                    jump_checked = true;
                    if let Some(j) = try_jump(
                        &p.tick_bitmap,
                        key,
                        tick,
                        sqrt_price_x_96,
                        amount_specified_remaining.into_raw(),
                        sqrt_price_limit_x_96,
                    ) {
                        if let Some(rest) = amount_specified_remaining
                            .into_raw()
                            .checked_sub(j.consumed)
                        {
                            amount_specified_remaining = I256::from_raw(rest);
                            amount_calculated -= I256::from_raw(j.amount_out);
                            sqrt_price_x_96 = j.end_sqrt;
                            tick = j.end_tick;
                            empty_words += j.steps as i32;
                            jump_checked = false;
                            recorder.flush(&p.tick_bitmap, true);
                        }
                    }
                }
            } else {
                empty_words = 0;
                jump_checked = false;
            }
        }

        recorder.finish(&p.tick_bitmap);
        let amount_out = (-amount_calculated).into_raw();
        Ok((
            amount_out,
            tick,
            liquidity,
            amount_specified_remaining.into_raw(),
        ))
    }

    fn next_rand(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    /// 造一个「活跃流动性被锁在远处 + 起点在长空 word 段里」的池子（dust 形状）。
    fn build_pool(seed: &mut u64, spacing: i32, remote_words: i32) -> RefPool {
        let mut bitmap: Bitmap = Bitmap::new();
        let mut ticks: Map<i32, i128> = Map::new();

        // 远端（走步方向下游）一个 initialized tick：L 变化点
        let far_c = -remote_words * TICKS_PER_WORD + 100;
        *bitmap.entry((far_c >> 8) as i16).or_insert(U256::ZERO) |=
            U256::from(1u8) << ((far_c % 256) as usize);
        ticks.insert(far_c * spacing, 5_000);
        // 反方向也放一个，覆盖 ofz 走步
        let up_c = remote_words * TICKS_PER_WORD + 100;
        *bitmap.entry((up_c >> 8) as i16).or_insert(U256::ZERO) |=
            U256::from(1u8) << ((up_c % 256) as usize);
        ticks.insert(up_c * spacing, -5_000);
        // 当前 word 内一个 initialized tick：覆盖「链在 word 内断掉」的分支
        let near_c = 200;
        if near_c != far_c && near_c != up_c {
            *bitmap.entry((near_c >> 8) as i16).or_insert(U256::ZERO) |=
                U256::from(1u8) << ((near_c % 256) as usize);
            ticks.insert(near_c * spacing, 3_000);
        }

        let (tick, liquidity) = if spacing == 1 {
            (0, 1_000_000u128 + (next_rand(seed) % 4) as u128)
        } else {
            (0, 1_000_000u128 * spacing as u128)
        };

        RefPool {
            tick_bitmap: Arc::new(bitmap),
            ticks,
            liquidity,
            fee: 9,
            tick_spacing: spacing,
            tick,
            sqrt_price: get_sqrt_ratio_at_tick(tick).unwrap(),
        }
    }

    fn amounts(seed: &mut u64, n: usize) -> Vec<U256> {
        (0..n)
            .map(|_| {
                let bits = (next_rand(seed) % 62) as u32;
                let base = U256::from(1u8) << bits;
                base * U256::from(1 + next_rand(seed) % 97)
            })
            .collect()
    }

    /// 核心断言：跳步版与逐 word 版**逐位相同**。
    fn assert_same(p: &RefPool, label: &str) {
        for &zfo in &[true, false] {
            let mut s = 0x9E3779B97F4A7C15u64;
            let list = amounts(&mut s, 24);
            for a in &list {
                let reference = walk(p, zfo, *a, false);
                let optimized = walk(p, zfo, *a, true);
                assert!(
                    reference.semantic_eq(&optimized),
                    "{label}: 跳步与逐 word 结果不一致 (zfo={zfo}, amount={a})\n  ref={reference:?}\n  opt={optimized:?}"
                );
            }
        }
    }

    #[test]
    fn warm_cache_is_bit_exact_on_dust_shape() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        let p = build_pool(&mut seed, 1, 24);
        // 冷启动（第一次走全量并建表）
        assert_same(&p, "cold");
        // 热缓存（后续走步应该跳步，结果仍必须逐位相同）
        assert_same(&p, "warm");
    }

    #[test]
    fn jump_actually_reduces_loop_count() {
        clear();
        let mut seed = 42u64;
        let p = build_pool(&mut seed, 1, 24);
        let a = U256::from(10u64).pow(U256::from(18u8));
        let cold = walk(&p, true, a, true);
        let warm = walk(&p, true, a, true);
        assert!(cold.semantic_eq(&warm), "cold={cold:?} warm={warm:?}");
        assert!(warm.loops < cold.loops, "warm={warm:?} cold={cold:?}");
        assert!(
            warm.loops * 3 < cold.loops,
            "跳步后循环次数应显著下降：cold={} warm={}",
            cold.loops,
            warm.loops
        );
    }

    #[test]
    fn bitmap_change_invalidates_table() {
        clear();
        let mut seed = 7u64;
        let mut p = build_pool(&mut seed, 1, 24);
        assert_same(&p, "before-change");

        // 在空 word 段中间插入一个新的 initialized tick（L 不变）
        let mid_c = -12 * TICKS_PER_WORD + 55;
        let mut new_bitmap = (*p.tick_bitmap).clone();
        *new_bitmap.entry((mid_c >> 8) as i16).or_insert(U256::ZERO) |=
            U256::from(1u8) << ((mid_c % 256) as usize);
        p.tick_bitmap = Arc::new(new_bitmap);
        p.ticks.insert(mid_c * p.tick_spacing, 1_000);

        // 指针变了 ⇒ 旧表失效；结果仍必须与逐 word 版逐位相同
        assert_same(&p, "after-change");
    }

    #[test]
    fn fee_and_spacing_variants_are_bit_exact() {
        clear();
        let mut seed = 0xdead_beefu64;
        for spacing in [1i32, 10, 60] {
            for remote_words in [3i32, 12, 40] {
                let p = build_pool(&mut seed, spacing, remote_words);
                let mut s = 0xABCD_1234u64;
                for zfo in [true, false] {
                    for a in amounts(&mut s, 12) {
                        let reference = walk(&p, zfo, a, false);
                        let optimized = walk(&p, zfo, a, true);
                        assert!(
                            reference.semantic_eq(&optimized),
                            "spacing={spacing} words={remote_words} zfo={zfo} amount={a}\n  ref={reference:?}\n  opt={optimized:?}"
                        );
                    }
                }
            }
        }
    }

    /// 生产里 limit 恒为极值；这里专门喂一个**落在长空 word 段内部**的 limit，
    /// 验证跳步不会越过它（否则原循环停在 limit，跳步会冲过去 ⇒ 结果不一致）。
    #[test]
    fn tight_price_limit_never_overshoots() {
        clear();
        let mut seed = 0x0BAD_F00Du64;
        let p = build_pool(&mut seed, 1, 24);
        let big = U256::from(10u64).pow(U256::from(30u8));
        // 冷启动：先用极值 limit 建表（覆盖整条空 word 段）
        let _ = walk(&p, true, big, true);
        let _ = walk(&p, false, big, true);

        for &zfo in &[true, false] {
            // limit 落在离当前价 4 个 word 处（链覆盖范围远比它长）
            let limit_tick = if zfo {
                p.tick - 4 * TICKS_PER_WORD
            } else {
                p.tick + 4 * TICKS_PER_WORD
            };
            let limit = get_sqrt_ratio_at_tick(limit_tick).unwrap();
            let reference = walk_with_limit(&p, zfo, big, false, limit);
            let optimized = walk_with_limit(&p, zfo, big, true, limit);
            assert!(
                reference.semantic_eq(&optimized),
                "tight limit zfo={zfo} limit_tick={limit_tick}\n  ref={reference:?}\n  opt={optimized:?}"
            );
            // 非空洞的断言：limit 确实把走步截住了（否则本用例没有意义）
            assert!(
                reference.err.is_none(),
                "参考走步报错: zfo={zfo} ref={reference:?}"
            );
            assert!(
                reference.remaining > U256::ZERO,
                "走步在 limit 前就把金额耗尽，用例退化: zfo={zfo} ref={reference:?}"
            );
            if zfo {
                assert!(
                    reference.end_tick <= limit_tick
                        && reference.end_tick >= limit_tick - 2 * TICKS_PER_WORD,
                    "end_tick 未停在 limit 附近: {reference:?}"
                );
            } else {
                assert!(
                    reference.end_tick >= limit_tick
                        && reference.end_tick <= limit_tick + 2 * TICKS_PER_WORD,
                    "end_tick 未停在 limit 附近: {reference:?}"
                );
            }
        }
    }

    #[test]
    fn fee_variants_are_bit_exact() {
        clear();
        let mut seed = 99u64;
        let mut p = build_pool(&mut seed, 1, 20);
        for fee in [0u32, 1, 9, 100, 500, 3000, 10_000, 100_000, 999_999] {
            p.fee = fee;
            let mut s = 0x5151u64;
            for a in amounts(&mut s, 10) {
                let reference = walk(&p, true, a, false);
                let optimized = walk(&p, true, a, true);
                assert!(
                    reference.semantic_eq(&optimized),
                    "fee={fee} amount={a}\n  ref={reference:?}\n  opt={optimized:?}"
                );
            }
        }
    }
}

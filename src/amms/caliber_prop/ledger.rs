//! Caliber swap 增量账本（快照 rebase-merge 用）
//!
//! ## 为什么需要它
//!
//! Caliber 的 `reserve_a`/`reserve_b` 与 `cfg+7` 位置（`pos_forward`/
//! `pos_reverse`）是**纯累积量**：`Swap` 事件只在当前值上做 `+in / −out`，
//! **没有任何事件会把链上真值写进来**；唯一写真值的是直读存储的快照，
//! 而快照块 `S` 取自存储节点头（滞后 flashblock 乐观头 1~2 块）。若快照
//! 无条件覆盖，就会把"事件账本已扣到的最新值"打回旧值，且 **`Swap` 事件
//! 继续在错误基线上累积**——错误一直持续到下一次快照，而下次快照读的
//! 还是旧块（2026-09-10 事故：pair `0x5dda42ef…` 的 USDT0 储备在
//! 70255481~70255493 恒为 `3,849.017528`，本地却被按 `>= 5,255.86` 报价）。
//!
//! ## 语义
//!
//! `reserves = 快照(S) + Σ(块 > S 的 swap 事件净变化)`
//!
//! - 事件路径：每笔 swap 把 `(block, +in, −out)` 记入环形缓冲（同块累加、
//!   乱序/重复忽略、容量 [`CALIBER_LEDGER_BLOCK_WINDOW`] 块挤出跟踪
//!   `evicted_up_to`）。
//! - 快照路径：[`CaliberSwapLedger::rebase`]——
//!   - `S ≥ last_event_block` → `Anchored`：快照已含全部事件，直接锚定并清空账本；
//!   - `base ≤ S < last_event_block` → `Merged`：`current + (快照 − 基底) − Σ(base, S]`
//!     精确把基底推进到 `S`；
//!   - 重启/老状态（`base_block == 0`）→ `Merged`：快照 + 缓冲内 `> S` 的事件 replay；
//!   - `S < base` 或窗口挤出 / 乱序污染 → `SkippedStale` / `SkippedWindowMiss`：
//!     **保持事件账本，不写回**（绝不比旧行为差）。
//!
//! `pos` 与 reserves 同走账本，但链上 `cfg+7` 是**块门控**的（每块首笔写入前
//! 清零，实测 70255494 的 low96 == 当块 3 笔 `amountOut` 之和，未包含上一块
//! 70255481 的 1,406,848,718），因此账本按块存 pos 的**块终值**，
//! rebase 后取最后一个 `> S` 的块终值。
//!
//! ## 长期维护
//!
//! - 真实事件写 `reserve_*` / `pos_*` 的路径**只有 `apply_chain_swap` 一处**；
//!   新增写路径必须同步调用 [`CaliberSwapLedger::record_swap`]，否则 rebase 偏差。
//! - `consumed_*`（纯模拟状态）不进账本。
//! - 快照块号必须显式钉死（`eth_blockNumber`），不要用 `BlockId::latest()`
//!   的隐式块——rebase 数学依赖 `S` 与快照读取同块。

use alloy::primitives::U256;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// 环形缓冲容量（块数）。需覆盖 `(上一快照块, flashblock 乐观头]` 的窗口：
/// 对账间隔 30s、失败退避上限 300s、单轮存储拉取窗口十几秒，而 XLayer 约
/// 1 块/秒 → 1024 块（≈17 分钟）留足余量。超窗不是致命：退化分支会
/// 「快照重锚 + 重放缓冲内尾部事件」，下一轮即恢复正常精确合并。
pub const CALIBER_LEDGER_BLOCK_WINDOW: usize = 1024;

/// [`CaliberSwapLedger::rebase`] 的合并结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerApply {
    /// `S ≥ last_event_block`：快照覆盖全部事件，直接锚定（清空账本）
    Anchored,
    /// `base ≤ S < last_event_block`：rebase 精确合并
    Merged,
    /// 陈旧快照（早于当前基底）：丢弃，事件账本为准
    SkippedStale,
    /// 窗口挤出 / 乱序污染 / 无块号：退化 guard，事件账本为准
    SkippedWindowMiss,
}

/// 单块的净变化（块内聚合）
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct BlockDelta {
    d_reserve_a: i128,
    d_reserve_b: i128,
    /// 块终值（链上 `cfg+7` 块门控语义）
    pos_forward: U256,
    pos_reverse: U256,
}

/// Caliber swap 事件净变化账本（checkpoint + redo log）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaliberSwapLedger {
    /// 按块升序、块内聚合；(block, 净变化)
    entries: VecDeque<(u64, BlockDelta)>,
    /// 快照基底块（0 = 未锚定/重启），与其对应的链上真值
    base_block: u64,
    base_reserve_a: U256,
    base_reserve_b: U256,
    base_pos_forward: U256,
    base_pos_reverse: U256,
    /// 已记录事件的最高块
    last_event_block: u64,
    /// 缓冲已挤出的最高块（挤出后无法再做 prefix 计算）
    evicted_up_to: u64,
    /// 乱序/重复污染标记：置位后不再做 rebase 合并
    incomplete: bool,
}

impl Default for CaliberSwapLedger {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            base_block: 0,
            base_reserve_a: U256::ZERO,
            base_reserve_b: U256::ZERO,
            base_pos_forward: U256::ZERO,
            base_pos_reverse: U256::ZERO,
            last_event_block: 0,
            evicted_up_to: 0,
            incomplete: false,
        }
    }
}

/// U256 → i128（超范围时饱和；reserves 实际量级远小于 i128::MAX）
pub(crate) fn to_i128(v: U256) -> i128 {
    let max = U256::from(i128::MAX as u128);
    if v > max {
        i128::MAX
    } else {
        v.to::<u128>() as i128
    }
}

/// i128 → U256（负值截断为 0）
pub(crate) fn from_i128(v: i128) -> U256 {
    if v <= 0 {
        U256::ZERO
    } else {
        U256::from(v as u128)
    }
}

impl CaliberSwapLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否已锚定（有快照基底）。
    pub fn is_anchored(&self) -> bool {
        self.base_block != 0
    }

    pub fn last_event_block(&self) -> u64 {
        self.last_event_block
    }

    /// 最近一次锚定的快照块（0 = 尚未锚定）。
    ///
    /// 语义：块 `anchor_block` 末的链上真值已写入本地；因此**块号 ≤
    /// `anchor_block` 的 swap 已包含在快照内，必须跳过**，否则会重复入账
    /// （快照与实时流并发到达时的经典窗口）。
    pub fn anchor_block(&self) -> u64 {
        self.base_block
    }

    /// 被环形缓冲挤出的最高块（仅用于观测/告警）。
    pub fn evicted_up_to(&self) -> u64 {
        self.evicted_up_to
    }

    /// 记录一笔已应用到本地池子的 swap 净变化。
    ///
    /// `d_reserve_a` / `d_reserve_b` 必须是**实际生效**的增量（输出侧
    /// `saturating_sub` 后按真实扣减量记录）；`pos_forward` / `pos_reverse`
    /// 是应用本笔后的块终值（调用方已完成块门控清零）。
    pub fn record_swap(
        &mut self,
        block: u64,
        d_reserve_a: i128,
        d_reserve_b: i128,
        pos_forward: U256,
        pos_reverse: U256,
    ) {
        let prev_last = self.last_event_block;
        if block > prev_last {
            self.last_event_block = block;
        }

        if let Some((b, delta)) = self.entries.back_mut() {
            if *b == block {
                delta.d_reserve_a = delta.d_reserve_a.saturating_add(d_reserve_a);
                delta.d_reserve_b = delta.d_reserve_b.saturating_add(d_reserve_b);
                delta.pos_forward = pos_forward;
                delta.pos_reverse = pos_reverse;
                return;
            }
        }

        if block < prev_last {
            // 乱序（理论上被 last_synced_block 守卫挡住）：尽力归入已存在的块，
            // 并置 incomplete 让后续 rebase 退化为 guard，避免错误合并。
            self.incomplete = true;
            if let Some((_, delta)) = self.entries.iter_mut().find(|(b, _)| *b == block) {
                delta.d_reserve_a = delta.d_reserve_a.saturating_add(d_reserve_a);
                delta.d_reserve_b = delta.d_reserve_b.saturating_add(d_reserve_b);
                delta.pos_forward = pos_forward;
                delta.pos_reverse = pos_reverse;
            }
            return;
        }

        self.entries.push_back((
            block,
            BlockDelta {
                d_reserve_a,
                d_reserve_b,
                pos_forward,
                pos_reverse,
            },
        ));

        while self.entries.len() > CALIBER_LEDGER_BLOCK_WINDOW {
            if let Some((evicted, _)) = self.entries.pop_front() {
                self.evicted_up_to = self.evicted_up_to.max(evicted);
            }
        }
    }

    /// 把快照合并进当前池子状态：`current = 快照(S) + Σ(块 > S 的事件净变化)`。
    ///
    /// 直接改写传入的 `reserve_*` / `pos_*` / `pos_block`。所有分支都不会让
    /// 事件账本回退，也**不会停在"什么都不写"上**——退化分支以快照重锚并尽量
    /// 重放缓冲尾部，保证周期对账总能收敛（否则 `evicted_up_to` 只前进、
    /// 合并永久失效、偏差无界累积）。
    #[allow(clippy::too_many_arguments)]
    pub fn rebase(
        &mut self,
        reserve_a: &mut U256,
        reserve_b: &mut U256,
        pos_forward: &mut U256,
        pos_reverse: &mut U256,
        pos_block: &mut u64,
        snap_reserve_a: U256,
        snap_reserve_b: U256,
        snap_pos_forward: U256,
        snap_pos_reverse: U256,
        snap_block: u64,
    ) -> LedgerApply {
        if snap_block == 0 {
            // 无块号快照（历史遗留路径）：不做合并，保持事件账本。
            // 不置 `incomplete`——否则会连累之后带块号的合法快照退化为重锚。
            return LedgerApply::SkippedWindowMiss;
        }

        // 1) 陈旧快照（早于当前基底）：丢弃，事件账本为准。
        //    必须先于「锚定」判定——否则空账本 + 陈旧快照会把 base_block 回卷。
        if self.base_block != 0 && snap_block < self.base_block {
            return LedgerApply::SkippedStale;
        }

        // 2) 快照已覆盖全部已记录事件（且不比基底旧）：直接锚定
        if snap_block >= self.last_event_block {
            return self.anchor(
                reserve_a,
                reserve_b,
                pos_forward,
                pos_reverse,
                pos_block,
                snap_reserve_a,
                snap_reserve_b,
                snap_pos_forward,
                snap_pos_reverse,
                snap_block,
                LedgerApply::Anchored,
            );
        }

        // 3) 精确合并：`prefix = Σ (base, S]` 必须完整在窗口内。
        //    - `base_block == 0`（首次锚定/重启/老序列化状态）→ 无 prefix 可减，
        //      走 4) 的「重锚 + 重放」；
        //    - `base_block < evicted_up_to`：基底之后有事件被挤出 → 无法做差；
        //    - `incomplete`：存在乱序/未入账事件 → 缓冲不可信。
        if self.base_block != 0 && self.base_block >= self.evicted_up_to && !self.incomplete {
            let mut prefix_a = 0i128;
            let mut prefix_b = 0i128;
            let mut drop = 0usize;
            for (idx, (blk, d)) in self.entries.iter().enumerate() {
                if *blk > snap_block {
                    break;
                }
                prefix_a = prefix_a.saturating_add(d.d_reserve_a);
                prefix_b = prefix_b.saturating_add(d.d_reserve_b);
                drop = idx + 1;
            }
            for _ in 0..drop {
                self.entries.pop_front();
            }
            // current + (snap − base) − prefix == snap + Σ(S, last]
            let ra = to_i128(*reserve_a)
                .saturating_add(to_i128(snap_reserve_a))
                .saturating_sub(to_i128(self.base_reserve_a))
                .saturating_sub(prefix_a);
            let rb = to_i128(*reserve_b)
                .saturating_add(to_i128(snap_reserve_b))
                .saturating_sub(to_i128(self.base_reserve_b))
                .saturating_sub(prefix_b);
            *reserve_a = from_i128(ra);
            *reserve_b = from_i128(rb);
            self.sync_pos_from_entries(
                pos_forward,
                pos_reverse,
                pos_block,
                snap_pos_forward,
                snap_pos_reverse,
                snap_block,
            );
            self.base_block = snap_block;
            self.base_reserve_a = snap_reserve_a;
            self.base_reserve_b = snap_reserve_b;
            self.base_pos_forward = snap_pos_forward;
            self.base_pos_reverse = snap_pos_reverse;
            return LedgerApply::Merged;
        }

        // 4) 退化路径（首次锚定 / 窗口挤出 / 乱序污染）：「快照重锚 + 重放」。
        //    缓冲对 `> evicted_up_to` 的块是完整且有序的，因此只要
        //    `S >= evicted_up_to` 且未乱序，重放 `(S, last_event]` 的结果与
        //    精确合并**等价**；否则丢弃缓冲、纯以快照锚定（误差被限制在
        //    "快照之后到达的尾部事件"，且下一轮快照必然收敛）。
        while self
            .entries
            .front()
            .map(|(b, _)| *b <= snap_block)
            .unwrap_or(false)
        {
            self.entries.pop_front();
        }
        if self.incomplete || snap_block < self.evicted_up_to {
            self.entries.clear();
            return self.anchor(
                reserve_a,
                reserve_b,
                pos_forward,
                pos_reverse,
                pos_block,
                snap_reserve_a,
                snap_reserve_b,
                snap_pos_forward,
                snap_pos_reverse,
                snap_block,
                LedgerApply::Anchored,
            );
        }
        let mut replay_a = 0i128;
        let mut replay_b = 0i128;
        for (_, d) in self.entries.iter() {
            replay_a = replay_a.saturating_add(d.d_reserve_a);
            replay_b = replay_b.saturating_add(d.d_reserve_b);
        }
        *reserve_a = from_i128(to_i128(snap_reserve_a).saturating_add(replay_a));
        *reserve_b = from_i128(to_i128(snap_reserve_b).saturating_add(replay_b));
        self.sync_pos_from_entries(
            pos_forward,
            pos_reverse,
            pos_block,
            snap_pos_forward,
            snap_pos_reverse,
            snap_block,
        );
        self.base_block = snap_block;
        self.base_reserve_a = snap_reserve_a;
        self.base_reserve_b = snap_reserve_b;
        self.base_pos_forward = snap_pos_forward;
        self.base_pos_reverse = snap_pos_reverse;
        self.evicted_up_to = 0;
        self.incomplete = false;
        LedgerApply::Merged
    }

    /// 以快照为唯一真值锚定：写回快照值、清空事件账本、推进基底并复位
    /// 窗口/乱序标记（这样下一轮起可恢复精确合并）。
    #[allow(clippy::too_many_arguments)]
    fn anchor(
        &mut self,
        reserve_a: &mut U256,
        reserve_b: &mut U256,
        pos_forward: &mut U256,
        pos_reverse: &mut U256,
        pos_block: &mut u64,
        snap_reserve_a: U256,
        snap_reserve_b: U256,
        snap_pos_forward: U256,
        snap_pos_reverse: U256,
        snap_block: u64,
        result: LedgerApply,
    ) -> LedgerApply {
        *reserve_a = snap_reserve_a;
        *reserve_b = snap_reserve_b;
        *pos_forward = snap_pos_forward;
        *pos_reverse = snap_pos_reverse;
        *pos_block = snap_block;
        self.entries.clear();
        self.base_block = snap_block;
        self.base_reserve_a = snap_reserve_a;
        self.base_reserve_b = snap_reserve_b;
        self.base_pos_forward = snap_pos_forward;
        self.base_pos_reverse = snap_pos_reverse;
        self.evicted_up_to = 0;
        self.incomplete = false;
        result
    }

    /// pos（`cfg+7`）取最后一个 `> S` 的块终值；无尾部事件则取快照值。
    #[allow(clippy::too_many_arguments)]
    fn sync_pos_from_entries(
        &self,
        pos_forward: &mut U256,
        pos_reverse: &mut U256,
        pos_block: &mut u64,
        snap_pos_forward: U256,
        snap_pos_reverse: U256,
        snap_block: u64,
    ) {
        match self.entries.back() {
            Some((b, d)) => {
                *pos_forward = d.pos_forward;
                *pos_reverse = d.pos_reverse;
                *pos_block = *b;
            }
            None => {
                *pos_forward = snap_pos_forward;
                *pos_reverse = snap_pos_reverse;
                *pos_block = snap_block;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 以 (1000, 2000) / pos (7, 0) 为快照真值跑一次 rebase。
    fn rebase_vals(l: &mut CaliberSwapLedger, blk: u64) -> (U256, U256, u64, LedgerApply) {
        let mut ra = U256::ZERO;
        let mut rb = U256::ZERO;
        let mut pf = U256::ZERO;
        let mut pr = U256::ZERO;
        let mut pb = 0u64;
        let r = l.rebase(
            &mut ra,
            &mut rb,
            &mut pf,
            &mut pr,
            &mut pb,
            U256::from(1000u64),
            U256::from(2000u64),
            U256::from(7u64),
            U256::ZERO,
            blk,
        );
        (ra, rb, pb, r)
    }

    #[test]
    fn anchored_when_snapshot_covers_all_events() {
        let mut l = CaliberSwapLedger::new();
        l.record_swap(10, 5, -3, U256::from(3u64), U256::ZERO);
        let (ra, rb, pb, r) = rebase_vals(&mut l, 10);
        assert_eq!(r, LedgerApply::Anchored);
        assert_eq!((ra, rb, pb), (U256::from(1000u64), U256::from(2000u64), 10));
        assert_eq!(l.last_event_block(), 10);
    }

    #[test]
    fn merged_keeps_events_newer_than_snapshot() {
        let mut l = CaliberSwapLedger::new();
        let (_, _, _, r) = rebase_vals(&mut l, 100);
        assert_eq!(r, LedgerApply::Anchored);
        l.record_swap(101, 50, -20, U256::from(20u64), U256::ZERO);
        l.record_swap(102, 7, -1, U256::from(21u64), U256::ZERO);

        let mut ra = U256::from(1057u64);
        let mut rb = U256::from(1979u64);
        let mut pf = U256::from(21u64);
        let mut pr = U256::ZERO;
        let mut pb = 102u64;
        // 陈旧快照 S=100（读到的还是基准值）→ 必须保留 101/102 的净变化
        let r = l.rebase(
            &mut ra,
            &mut rb,
            &mut pf,
            &mut pr,
            &mut pb,
            U256::from(1000u64),
            U256::from(2000u64),
            U256::from(7u64),
            U256::ZERO,
            100,
        );
        assert_eq!(r, LedgerApply::Merged);
        assert_eq!(ra, U256::from(1057u64));
        assert_eq!(rb, U256::from(1979u64));
        assert_eq!(pf, U256::from(21u64));
        assert_eq!(pb, 102);

        // 再喂 S=101 的快照（已反映 101 的变化）→ 只保留 102 的变化
        let r = l.rebase(
            &mut ra,
            &mut rb,
            &mut pf,
            &mut pr,
            &mut pb,
            U256::from(1050u64),
            U256::from(1980u64),
            U256::from(20u64),
            U256::ZERO,
            101,
        );
        assert_eq!(r, LedgerApply::Merged);
        assert_eq!(ra, U256::from(1057u64));
        assert_eq!(rb, U256::from(1979u64));
        assert_eq!(pb, 102);
    }

    #[test]
    fn stale_snapshot_is_skipped() {
        let mut l = CaliberSwapLedger::new();
        let (_, _, _, r) = rebase_vals(&mut l, 100);
        assert_eq!(r, LedgerApply::Anchored);
        let (ra, _, _, r) = rebase_vals(&mut l, 99);
        assert_eq!(r, LedgerApply::SkippedStale);
        assert_eq!(ra, U256::ZERO);
    }

    /// 窗口挤出后**不能停在"什么都不写"**（否则周期对账永久失效、偏差无界
    /// 累积）：退化分支以快照重锚 + 重放缓冲内 `(S, last_event]` 的尾部事件，
    /// 并复位窗口标记，使下一轮恢复精确合并。
    #[test]
    fn window_miss_replays_tail_then_recovers() {
        let mut l = CaliberSwapLedger::new();
        let (_, _, _, r) = rebase_vals(&mut l, 100);
        assert_eq!(r, LedgerApply::Anchored);
        let last = 100 + CALIBER_LEDGER_BLOCK_WINDOW as u64 + 5;
        for b in 101..=last {
            l.record_swap(b, 1, -1, U256::from(1u64), U256::ZERO);
        }
        // 锚定块 100 之后的早期事件（101..105）已被挤出 → 无法做精确 prefix
        let (ra, rb, _, r) = rebase_vals(&mut l, 110);
        assert_eq!(r, LedgerApply::Merged);
        // 重放 111..=last 共 (last - 110) 笔
        let replayed = last - 110;
        assert_eq!(ra, U256::from(1000u64 + replayed));
        assert_eq!(rb, U256::from(2000u64 - replayed));

        // 重锚后窗口标记复位 → 下一轮恢复精确合并
        let (_, _, _, r) = rebase_vals(&mut l, 200);
        assert_eq!(r, LedgerApply::Merged);
    }

    /// 乱序污染（`incomplete`）时缓冲不可信 → 纯快照锚定，并复位标记。
    #[test]
    fn out_of_order_pollution_reanchors_and_recovers() {
        let mut l = CaliberSwapLedger::new();
        let (_, _, _, r) = rebase_vals(&mut l, 100);
        assert_eq!(r, LedgerApply::Anchored);
        l.record_swap(102, 10, -10, U256::from(1u64), U256::ZERO);
        l.record_swap(101, 10, -10, U256::from(1u64), U256::ZERO); // 乱序
        let (ra, rb, _, r) = rebase_vals(&mut l, 101);
        assert_eq!(r, LedgerApply::Anchored);
        assert_eq!((ra, rb), (U256::from(1000u64), U256::from(2000u64)));
        assert_eq!(l.anchor_block(), 101);
        // 标记复位：之后的精确合并可用（102 的条目已在重锚时清空，条目 101 被丢弃）
        let (_, _, _, r) = rebase_vals(&mut l, 200);
        assert_eq!(r, LedgerApply::Anchored);
    }

    #[test]
    fn cold_start_replays_events_after_snapshot() {
        let mut l = CaliberSwapLedger::new();
        l.record_swap(101, 50, -20, U256::from(20u64), U256::ZERO);
        l.record_swap(102, 7, -1, U256::from(21u64), U256::ZERO);
        // base_block = 0（重启）→ 快照(S=100) + replay(101,102)
        let (ra, rb, pb, r) = rebase_vals(&mut l, 100);
        assert_eq!(r, LedgerApply::Merged);
        assert_eq!(ra, U256::from(1057u64));
        assert_eq!(rb, U256::from(1979u64));
        assert_eq!(pb, 102);
    }
}

//! ElfomoFi vault 余额增量账本（checkpoint + redo log）。
//!
//! ## 为什么需要它
//!
//! `vault_xeth` / `vault_usdt0` 是**纯累积量**：只有 `ElfomoTrade` 事件在改它
//! （逐笔 `±amount`），链上绝对值只能靠周期快照 `token.balanceOf(vault)` 读到。
//! 两条通道没有统一时间锚点，所以只靠"覆盖"或"跳过"二选一都会坏：
//!
//! - 用快照覆盖 → 丢掉本地已应用的 `(S, R]` 区间增量（`R` = 本地已处理块）；
//! - 跳过快照 → 事件流漏掉的那一笔永远修不回来（只能等行情静默后快照才追上）。
//!
//! 本账本给出正确语义（`docs/dynamic_state_sync_principles.md` §3 规则 3）：
//!
//! ```text
//! current = anchor(块 S) + Σ_{块 > S} Δ(块)
//! ```
//!
//! 事件到达时记录净变化；快照落地时**以快照为锚、保留 `(S, ·]` 的条目**，
//! 于是快照永远能落地（不再需要"本地水位更高就跳过"这种会把唯一纠错通道
//! 饿死的判据），也不会丢增量。
//!
//! ## 不变式
//!
//! `current_signed() == anchor 值 + Σ 全部 entries` 恒成立：
//! 超窗挤出时把最老条目**折叠进锚点**（而不是丢弃），故挤出也不破坏真值。

use alloy::primitives::U256;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::ops::Bound::{Excluded, Unbounded};

/// 账本块窗口（内存上限）。需覆盖 `(上一快照块, flashblock 乐观头]`：
/// XLayer 约 1 块/秒，快照间隔 221s、失败退避上限 300s → 1024 块（≈17 分钟）
/// 留足余量。超窗按"折叠进锚点"处理，不丢真值。
pub const ELFOMO_LEDGER_BLOCK_WINDOW: usize = 1024;

/// 已入账日志键 `(block, log_index)` 的保留条数（挡重连补拉/重复投递）。
///
/// 与块窗口对齐（每块通常 1 笔成交 → 约覆盖 `ELFOMO_LEDGER_BLOCK_WINDOW` 块），
/// 不短于快照间隔；上游 `applied_log_dedup` 与 `block <= base_block` 锚点守卫
/// 是第二道防线，这里只是把"同块同日志重放"挡在账本之外。
const RECENT_LOG_KEYS: usize = ELFOMO_LEDGER_BLOCK_WINDOW;

/// 单块 vault 净变化（**有符号**，块内多笔成交聚合为块末净值）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultDelta {
    pub d_xeth: i128,
    pub d_usdt0: i128,
}

/// [`VaultDeltaLedger::rebase`] 的落地结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultLedgerApply {
    /// 快照覆盖全部已记录事件（无 `(S, ·]` 增量）→ 直接锚定
    Anchored,
    /// 快照与 `(S, ·]` 增量精确合并
    Merged,
    /// 快照早于当前锚点块 → 无法重建 `(S, base]` 增量，丢弃（保留事件账本）
    SkippedStale,
    /// 快照无块号（无法作为锚）→ 不做合并
    SkippedNoBlock,
}

/// U256 → i128（超范围饱和；vault 余额量级远小于 `i128::MAX`）。
fn to_i128(v: U256) -> i128 {
    let max = U256::from(i128::MAX as u128);
    if v > max {
        i128::MAX
    } else {
        v.to::<u128>() as i128
    }
}

/// i128 → U256（负值截断为 0；负值本身由调用方 `warn`，不在此静默掩盖）。
fn from_i128(v: i128) -> U256 {
    if v <= 0 {
        U256::ZERO
    } else {
        U256::from(v as u128)
    }
}

/// ElfomoFi vault 增量账本。
///
/// 与 `CaliberSwapLedger` 同形状（checkpoint + redo log），但因"存量只由
/// `ElfomoTrade` 改变"，只有两个累积字段，实现更简单。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultDeltaLedger {
    /// 是否已有链上绝对值锚点（`false` = 尚未 init/落地过快照）
    anchored: bool,
    /// 锚点块号（链上绝对值对应的块，快照为块末状态）
    base_block: u64,
    /// 锚点处的 vault 余额（有符号，便于折叠时精确相加）
    base_xeth: i128,
    base_usdt0: i128,
    /// 按块升序、块内聚合的净变化
    entries: BTreeMap<u64, VaultDelta>,
    /// 已记录事件的最高块（观测用）
    last_event_block: u64,
    /// 最近已入账的日志键 `(block, log_index)`，用于去重
    recent_logs: VecDeque<(u64, u64)>,
}

impl Default for VaultDeltaLedger {
    fn default() -> Self {
        Self {
            anchored: false,
            base_block: 0,
            base_xeth: 0,
            base_usdt0: 0,
            entries: BTreeMap::new(),
            last_event_block: 0,
            recent_logs: VecDeque::new(),
        }
    }
}

impl VaultDeltaLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_anchored(&self) -> bool {
        self.anchored
    }

    /// 锚点块号（`0` = 尚未锚定）。
    pub fn anchor_block(&self) -> u64 {
        if self.anchored {
            self.base_block
        } else {
            0
        }
    }

    pub fn last_event_block(&self) -> u64 {
        self.last_event_block
    }

    /// 当前在账的块条目数（观测用）。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 以块 `block` 的链上绝对值锚定（`init` / 快照落地）。
    ///
    /// 块 `block` 末的链上真值已包含**该块及更早**的全部成交，因此必须
    /// 清掉 `≤ block` 的条目，否则与锚点重复入账。
    pub fn anchor(&mut self, block: u64, vault_xeth: U256, vault_usdt0: U256) {
        self.anchored = true;
        self.base_block = block;
        self.base_xeth = to_i128(vault_xeth);
        self.base_usdt0 = to_i128(vault_usdt0);
        // 块号 0 = 无块级语义（仅测试/异常兜底）：不做 "≤ block 已覆盖" 的裁剪。
        if block != 0 {
            self.entries = self.entries.split_off(&block.saturating_add(1));
        }
    }

    /// 记录一笔已**实际应用**到本地池子的 vault 净变化。
    ///
    /// 返回 `false` 表示该笔被丢弃（重复回放 / 已含在快照锚点内），
    /// 调用方**不得**再改本地余额。
    pub fn record(
        &mut self,
        block: u64,
        log_index: Option<u64>,
        d_xeth: i128,
        d_usdt0: i128,
    ) -> bool {
        // 锚点（块末状态）已覆盖该块（含）→ 重复回放，丢弃
        if self.anchored && self.base_block != 0 && block <= self.base_block {
            return false;
        }
        if let Some(idx) = log_index {
            if self
                .recent_logs
                .iter()
                .any(|(b, i)| *b == block && *i == idx)
            {
                return false;
            }
            self.recent_logs.push_back((block, idx));
            while self.recent_logs.len() > RECENT_LOG_KEYS {
                self.recent_logs.pop_front();
            }
        }
        let entry = self.entries.entry(block).or_default();
        entry.d_xeth = entry.d_xeth.saturating_add(d_xeth);
        entry.d_usdt0 = entry.d_usdt0.saturating_add(d_usdt0);
        if block > self.last_event_block {
            self.last_event_block = block;
        }
        self.fold_overflow();
        true
    }

    /// 记录一笔 `ElfomoTrade`（按 pair 方向给出正的 `amount_in` / `amount_out`）。
    ///
    /// 金库是成交的双向对手方：账户给出什么金库就收进什么，账户收到什么金库
    /// 就付出什么。返回 `false` 同 [`Self::record`]（重复回放/已含在锚点内）。
    pub fn record_trade(
        &mut self,
        block: u64,
        log_index: Option<u64>,
        x_to_y: bool,
        amount_in: U256,
        amount_out: U256,
    ) -> bool {
        let (d_xeth, d_usdt0) = if x_to_y {
            (to_i128(amount_in), -to_i128(amount_out))
        } else {
            (-to_i128(amount_out), to_i128(amount_in))
        };
        self.record(block, log_index, d_xeth, d_usdt0)
    }

    /// 超窗时把最老条目**折叠进锚点**（保持 `anchor + Σ entries` 恒等于真值）。
    fn fold_overflow(&mut self) {
        while self.entries.len() > ELFOMO_LEDGER_BLOCK_WINDOW {
            let Some((&block, &delta)) = self.entries.iter().next() else {
                break;
            };
            self.entries.remove(&block);
            self.base_xeth = self.base_xeth.saturating_add(delta.d_xeth);
            self.base_usdt0 = self.base_usdt0.saturating_add(delta.d_usdt0);
            // 折叠后锚点语义 = "块 block 末的链上真值"
            self.base_block = self.base_block.max(block);
            self.anchored = true;
        }
    }

    /// 当前余额的**有符号**视图（负值说明本地账本与链上不一致，须 `warn`）。
    pub fn current_signed(&self) -> (i128, i128) {
        let mut x = if self.anchored { self.base_xeth } else { 0 };
        let mut u = if self.anchored { self.base_usdt0 } else { 0 };
        for delta in self.entries.values() {
            x = x.saturating_add(delta.d_xeth);
            u = u.saturating_add(delta.d_usdt0);
        }
        (x, u)
    }

    /// 当前余额（负值截断为 0；诊断用 [`Self::current_signed`]）。
    pub fn current(&self) -> (U256, U256) {
        let (x, u) = self.current_signed();
        (from_i128(x), from_i128(u))
    }

    /// 把块 `snap_block` 末的链上绝对值合并进账本：
    /// `current = 快照(S) + Σ_{块 > S} 增量`，并裁掉 `≤ S` 的条目。
    ///
    /// 唯一不落地的情况是**快照早于当前锚点块**（`SkippedStale`）——那时
    /// `(S, base]` 的增量已不在账本里，重建必然错，只能等更新的快照。
    pub fn rebase(
        &mut self,
        snap_block: u64,
        snap_xeth: U256,
        snap_usdt0: U256,
    ) -> VaultLedgerApply {
        if snap_block == 0 {
            return VaultLedgerApply::SkippedNoBlock;
        }
        if self.anchored && self.base_block != 0 && snap_block < self.base_block {
            return VaultLedgerApply::SkippedStale;
        }
        let had_newer = self
            .entries
            .range((Excluded(snap_block), Unbounded))
            .next()
            .is_some();
        self.anchor(snap_block, snap_xeth, snap_usdt0);
        if had_newer {
            VaultLedgerApply::Merged
        } else {
            VaultLedgerApply::Anchored
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(v: u64) -> U256 {
        U256::from(v)
    }

    #[test]
    fn record_aggregates_per_block_and_sums_signed_deltas() {
        let mut l = VaultDeltaLedger::new();
        l.anchor(100, u(1_000), u(2_000));
        assert!(l.record(105, Some(0), 50, -30));
        assert!(l.record(105, Some(1), 25, -10));
        assert!(l.record(106, Some(0), -5, 7));

        // 同块聚合 + 全量有符号求和
        assert_eq!(l.len(), 2);
        assert_eq!(
            l.current_signed(),
            (1_000 + 50 + 25 - 5, 2_000 - 30 - 10 + 7)
        );
        assert_eq!(l.last_event_block(), 106);
        assert_eq!(l.anchor_block(), 100);
    }

    #[test]
    fn rebase_merges_newer_entries_instead_of_skipping() {
        // 核心修复：快照块落后于"本地已处理块"时不再跳过，而是 rebase 合并。
        let mut l = VaultDeltaLedger::new();
        l.anchor(100, u(1_000), u(2_000));
        l.record(105, Some(0), 50, -30);
        l.record(106, Some(1), 10, -5);

        // 快照读块 103 落后于本地事件块 106 → 仍然落地：
        // current = 快照(103) + Σ_{>103} = (snap + 60, snap − 35)
        let apply = l.rebase(103, u(9_000), u(8_000));
        assert_eq!(apply, VaultLedgerApply::Merged);
        assert_eq!(l.current(), (u(9_000 + 60), u(8_000 - 35)));
        assert_eq!(l.anchor_block(), 103);
        // 块 ≤ 103 的条目已被快照覆盖并裁掉
        assert!(l.entries.keys().all(|b| *b > 103));
    }

    #[test]
    fn rebase_anchors_when_snapshot_covers_every_entry() {
        let mut l = VaultDeltaLedger::new();
        l.anchor(100, u(1_000), u(2_000));
        l.record(105, Some(0), 50, -30);

        let apply = l.rebase(106, u(7_000), u(9_000));
        assert_eq!(apply, VaultLedgerApply::Anchored);
        assert_eq!(l.current(), (u(7_000), u(9_000)));
        assert!(l.is_empty());
    }

    #[test]
    fn rebase_skips_only_when_snapshot_precedes_anchor() {
        let mut l = VaultDeltaLedger::new();
        l.rebase(100, u(1_000), u(2_000));
        l.record(105, Some(0), 50, -30);

        // 早于锚点块：无法重建 (S, base] 增量 → 丢弃，账本不动
        assert_eq!(l.rebase(90, u(1), u(2)), VaultLedgerApply::SkippedStale);
        assert_eq!(l.current(), (u(1_050), u(1_970)));
        assert_eq!(l.anchor_block(), 100);

        // 无块号快照：不做合并
        assert_eq!(l.rebase(0, u(1), u(2)), VaultLedgerApply::SkippedNoBlock);
        assert_eq!(l.current(), (u(1_050), u(1_970)));
    }

    #[test]
    fn record_rejects_replay_and_post_anchor_blocks() {
        let mut l = VaultDeltaLedger::new();
        l.anchor(100, u(1_000), u(2_000));

        // 重复投递同一条日志（block+log_index 相同）→ 只入账一次
        assert!(l.record(105, Some(7), 50, -30));
        assert!(!l.record(105, Some(7), 50, -30));
        assert_eq!(l.current(), (u(1_050), u(1_970)));

        // 快照锚点已覆盖的块（≤ base_block）→ 丢弃，防止重复入账
        assert!(!l.record(100, Some(9), 999, -999));
        assert_eq!(l.current(), (u(1_050), u(1_970)));
    }

    #[test]
    fn overflow_folds_oldest_entries_into_anchor() {
        let mut l = VaultDeltaLedger::new();
        l.anchor(1, u(1_000), u(100_000));
        let n = ELFOMO_LEDGER_BLOCK_WINDOW + 3;
        for i in 0..n {
            assert!(l.record(2 + i as u64, None, 1, -1));
        }
        // 挤出只影响分解粒度，不影响真值
        assert_eq!(l.current(), (u(1_000 + n as u64), u(100_000 - n as u64)));
        assert!(l.len() <= ELFOMO_LEDGER_BLOCK_WINDOW);
        assert!(l.anchor_block() > 1, "折叠后锚点应前移");
    }

    #[test]
    fn negative_balance_is_visible_in_signed_view() {
        // 不静默掩盖负值：有符号视图暴露负余额，materialize 时截断为 0
        let mut l = VaultDeltaLedger::new();
        l.anchor(10, u(10), u(10));
        assert!(l.record(11, None, -25, -25));
        assert_eq!(l.current_signed(), (-15, -15));
        assert_eq!(l.current(), (U256::ZERO, U256::ZERO));
    }
}

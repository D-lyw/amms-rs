# AMM 动态状态同步 —— 主线设计与原则

> 面向：第一次接触本库的 AI / 工程师。
> 目的：先用 5 分钟建立"两条通道 + 合并语义"的心智模型，之后再看各协议模块就不会迷路。
> 相关：`docs/dynamic-state-sync-audit.md`（逐协议分层审计）、`docs/caliber_prop_internal.md`、
> `docs/binaryfi_prop_internal.md`、`docs/caliber_prop_realtime_sync_design.md`。

---

## 0. 一句话主线

**实时事件流是主通道（最快）；事件拿不到的数据用异步 RPC 快照兜底；
两者合并的唯一正确语义是「快照锚定块 S + 字段级水位保鲜 + 累积量 rebase」——
而不是用块级水位（`last_synced_block`）决定谁覆盖谁。**

---

## 1. 两条通道与各自职责

| | 实时通道 | 异步通道 |
|---|---|---|
| 来源 | 事件 logs / 链特有推送（Base `pendingLogs`、Arbitrum sequencer feed、XLayer flashblocks）+ 零事件池子的 **raw tx 提取** | `update()` / `sync_all_pools()` / 周期快照任务 / maintenance `Resync` |
| 延迟 | 亚秒级 | 秒级（受存储 RPC 头部滞后影响） |
| 覆盖范围 | 只有"有事件"或"能看到那笔交易"的字段 | 全量字段（含事件永远拿不到的运行期字段） |
| 固有风险 | 丢帧 / 漏事件 → 状态漂移 | 读到**旧块**数据 → 把实时已推进的新状态回卷 |

**原则：能走实时就走实时；异步只补实时拿不到的东西，且永远不允许它把实时已推进的状态打回去。**

---

## 2. 为什么必须有异步通道

不是所有动态状态都有事件：

- Curve NG/Legacy：`balances`、`stored_rates`、`price_scale`、`D`、ramp 中的 `amp`…
- BinaryFi：`quote`/容量/费率（L2 update 日志只有 price/ladder/点差，拿不到全部字段）
- **Caliber：报价更新 `batchUpdateParameters` 零事件**，只能靠 raw tx calldata 或 storage 快照
- Elfomo：`updatePrices` raw tx 本地直算
- Pendle / FluidDex / Balancer / RocketPool / Slipstream：利率、limits、rate provider、动态费…

→ 逐协议分层见 `docs/dynamic-state-sync-audit.md`。**异步通道不是"兜底冗余"，对零事件协议它就是唯一可用的权威来源。**

---

## 3. 合并语义（核心，改动前必须复述一遍）

异步快照落地时必须回答两个问题：

1. **这份快照对应哪个链上块 `S`？** —— 必须显式锚定，不能"`latest()` 了事"。
2. **本地这个字段是否已经比 `S` 新？**

### 规则 1 — 字段级水位优先
每个"既能被实时通道推进、又可能被快照覆盖"的字段都要有自己的水位：

- BinaryFi：`price_updated_block`（`apply_snapshot` 内 `price_updated_block >= snap_block → 不覆盖`）
- Caliber：`ladder.price_update_block / price_update_tx_index`（块粒度不够时用 `(block, tx_index)`）

落地判据：`字段水位 > S → 不覆盖该字段`（其余字段正常刷新）。

### 规则 2 — 块级水位**不能**当"新鲜度"判据
`last_synced_block` 的语义是"本地处理到哪个链上块"。在实时流下它**恒新**——
该池只要有**任何**事件/交易被处理就会推进（caliber 里连一笔 swap 日志都会
`last_synced_block = block_number`）。用它判断"快照是否过期"会**误杀唯一的纠错通道**。

这是 BinaryFi v1.19.6/v1.19.7（`6dd3305`）与 Caliber 事故（见 §6）**同一个坑**。

### 规则 3 — 累积量必须 rebase，不能覆盖
对"事件只增减、从不写真值"的累积量（BinaryFi `reserves`、Caliber 储备/`pos`），
快照只能当 checkpoint：

```
state = snapshot(S) + Σ_{块 > S} 事件净变化
```

- BinaryFi：`ReservesDeltaLedger`（`7c1e9d7`）。
- Caliber：`CaliberSwapLedger`（`src/amms/caliber_prop/ledger.rs`），记账的是
  `reserve_a/b`（**实际生效**增量，含 `saturating_sub` 截断）与 `pos_forward/reverse`
  （按 `cfg+7` 块门控语义记**块终值**）。分支：
  - `Anchored`：`S ≥ last_event_block` → 快照覆盖全部事件，清账本锚定；
  - `Merged`：精确 rebase（`current + (snap − base) − Σ(base, S]`）；
  - `SkippedStale`：`S < base` → 丢弃，事件账本为准；
  - 退化（首次锚定 / 窗口挤出 / 乱序污染）：**快照重锚 + 尽量重放缓冲尾部**，
    并复位窗口标记，保证下一轮恢复精确合并——**绝不能停在"什么都不写"上**，
    否则 `evicted_up_to` 只前进、周期对账永久失效、偏差无界累积。
- 事件路径必须与账本同步：新增任何写 `reserve_*` / `pos_*` 的路径都要
  `record_swap`；`consumed_*` 是纯模拟状态，不进账本。

### 规则 4 — 写回单调、原子
多字段写回在**同一个写锁**内整体落地，读者不能看到半新半旧；
`last_synced_block` 写回取 `max`，只前进不回退。

### 规则 5 — 快照 RPC 不得在 state 写锁内执行，也不得整只覆盖
三段式：**读锁只取元数据 → 无锁拉取（只读）→ 短写锁内基于 current existing 合并写回**。
否则整条实时管线（flashblock apply + 引擎读锁）会被跨链 RPC 停顿（`5144eb8` 教训）。

⚠️ **"读锁里克隆整池 → 锁外 RPC → 写锁内 `*existing = pool`"是比持锁 RPC 更隐蔽的坑**：
RPC 窗口（Caliber 全量对账十几秒）内实时流推进的储备/pos/报价/`last_synced_block`
会被整只覆盖丢弃，本地报价停在旧水位 → 幻影机会（2026-09-10 事故）。
合并必须作用在**写锁内当时的 existing** 上（BinaryFi 已在 `sync_services.rs` 这样做，
Caliber 2026-09-10 才补齐）。

---

## 4. 框架里的水位与守卫（**别混用**）

| 机制 | 语义 | 正确用法 | 反模式 |
|---|---|---|---|
| `last_synced_block` | 本地处理到哪个链上块 | 幂等去重、reorg/回卷保护、写回单调 | 当作"该池状态新鲜度"判据 |
| 字段水位（`price_update_*` / `price_updated_block`） | 该字段最近一次实时更新的 (block, tx) | 快照落地保鲜、实时事件幂等与补账 | 只写不读 |
| `DeferredStale` | 快照目标块 < 本地块级水位 → 推迟任务 | 仅当快照**确实会**回卷实时状态 | 用在"纠错任务"上 → 纠错被永久挡死 |
| `RetryLater` | 目标块尚未被存储 RPC 收录 | 乐观头领先存储头时的预期状态 | 降级读旧块当成功 |
| `SkippedStale` | 快照块 < 本地水基底 → 丢快照 | 事件完备的协议 | 事件不完备的协议（恢复通道会完全失效） |
| `anchor_block`（账本基底块） | "块 ≤ 它的状态已含在快照内" | 挡掉快照/实时流并发时同块 swap 的**重复入账** | 当作块级新鲜度判据 |

---

## 5. 零事件 / 事件不完备池子的特殊性

代表：**Caliber**（报价零事件）、**BinaryFi**（容量/费率）、Elfomo（raw tx 直算）、Fermi（Titan 流）。

对这类模块：

1. 实时通道是**有损**的（fire-once、无重放、无缺口检测），所以
   **异步快照不是兜底，而是唯一的正确性锚**。
2. 因此**任何用块级水位把快照挡住的逻辑，都会让本地永久停留在错误状态**。
3. 报价类字段 → 字段水位保鲜；累积量 → ledger rebase；
   **快照落地后必须 stamp 字段水位**（表示"S 及更早的更新已包含在快照内"），
   否则之后到达的旧实时事件还会把它打回去。
4. 实时通道的缺口要能自证：`(payload_id, index)` 连续性、订单/事件序号、
   或与规范块 `transactionIndex` 对账。

---

## 6. 踩坑记录（撞过的墙，按 commit）

| commit | 现象 | 结论 |
|---|---|---|
| `2926b91` v1.17.5 | Resync 用旧 canonical 快照回卷实时状态 | 加新鲜度守卫；**但判据用了块级水位，为零事件协议埋下 §3 规则 2 的坑** |
| `f71093a` | caliber 回滚交易（status=0x0）的 deadline 被当有效报价 | 实时更新必须按 receipt status 过滤 |
| `8816ed5` v1.17.4 | caliber 更新后 spot 缓存最长 30s 滞后 | 实时更新后立即 `refresh_prices()` |
| `25d44fc` v1.17.6 | BinaryFi 梯子被清空后容量残留 → 幻影报价 | 空梯子 = 容量权威归零 |
| `6dd3305` v1.19.7 | BinaryFi AsyncUpdate 快照被 `last_synced_block` 竞态丢弃 | **块级水位不可当新鲜度判据（规则 2）**；改用 `price_updated_block` 保鲜 |
| `7c1e9d7` v1.19.8 | reserves 被旧快照覆盖 → 幻影容量 15s、9 笔失败 | **累积量必须 rebase（规则 3）** |
| `5144eb8` v1.19.10 | 快照 RPC 在写锁内 → 实时管线 150~515ms 停顿 | **三段式，RPC 出锁（规则 5）** |
| `56fc14c` | caliber 尾部漏更新无法补账 | 水位改 `(block, tx_index)`，可补账 + 丢弃点留痕 |
| `6abd894`/`8a96aa5` v1.18.x | Resync 目标块超前存储 RPC 头被降级读旧块 | 新增 `RetryLater`，按目标块重试 |
| `4a99931` | caliber swap 日志断流 | 断流回补 + 对账周期 60s→30s |
| 2026-09-10 | caliber 幻影报价：pair `0x5dda42ef…` 链上 USDT0 储备 `3,849.02`，本地按 `>= 5,255.86` 报价 → 4 笔上链还款不足回滚 | 三条独立缺陷叠加：①周期对账 Phase-1 克隆整池、锁外 RPC、整只覆盖（丢弃窗口内实时事件）；②`apply_snapshot` 对累积量无条件赋值（无 rebase）；③全槽位 `BlockId::latest()`，储备与 ladder 读到的块漂移。修复：`CaliberSwapLedger` + `apply_snapshot_merged`（A 类 rebase/B 类水位保鲜/C 类覆盖）+ 快照块号显式钉死 + 对账锁内合并写回 |
| 2026-09-10 | caliber `pos`（`cfg+7`）跨块无限累加 | 链上 `cfg+7` 是**块门控**的（实测 70255494 的 low96 == 当块 3 笔 `amountOut` 之和，不含上一块），本地必须新块先清零；`pos_block` 字段复刻该语义 |

---

## 7. 新增协议 / 新增异步刷新接入 checklist

1. 这个字段能被事件覆盖吗？不能 → 归入异步通道，注册周期任务。
2. 该字段有自己的水位吗？没有就加（块粒度不够用 `(block, tx_index)`）。
3. 快照落地是否锚定显式块 `S`？是否按水位保鲜？是否 stamp 水位？
4. 累积量吗？是 → 做 ledger rebase，不要直接覆盖。
5. RPC 是否在写锁内？是 → 改三段式。
6. 实时通道有损吗？有损 → 必须能检测缺口，并能用规范块/快照补回来。
7. 有回归测试吗？至少要覆盖：旧快照不回卷、重复事件幂等、缺口可补账。

---

## 8. 关键代码索引

| 主题 | 位置 |
|---|---|
| 待同步队列 / 守卫 | `src/state_space/maintenance.rs`（`PendingSyncQueue`、`execute_pending_task`、`DeferredStale`/`RetryLater`/`SkippedStale`） |
| 快照周期任务 | `src/state_space/sync_services.rs`（`start_*_sync_task`） |
| 实时 apply 与水位 | `src/state_space/mod.rs`（`sync()`、`apply_caliber_updates`、`apply_logs_for_block_timed`） |
| XLayer 实时提取 | `src/state_space/xlayer_flashblocks.rs` |
| Caliber 池子 | `src/amms/caliber_prop/{mod.rs,types.rs,factory.rs}` |
| Caliber 累积量账本 | `src/amms/caliber_prop/ledger.rs`（`CaliberSwapLedger::record_swap/rebase`、`LedgerApply`、`anchor_block`） |
| BinaryFi 池子 | `src/amms/binaryfi_prop/`（`ReservesDeltaLedger`、`apply_l2_update_full`、`apply_snapshot`） |

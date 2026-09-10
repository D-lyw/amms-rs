# AGENTS.md — amms-rs

## 先读这个（5 分钟建立心智模型）

- **`docs/dynamic_state_sync_principles.md`** — 本库的**主线设计与原则**：
  实时事件通道 vs 异步 RPC 快照的职责划分、两者合并的**唯一正确语义**
  （快照锚定块 S + 字段级水位 + 累积量 rebase）、水位/守卫的正确用法与反模式、
  零事件池子（caliber / binaryfi / elfomo / fermi）的特殊性、踩坑索引。
  **任何涉及状态同步、快照、Resync/AsyncUpdate、`last_synced_block` 的改动前必读。**
- `docs/dynamic-state-sync-audit.md` — 逐协议的"事件是否够用"分层审计。
- `docs/<protocol>_internal.md` — 单协议链上逻辑逆向（caliber / binaryfi / elfomo / fermi）。
- `README.md` — 支持的协议与链一览。

## 硬性约定

1. **不要用块级水位判断"某个字段是否新鲜"。** `last_synced_block` 在实时流下恒新，
   拿它当新鲜度判据会误杀唯一的纠错通道（见 principles 文档 §3 规则 2 与踩坑记录）。
2. **异步快照落地必须显式锚定块 S，并按字段水位保鲜；累积量必须 rebase 不能覆盖。**
3. **快照 RPC 不得在 state 写锁内执行**（三段式：读锁分组 → 无锁拉取 → 短写锁写回）。
4. 新增 AMM 协议前先过 principles 文档 §7 的接入 checklist。

## 仓库

- 目标仓库：`amms-rs`（本库）；主要消费者：`dex-arbitrage`（`{ git = ..., branch = "main" }`）。
- 改动本库后，消费者需 `cargo update -p amms` 并提交 `Cargo.lock`。
- 测试：优先定向测试（`cargo test --lib <module>`）；全量 `cargo test --lib` 耗时长，按需再跑。

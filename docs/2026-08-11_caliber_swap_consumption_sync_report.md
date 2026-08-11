# Caliber flashblock 路径支持 swap 消耗事件同步（ladder consumption）— 实现与取证报告

- 日期：2026-08-11
- 链：XLayer（chainId 196）
- 版本：v1.16.12
- 需求：`docs/../attachments` 需求文档 §1-§8（2026-08-11）

## 1. 结论

已完成：XLayer flashblocks 实时同步路径现在解析 Caliber 市场合约的 `Swap` 事件，
按 `receipt.status == 0x1` 过滤后应用，更新本地 ladder 的 pos（`pos_forward`/`pos_reverse`）
与储备（`reserve_a`/`reserve_b`），使本地报价在"同块内发生 swap 消耗"后与链上一致。
事故块 67650064 重放路径由单测锁定：同块 tx#15 消费后本地 quote 输出 ≈ 链上
`0x128ae93`（19,443,347），不再产生 `214,408,536` 这类消耗前幻影报价。

## 2. 事故证据（块 67650064，tx#15）

- 事件签名 topic0：`0x36d90ab6736dbd42ac28b968350d068640e9aea3f7b807679fe64d2a50dcbb03`
- topics = `[sig, pairId(bytes32), caller]`；data = `[tokenIn, tokenOut, amountIn, amountOut, flags]`
- 实测：`amountIn=1,900,532,488,745,085,420`、`amountOut=416,820,481`
- tx#15 后链上 `quote(W→U, 977,689,766,888,449,551)` = `0x128ae93` = 19,443,347
  （本地 `apply_chain_swap` 后 `reserve_a` 逐位一致）
- 块末（tx#22 再消费 19,156,271）cfg+7 low96 = 435,976,752，本地模拟 quote = 287,076 与链上 cfg+5 封顶一致
- **反向 pos 语义（块 0x4089101 / 0x408910d 取证）**：cfg+7 mid96 不是"累计输出量"，
  而是 U→W 方向累计的**扣费后输入量** `amountIn_y - floor(amountIn_y * fee / 1e6)`
  （82,580,656 in → mid96=82,564,140；34,578,457 in → mid96=34,571,542），
  与 `quote_reverse_exact` 的 pos（y 单位）逐位一致。
- **受限 swap 的储备入账（tx#22 取证）**：输出被调用方限制时（amount_out <
  quote(amount_in)），链上只入账"产生该输出的 ladder 输入"（事件
  in=526,057,675,902,332,428、out=19,156,271，cfg+4 仅 +87,349,782,419,593,420），
  超出部分停留在合约余额、不进 pair 储备；本地 `ladder_input_for_output` 二分复刻
  平台区上沿，与链上偏差 < 2.2e9 wei（远低于 dust，周期对账兜底）。

## 3. 实现

| 位置 | 改动 |
|---|---|
| `src/amms/caliber_prop/mod.rs` | `CALIBER_SWAP_EVENT`、`CaliberSwapEvent`、`decode_caliber_swap_log`、`apply_chain_swap`（储备 + pos_forward/pos_reverse，方向切换归零；输入侧按 `ladder_input_for_output` 复刻受限入账，反向 pos 累计扣费后输入）；`quote_forward_pos_exact`/`quote_reverse_exact` 支持 low96/mid96 pos；`fetch_exact_snapshot` 拆 pos_reverse/pos_forward |
| `src/state_space/xlayer_flashblocks.rs` | `extract_logs_from_xlayer_flashblock` 增加 caliber swap 提取通道（`caliber_contracts` 地址预筛 + status==0x1 过滤 + dedup 占键）；流循环 `apply_caliber_swaps_for_block` |
| `src/state_space/mod.rs` | `apply_caliber_swaps`（pairId+合约 → virtual_address 路由、tx_index 排序、幂等块号保护） |
| `src/amms/caliber_prop/types.rs` | `pos` → `pos_reverse` + `pos_forward`（serde alias 兼容旧快照） |

纪律（与 2026-08-09 P0 一致）：失败/未确认交易事件一律不应用；pairId 不匹配静默跳过；
块号落后于池子已同步块跳过（禁止回卷）；`consumed_*` 为纯模拟状态不随真实事件累加
（避免双重计数）；周期对账（45s）保留为兜底，实时路径与快照路径对同一块应幂等一致。

## 4. 验证

- `cargo test --lib caliber`：36 passed（含事故块重放、正向/反向/方向切换、受限/未受限 ladder 入账、
  status=0x0 过滤、解码失败 fail-safe）
- `cargo test --lib state_space`：38 passed（含 swap 事件路由/乱序按 txIndex 应用/未知 pairId/块号回卷保护）
- `cargo check --lib`：通过（仅存量 warning）
- 已知存量问题（非本次引入）：examples `arbitrum_prod_manager_probe`、`xlayer_curve_ng_stable_realtime_check`
  引用已移除的 `reconcile_cursor` 字段无法编译，属 main 历史遗留，与本次改动无关。

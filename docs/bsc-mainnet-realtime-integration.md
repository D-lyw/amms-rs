# BSC (BNB Smart Chain) 集成实施方案 —— amms-rs 实时同步扩展

> 目标：让 `amms-rs` 的 `StateSpaceManager` 实时状态同步支持 BSC 主网（chain_id=56）。
> 实时通道采用 **WS `eth_subscribe("logs")` push 订阅**（主路径）+ **canonical getLogs 对账**（兜底），
> 复用 Base `pendingLogs` 管线的既有架构模式（push 主 + canonical_head_tracker 对账 + 去重）。
> 本方案只动 amms-rs；app 侧（dex-arbitrage）配套改动单列于 §5。

---

## 1. 现状梳理（代码事实）

### 1.1 实时源分发矩阵（`src/state_space/mod.rs`）

| 公开枚举 `RealtimeSyncSource` | 内部 `SelectedRealtimeSource` | 实现文件 | 适用链 |
|---|---|---|---|
| `Auto`（按 chain_id resolve） | — | `resolve_realtime_source` (:920) | 全部 |
| `WsLogs` | `NewHeadsPull` | `ws_logs.rs` | 非 Base/XLayer/Arb/Robinhood 默认 |
| `BaseFlashblocksRaw`（兼容名） | `BasePendingLogs` | `base_pending_logs.rs` | Base |
| `XlayerFlashblocksRaw` | `XlayerFlashblocksPull` | `xlayer_flashblocks.rs` | XLayer |
| `ArbitrumSequencerFeed` | `ArbitrumFeedPull` | `arbitrum_feed.rs` | Arbitrum / Robinhood |

关键事实：
- app 侧（`crates/core/src/bin/arbitrage.rs:132`）**只调用** `with_realtime_ws_endpoints(cfg.chain.ws_urls)`，不显式设置 `RealtimeSyncSource` → 全部走 `Auto`，由 amms 内部按 chain_id 分发。
- `with_realtime_ws_endpoints` 文档明确"非 Base 路径当前忽略这些端点"（`mod.rs:1997`）——BSC 需在 `build_realtime_stream` 中为 BSC 消费该配置。
- 共享件（所有管线复用，BSC 全部可继承）：
  - `build_query_chunks` (:1461)：协议感知分块，`TopicFiltered(全协议事件签名)` 或 `AddressOnly`，每块 ≤ `LOG_ADDRESS_CHUNK_SIZE=200` 地址；UniswapV4/Balancer Vault/Slipstream FeeModule/FoT 等特殊地址都在这里注册。
  - `initial_backfill_results` (:947)：启动 catch-up，期间更新抑制（不产出下游通知）。
  - `backfill_range` (:1367)：gap 补拉，窗口自适应收缩（provider 块范围上限安全）。
  - `AppliedLogDedupCache` (:309)：位置 key（`block-global log_index`）+ 跨源 content-hash 别名去重。
  - `run_canonical_head_tracker` (:1728)：`newHeads` 推进 `canonical_head`，唤醒 `pending_sync_worker`。
  - `run_pending_sync_worker` / `run_silent_drift_probe_task`：对账与漂移兜底。

### 1.2 Base `pendingLogs` 管线 = BSC 的模板（`base_pending_logs.rs`）

结构（要原样复刻的骨架）：
1. `initial_backfill_results(...)`（LogSource::RealtimeFlashblock）
2. 多候选 WSS 连接 failover（`connect_async` 逐个尝试）
3. 每 query chunk 一条 `eth_subscribe("pendingLogs", filter)`，逐条等待 ack（含超时/错误处理），`subscription_ids` 记录
4. 循环 `socket.next()`：Ping→Pong、Text/Binary 解析 JSON、校验 `method=="eth_subscription"` 且 sub_id 在册、`serde_json::from_value::<Log>` 解码、`pending_log_dedup` 预去重、`apply_logs_for_block(..., LogSource::RealtimeFlashblock)`、`yield Ok((meta, affected))`
5. 空闲超时 `STREAM_IDLE_TIMEOUT` / 断线 → 重连 loop（`STREAM_RECONNECT_DELAY`）

差异（BSC vs Base）：
- 订阅方法 `"logs"`（标准 geth）而非 `"pendingLogs"`（flashblocks 专有）。
- Base 的 push 日志是"预确认"（可能无稳定 logIndex）；BSC 标准 `logs` 订阅返回**已打包块的日志**，`blockNumber` + block-global `logIndex` 齐全 → 与 canonical backfill 位置 key 完全一致，去重更稳。
- Base 需要 `LogSource::RealtimeFlashblock` 语义；BSC 新增独立 `LogSource::BscLogsPush`（dedup 走 canonical 分支，同 `NewHeadsPull`，见 §3.5）。

### 1.3 对账兜底 gate（`ensure_background_tasks` :575）

```rust
if (chain_id == BASE_CHAIN_ID && matches!(selected, SelectedRealtimeSource::BasePendingLogs))
    || (chain_id == XLAYER_CHAIN_ID && matches!(selected, SelectedRealtimeSource::XlayerFlashblocksPull))
{
    tokio::spawn(run_canonical_head_tracker(...)); // newHeads → canonical_head → pending_sync_worker 对账
}
```
BSC 必须加入此 gate（push 链标配），否则 push 漏日志无兜底。

---

## 2. BSC 链特性 → 设计要求

| 特性 | 对设计的要求 |
|---|---|
| 0.45s 块（~192k 块/天，实测单块 ~79 tx / ~12M gas） | push 订阅为主，避免每块 getLogs 的 RPC 风暴（130+ 次/分起步） |
| 官方 dataseed 禁 `eth_getLogs`、无 WS、10K/5min 限流 | 主 RPC（getLogs/backfill）与实时 WSS 都必须第三方/自建节点 |
| 有公开 mempool（Geth 系） | pending 前视通道可行（app 侧，见 §5.3），与 realtime 并行 |
| Fast Finality ~1.125s（2 块）；存在 reorg 窗口 | canonical push 可能先推非最终块；执行侧需 finality 策略（core 负责） |
| 无 flashblocks | 用标准 `eth_subscribe logs` 替代，450ms 块周期即"预确认粒度" |
| 日志吞吐 ≈ ETH 的 10 倍量级 | 订阅 filter 必须精确（沿用 query chunks 的 topic 过滤）；日志量大时按块批量 apply |

---

## 3. amms-rs 代码改动设计（核心）

### 3.1 常量与枚举（`src/state_space/mod.rs`）

```rust
const BSC_MAINNET_CHAIN_ID: u64 = 56;

// LogSource 增加
BscLogsPush,

// SelectedRealtimeSource 增加
BscLogsPush,

// 公开枚举 RealtimeSyncSource 增加（显式配置用；Auto 也会命中 56）
/// BSC 主网实时同步：标准 `eth_subscribe("logs")` push 订阅 + canonical 对账。
/// 要求 `with_realtime_ws_endpoints` 提供支持 logs 订阅的 WSS。
BscMainnetLogsPush,
```

### 3.2 分发逻辑（`mod.rs`）

`resolve_realtime_source`：
```rust
RealtimeSyncSource::Auto => {
    if chain_id == BASE_CHAIN_ID { BasePendingLogs }
    else if chain_id == XLAYER_CHAIN_ID { XlayerFlashblocksPull }
    else if chain_id == ARBITRUM_CHAIN_ID || chain_id == ROBINHOOD_CHAIN_ID { ArbitrumFeedPull }
    else if chain_id == BSC_MAINNET_CHAIN_ID { BscLogsPush }   // 新增
    else { NewHeadsPull }
}
RealtimeSyncSource::BscMainnetLogsPush => SelectedRealtimeSource::BscLogsPush, // 新增
```

`build_realtime_stream`：新增 match arm，镜像 Base 的 ws 端点校验：
```rust
SelectedRealtimeSource::BscLogsPush => {
    let ws_candidates = self.realtime_ws_endpoints.clone().ok_or_else(|| /* 报错提示需 with_realtime_ws_endpoints */)?;
    Ok(Box::pin(Self::subscribe_bsc_logs_push_stream(provider, state, hooks, update_seq,
        realtime_head, canonical_head, pending_sync_queue, pending_sync_notify,
        applied_log_dedup, query_chunks, ws_candidates, chain_id)))
}
```

`ensure_background_tasks`：canonical_head_tracker gate 增加 BSC：
```rust
|| (chain_id == BSC_MAINNET_CHAIN_ID && matches!(selected, SelectedRealtimeSource::BscLogsPush))
```

### 3.3 新文件 `src/state_space/bsc_logs_push.rs`

以 `base_pending_logs.rs` 为模板，函数签名完全对齐：

```rust
pub(super) fn subscribe_bsc_logs_push_stream(
    provider, state, hooks, update_seq, realtime_head, canonical_head,
    pending_sync_queue, pending_sync_notify, applied_log_dedup,
    query_chunks, ws_candidates, chain_id,
) -> impl Stream<Item = Result<(RealtimeUpdateMeta, Vec<Address>), StateSpaceError>> + Send
```

实现要点：
1. **filter 构造**：把 `base_pending_logs.rs` 的 `chunk_to_pending_logs_filter` 泛化为 `chunk_to_subscription_filter`（移到 mod.rs 或公共模块，Base/BSC 共用）：
   ```json
   {"address": [...], "topics": [[sig0, sig1, ...]]}
   ```
   （`eth_subscribe("logs")` 下 `fromBlock` 省略即 `latest`；显式加 `"fromBlock":"latest"` 亦可。）
2. **订阅**：`"method":"eth_subscribe", "params":["logs", filter]`，逐 chunk 等待 ack（复用 Base 的 ack 循环：Ping/Pong、id 匹配、error、超时、`eth_subscription` 通知跳过）。
3. **消息处理**：与 Base 相同（Text/Binary → JSON → `method=="eth_subscription"` → sub_id 在册 → `serde_json::from_value::<Log>`）。
4. **防御**：`log.block_number` 为 `None` 直接丢弃（标准 logs 订阅不应出现，防御乱序/半成品推送）。
5. **去重与应用**：本地 `PendingLogDedupCache`（复用 Base 结构）预去重 → `apply_logs_for_block(..., LogSource::BscLogsPush)` → `yield Ok((meta, affected))`。
6. **重连**：多候选 WSS failover、`STREAM_IDLE_TIMEOUT`(60s) 空闲断线、`STREAM_RECONNECT_DELAY`(2s) 重连，全部沿用常量。
7. **日志**：连接/订阅数/断线原因/重连，与 Base 一致的 tracing 风格，`chain_id=56` 可检索。

### 3.4 链参数调整（`mod.rs`）

```rust
fn backfill_window_size(chain_id: u64) -> u64 {
    match chain_id {
        BSC_MAINNET_CHAIN_ID => 300,   // 0.45s 块；STREAM_IDLE_TIMEOUT 60s ≈ 133 块，300 覆盖断流+余量
        ... // 其余不变
    }
}
```
- 注意：部分 provider 对单次 `getLogs` 有块范围上限（1000~5000），300 安全；若个别 provider 更严，`backfill_range` 已有窗口自适应收缩兜底。
- `drift_probe_interval` 默认 120s 在 BSC 上 ≈ 266 块漂移窗口，偏长 → 在 app 配置层用 `with_drift_probe_interval` 调短（建议 30~60s），amms 默认值不动（避免影响其他链）。
- `LOG_ADDRESS_CHUNK_SIZE=200` 保持；BSC 池子多时订阅数 = ceil(池数/200)（含共享合约 chunk），注意第三方 WSS 单连接订阅数上限（常见 10~50），必要时按 provider 调低 chunk 或拆多连接。

### 3.5 去重与对账语义（正确性核心）

- `AppliedLogDedupCache::insert_log_if_new` 仅对 `XlayerFlashblock` 走专用 content-hash 计数分支；**其余 source（含新 `BscLogsPush`）走 canonical 分支**：稳定位置 key（blockNumber + block-global logIndex）+ 跨源 content 别名。
- 因此 BSC 的 push 与 canonical backfill 重叠时天然互斥去重：同一条日志无论先到（push）后到（backfill）只会 apply 一次。
- 对账闭环（与 Base 相同，两层）：
  1. **log 级**：断线/启动时 `initial_backfill_results` 以 `realtime_head+1..tip` 用 getLogs 补拉，与 push 已处理的按位置 key 去重；
  2. **状态级**：`canonical_head_tracker` 推进 `canonical_head` → `run_silent_drift_probe_task`（eth_call 探针检测漂移→入队 Resync）与 `run_maintenance_coverage_scheduler`（旧池周期覆盖）→ `pending_sync_worker` 执行池级重同步。
- 前提：**主 HTTP provider 必须支持 `eth_getLogs`**（官方 dataseed 不行），见 §5.1。

### 3.6 测试

| 用例 | 位置 |
|---|---|
| `resolve_realtime_source(Auto, 56) == BscLogsPush`；显式 `BscMainnetLogsPush` 同样命中 | mod.rs tests |
| `backfill_window_size(56) == 300` | 扩展现有 `backfill_window_size_is_chain_specific` |
| `chunk_to_subscription_filter`：TopicFiltered/AddressOnly 两种 chunk → JSON 结构断言 | bsc_logs_push.rs tests |
| 日志解码/防御：blockNumber=None 丢弃 | bsc_logs_push.rs tests |
| 集成探针 example（可选）：连真实 BSC WSS（如 `wss://bsc.drpc.org`），订阅 `logs` 跑 60s，统计接收/apply 数与 `realtime_head` 推进 | `examples/bsc_logs_probe.rs`（参考 `xlayer_flashblocks_receipt_probe`） |

---

## 4. 设计决策记录（为什么这样做）

1. **为什么 push 而非 newHeads+getLogs**：BSC 0.45s 块把 getLogs 轮询成本放大 27 倍且官方端点禁 getLogs；push 延迟更低（块产生即推送）、RPC 负载≈0。Ethereum 当年从 push 改 pull 是因为"无游标不可恢复"——本方案用 canonical 对账兜底解决该问题（Base 已验证的架构）。
2. **为什么标准 `logs` 而非 `pendingLogs`**：BSC 无 flashblocks 端点；标准 logs 订阅返回已打包块日志，位置 key 稳定，对账语义干净。mempool 级预确认留给 app 侧 pending 通道（`newPendingTransactions`）承担，职责分离。
3. **为什么新增 `LogSource::BscLogsPush` 而非复用 `RealtimeFlashblock`**：dedup 分支相同（canonical 分支），但独立枚举便于日志归因、后续差异化和单测。
4. **为什么不改 core 消费接口**：`subscribe_with_meta` 流签名不变，app 无需感知链差异（与现有 Base/XLayer 一致）。

---

## 5. dex-arbitrage app 侧配套（另立任务，简要）

### 5.1 链配置 `configs/chains/56.toml`
- `[chain] ws_urls = ["wss://<第三方 logs-capable WSS>"]`（Chainstack / Alchemy / QuickNode / GetBlock / NodeReal / drpc / 自建 `wss://127.0.0.1:8546`）。
- **主 RPC URL（provider）必须是 getLogs-capable 的第三方/自建节点**（官方 dataseed 禁用 getLogs，backfill/对账会失败）。
- `weth = 0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c`（WBNB）；`[gas_token] symbol="BNB"`；锚点代币 WBNB/USDT/USDC/BTCB；`[sync_intervals] drift_probe_interval_secs=30~60`。

### 5.2 pools-index
- 增加 BSC 子图端点：PancakeSwap v2/v3（BSC 主战场）、Uniswap v3（BSC 手动部署实例）等（`crates/pools_index/src/subgraphs.rs`）。

### 5.3 pending 前视（`crates/core/src/pending/`）
- `PendingSourceKind` 新增 `BscRpc`：`eth_subscribe("newPendingTransactions", true)`（full tx，喂现有 tx-intent-decoder），默认端点=自建/第三方 WSS；adapter 复用 `xlayer_rpc.rs` 模式。
- `registry.rs`：`(56, BscRpc)` 默认端点 + `default_sources(56) = vec![BscRpc]`。
- 后续可选：bloXroute BDN `pendingTxs` / BlockRazor Private Mempool（私有 orderflow backrun）。

### 5.4 执行层
- `TransactionSenderFactory` 增 `56 => BscTransactionSender`：`eth_sendRawTransaction`（自建/第三方）为主；bundle 通道 BlockRazor `eth_sendMevBundle` / BlockSec `eth_sendBundle`（复用 `ethereum.rs` 的 `send_bundle_generic`）。
- 0.45s 块竞价窗口 ~几百 ms：沿用 `tcp_nodelay` + keep-alive warmup 模式。

---

## 6. 分期实施与验收

| Phase | 内容 | 验收 |
|---|---|---|
| P1 | 枚举/常量/分发/backfill 参数 + 单测 | `cargo test -p amms --lib state_space::` 全绿 |
| P2 | `bsc_logs_push.rs` + filter 泛化 + 探针 example | 真实 BSC WSS 60s 探针：realtime_head 持续推进、无 panic、无重复 apply |
| P3 | canonical 对账验证 | 人为断流/杀订阅后，60s 内 `pending_sync_worker` 用 getLogs 补回缺失块 |
| P4 | app 配置 + pending + executor + pools-index | 端到端：realtime 通道延迟 < 1s；pending 前视收到 tx 流 |

## 7. 风险清单

- 第三方 WSS 抖动：多候选 failover + 重连已内置；对账兜底保证最终一致。
- 订阅数上限：BSC 池子规模大时 chunk 订阅数可能触顶 → 按 provider 调 `LOG_ADDRESS_CHUNK_SIZE` 或拆连接。
- provider `getLogs` 块范围上限：`backfill_range` 窗口自适应收缩已兜底。
- BSC reorg：Fast Finality 2 块内；执行依赖最终性时在 core 侧按 `finalized`/确认数处理（本方案不涉及）。
- 日志吞吐：push filter 已按 topic 精确过滤；如单连接带宽吃紧，按 chunk 拆分多条 WS 连接（Phase 3 观察点）。

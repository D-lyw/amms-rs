# Caliber propAMM 实时交易监控同步 — 设计与实现方案

> **实现状态（2026-08-07）**：M1-M3 已实现并合入工作区；M4 端到端真实流验证已完成：
> `cargo run --example caliber_prop_flashblocks_probe` 实测 120s，
> 721 条 flashblock 消息 / 扫描 1836 笔交易 / 捕获 60 笔 caliber 更新（300 pairs）/
> 解码失败 0；链上交叉验证 tx 级 60/60、存储级（`data+0`）300/300 全对齐，
> 流内 raw tx 与规范链 `to`/`input` 一致。

> 目标：用 XLayer flashblocks 实时交易流驱动本地 caliber 池子报价更新
> （`field0`/`field1`/deadline），替代当前短周期拉取（`start_caliber_prop_ladder_sync_task`），
> 将周期任务降频为"对账/兜底"。
>
> 逆向事实依据：`docs/caliber_prop_internal.md`
> - 更新交易 = `batchUpdateParameters((bytes32,uint64,uint32,uint64)[])`（selector `0x008dcc8e`）
> - calldata 每 pair = `[pairId, price→field0, flags→field1, deadline→tsX]`，只写 `data+0` 一个槽
> - 更新交易 **0 条日志** → 只能走 raw transactions，不能走日志匹配
> - 更新频率：实测 ~1-2s/次（200 块内 99 次），XLayer 出块 1s、每块 7~21 笔交易
> - ladder 静态（3 点固定曲线，所有 pair 相同，不随更新变）
> - 储备/pos（`cfg+4/5`、`cfg+7`）不随更新变（低频变动：做市商充值等）

## 1. 现状与痛点

- `sync_events()` 返回空 → caliber 不进日志管道，报价刷新完全依赖
  `sync_services::start_caliber_prop_ladder_sync_task`（`state_space/mod.rs:1961` 注册）周期
  `pool.update()`（约 10+n 次 `eth_getStorageAt`）。
- 痛点：① 轮询有固有滞后（报价 ~1-2s 更新一次，轮询要么跟不上要么 RPC 成本高）；
  ② 每次刷新 ~10+n 次 RPC；③ 与"实时价格"的使用场景（MEV/套利）不匹配。

## 2. 设计原则

1. **复用现有管道**：不新建并行链路；在 flashblocks 提取阶段产出"日志 + 交易事件"两类产物，
   排序/去重/`last_synced_block`/reorg 簿记全部复用。
2. **声明式/同构接入**：caliber 合约地址集合像 `binaryfi_engines` 一样显式传入提取函数，
   不在核心路由堆协议分支（对账任务保留，解决回填不对称）。
3. **fail-safe**：calldata 解码失败 / pairId 不在本地 / 断流 → 静默跳过，由低频对账快照纠正，
   绝不污染状态。
4. **成功确认**：caliber 更新无事件，只能靠 receipt status 确认交易是否真正落地；
   回滚（`status=0x0`）或 receipt 缺失（未确认）的更新一律不应用（2026-08-09 P0 修复）。
4. **性能零头**：每块逐笔 RLP 定位 `to`（约 1µs/笔，实测每块 ~14 笔），命中 1-2 笔 ABI 解码，
   每块增量 <100µs。

## 3. 总体架构

```
XLayer Flashblocks WS payload
  ├─ diff.transactions[]   (raw RLP 交易 hex，唯一能发现 caliber 更新的地方)
  ├─ metadata.receipts     (键 = tx hash；含 status，用于 caliber 成功确认)
  └─ base/diff 块信息
        │
        ▼
extract_logs_from_xlayer_flashblock()          ← 现有：日志提取（不动）
        │  新增（同函数内，与日志提取并行）：
        │    1) 遍历 diff.transactions，轻量 RLP 定位 to
        │    2) to ∈ caliber_contracts && input[0..4]==0x008dcc8e
        │    3) keccak256(raw) → 查 metadata.receipts，status != 0x1 → 丢弃
        │    4) ABI 解码 batchUpdateParameters → CaliberBatchUpdate[]
        │    5) 结合 XlayerTxCountTracker 的 tx_base 得块内全局 tx_index
        ▼
apply_logs_for_block_timed() / StateSpace.sync()  ← 复用现有簿记
        │  新增 apply_caliber_updates()（与 apply_logs 同锁同序）：
        │    - 块内按 tx_index 排序，逐 pair 路由
        │    - pairId → virtual_address（CaliberPropPool::virtual_address_from_pair_id）
        │    - 调 pool.apply_batch_update()
        ▼
CaliberPropPool.apply_batch_update()            ← 池子侧纯逻辑
        │    - field0=price, field1=flags, deadline 落库
        │    - last_synced_block = block_number
        ▼
start_caliber_prop_ladder_sync_task(降频保留)     ← 对账/兜底
        - 冷启动 / 断流回填 / 储备与 pos 变动 / 漏更新纠正
```

## 4. 具体改动清单

### A. 池子侧（`src/amms/caliber_prop/mod.rs`，纯逻辑，可单测）

1. **ABI 解码器**（不写固定偏移，用 `sol!` 标准 ABI）：
   ```rust
   pub struct CaliberBatchUpdate {
       pub pair_id: B256,
       pub price: U256,    // → field0
       pub flags: u32,     // → field1
       pub deadline: u64,  // → data+0 的 tsX（过期判断）
   }
   pub fn decode_batch_update_parameters(input: &[u8]) -> Option<Vec<CaliberBatchUpdate>>;
   ```
   在 `ICaliberPropAMM` 的 `sol!` 块内补充
   `batchUpdateParameters((bytes32,uint64,uint32,uint64)[])` 函数签名，
   用 `SolValue`/生成的调用解码（与 `batchQuote` 同风格）。

2. **raw tx → to 提取**：
   ```rust
   fn extract_to_from_raw_tx(raw: &[u8]) -> Option<Address>;
   ```
   实现为**轻量 RLP 定位**（已落地）：只读信封类型字节 + RLP 列表头 + 跳过前几项字段，
   **不解析 calldata、零分配**（EIP-1559 `to` 为第 5 项、2930 第 4 项、legacy 第 3 项），
   单笔 ~100-200ns。calldata 仅在 `to` 命中目标合约后经完整
   `TxEnvelope::decode` 提取（`extract_input_from_raw_tx`，每块仅 1-2 笔）。

3. **池子应用方法**：
   ```rust
   impl CaliberPropPool {
       pub fn apply_batch_update(&mut self, u: &CaliberBatchUpdate, block_number: u64);
   }
   ```
   - 校验 `u.pair_id == self.pair_id`，不匹配直接忽略；
   - `ladder.field0 = u.price`、`ladder.field1 = u.flags`（U256 转换）；
   - 更新过期/暂停语义所需的时间字段（对齐链上 `data+0`：`tsX=deadline`、`tsY=0`）；
   - `last_synced_block = block_number`。

4. **tx 兴趣声明**（供提取层构建地址集合）：
   ```rust
   pub const CALIBER_BATCH_UPDATE_SELECTOR: [u8; 4] = [0x00, 0x8d, 0xcc, 0x8e];
   pub fn caliber_contracts(pools: &[CaliberPropPool]) -> HashSet<Address>;
   ```
   沿用 `binaryfi_engines` 的传参模式，不在核心路由加协议分支。

### B. 提取层（`src/state_space/xlayer_flashblocks.rs`）

5. `extract_logs_from_xlayer_flashblock` 增加参数
   `caliber_contracts: &HashSet<Address>`（与现有 `binaryfi_engines` 并列）：
   - 日志提取完成后，遍历 `fb.diff.transactions`（函数内已有该数据）：
     - hex decode → `extract_to_from_raw_tx` → `to ∈ caliber_contracts`；
     - `input[..4] == CALIBER_BATCH_UPDATE_SELECTOR` → `decode_batch_update_parameters`；
     - 用现有 `XlayerTxCountTracker` 的 `tx_base` + 数组下标得到块内全局 `tx_index`
       （复用懒排序已有的 `real_tx_index_map` 机制，保证多 slice 顺序正确）。
   - 返回结构扩展（向后兼容：日志路径不变）：
     ```rust
     struct XlayerCaliberExtract {
         pub updates: Vec<(u64 /*tx_index*/, u64 /*block_number*/, CaliberBatchUpdate)>,
     }
     ```
     或直接并入现有返回元组并列字段。

6. `subscribe_xlayer_flashblocks_stream`：把提取出的 caliber 事件在
   `apply_logs_for_block_timed` 之后（或同批次内）交给路由/应用步骤。
   - 提取阶段需要 `state` 中的 caliber 合约地址集合：与 `binaryfi_engines`
     的构建方式一致（订阅启动时从 `state` 收集，或经 `HookRegistry` 注入）。

### C. 路由与应用（`src/state_space/mod.rs`）

7. 新增 `apply_caliber_updates(state, updates)`（或并入 `apply_logs_for_block_timed` 的通用步骤）：
   - 块内按 `tx_index` 排序（EVM 语义：后者覆盖前者）；
   - 逐条 `virtual_address = CaliberPropPool::virtual_address_from_pair_id(pair_id, contract)`，
     在 `state` 中查池子（不存在则跳过）；
   - 调 `pool.apply_batch_update(u, block_number)`（`get_mut_cow` 写回，与现有 sync 一致）；
   - 推进 `realtime_head`（复用现有簿记）。
   - 不需要动 `build_query_chunks`：caliber 仍无日志事件，tx 兴趣由提取层显式传参。

### D. 对账层（`src/state_space/sync_services.rs`）

8. `start_caliber_prop_ladder_sync_task`：
   - 语义从"报价刷新"改为"对账/兜底"，默认间隔从 `non_event_interval` 降为可配置的
     `caliber_reconcile_interval`（建议默认 30-60s，可配置）；
   - 职责：冷启动、断流回填、储备/pos 变动、漏更新纠正（`update()` 路径不变）。
9. `state_space/mod.rs:1961` 注册处：`caliber_ladder_sync_interval` 语义调整为对账间隔；
   新增实时开关 `caliber_realtime_sync`（默认 true，false 时退回纯周期拉取，便于灰度）。

### E. 配置

- `StateSpaceBuilder`：`caliber_reconcile_interval: Option<Duration>`、
  `caliber_realtime_sync: bool`（默认开）；文档注明原 `caliber_ladder_sync_interval`
  改名/语义变更。

## 5. 容错与边界

| 场景 | 处理 |
|---|---|
| 块内多笔更新 | 按 tx_index 排序应用，后者覆盖（EVM 语义） |
| calldata 解码失败 | 跳过该笔，不影响其他 pair；下次对账纠正 |
| 更新交易回滚（status=0x0） | receipt 校验拦截，不应用（P0：防幻影 deadline 污染本地时效） |
| receipt 缺失（未确认） | 不应用；实测同一 slice 内 diff.txs 数量 == receipts 数量 |
| pairId 不在本地池子 | 跳过（新 pair 未发现，走 discover/对账） |
| flashblocks 断流/重连 | 复用现有重连+初始回填；回填期间的 caliber 更新由对账任务补齐 |
| reorg | 依赖现有 realtime_head/对账兜底，caliber 事件不做额外 reorg 检测 |
| 冷启动 | `init()` 全量快照（现有） |
| 多部署 | 合约地址集合化（HashSet），与 `binaryfi_engines` 同构 |
| 对账与实时竞态 | 同一 RwLock，对账为幂等全量覆盖，实时为增量，最终一致 |

## 6. 性能分析（每块增量）

- XLayer 实测：1s/块、每块 7~21 笔（均 ~14）。
- 逐笔轻量 RLP 定位 `to`（零分配）：~100-200ns/笔 → ~2-3µs/块；
- 命中 1-2 笔完整解码 + ABI 解码：~几十 µs；
- 总计 **<100µs/块**，相对整条 flashblocks→decode→apply（毫秒级）可忽略。
- 现有链路在 binaryFI 增强/懒排序时已会对 raw tx 做 hex-decode + keccak，
  caliber 的 to 提取比那更轻。
- 可选优化（暂不需要）：raw hex 字符串预筛含合约地址子串（纳秒级）→ RLP 确认，误报兜底。
  已落地的是"to 轻量定位先行、calldata 命中后解码"，过滤阶段不做任何 calldata 拷贝。

## 7. 测试计划

1. **单测（纯函数）**：
   - `decode_batch_update_parameters`：用真实交易 `0xd9a1ffba...` 的 calldata 解码，
     断言 5 个 pair 的 (pairId, price, flags, deadline) 与链上一致；
   - `extract_to_from_raw_tx`：用真实 raw tx 断言 `to` 正确（EIP-1559/legacy 各一例）；
   - `apply_batch_update`：应用后 field0/field1/last_synced_block 正确；pairId 不匹配忽略。
2. **集成（fork）**：构造含 `0xd9a1ffba...` 更新交易的 flashblocks 风格 payload，
   跑提取→路由→应用，断言池子 field0 与链上 `data+0` 一致；
   现有 `tests/caliber_prop/xlayer_fork_test.rs` 快照路径回归（不受影响）。
3. **端到端（可选）**：真实订阅 XLayer flashblocks 一段时间，逐块对比本地 field0 与链上
   `data+0`（用 `eth_getStorageAt` 抽查）。

## 8. 实施里程碑

- **M1 池子侧纯函数**：解码器 + to 提取 + apply + 单测（不动核心链路，可先行合入）。
- **M2 提取层**：`xlayer_flashblocks.rs` 产出 caliber 事件 + 单测。
- **M3 路由/对账**：`state_space` 应用 + 周期任务降频 + 配置开关。
- **M4 端到端**：真实流验证 + 更新 `docs/caliber_prop_internal.md` §5 同步策略、
  本模块头注释（`sync_events()` 空的说明改为"实时交易驱动 + 低频对账"）。

## 9. 开放问题（不阻塞实现）

- `cfg+7`（pos）的写入者尚未反汇编确认：只影响反向报价的 pos 精度追踪，
  实时层刷新的是 field0（报价主体），pos 由对账快照覆盖。
- 回填精度受对账间隔约束：flashblocks 断线期间的 caliber 更新只能靠对账补齐，
  对账间隔即回填延迟上界。
- XLayer flashblocks 的 `diff.transactions` 覆盖 slice 内全部交易（现有 `tx_tracker`
  按它计数），多 slice 顺序用 `tx_base` 拼接——实现时复用该机制，勿自行假设。

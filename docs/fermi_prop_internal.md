# Fermi PropAMM 模块内部设计文档

> 面向长期迭代维护;记录 Fermi(Ethereum 主网 PropAMM)模块的架构、数据流、同步与模拟逻辑。
> 所有链上事实均为 2026-08-23 实测验证;模块开发按本文件里程碑(M1→M5)推进。

## 1. 目标

在 amms-rs 中实现 Fermi PropAMM 的本地实时同步与逐位对齐的 Swap 模拟:

- 实时同步:消费 Titan 报价流(链上看不到的高频报价)+ 链上事件(成交/对账/pair 生命周期),双数据源驱动本地状态;
- 本地模拟:`simulate_swap`/`spot_price` 在本地复刻链上 `quote` 逻辑,与 `eth_call`(state overrides + block override)逐位一致;
- 复用现有 AMMS 基建:AMM trait、Factory、state_space 实时同步与变化检测钩子。

## 2. Fermi 定位

FermiSwap 是 Ethereum 主网 PropAMM 中交易量最大的 venue(2026-08 占主网 PropAMM 主要份额)。
与 XLayer 的 BinaryFi/Caliber 同属"专业做市商高频更新链上报价"模型,但数据基础设施不同:

- XLayer:报价更新直接上链(事件/交易可观察),用事件驱动本地同步;
- Ethereum:报价更新走 Titan 私有流,链上只有"有吃单时才落块"的稀疏快照 → **必须新增 Titan 流作为第二数据源**。

### 2.1 协议核心逻辑精炼（速览）

- **共享金库**：trader vault `0x585d...B4F` 为全部 8 对 pair 共用流动性金库；报价方向 =
  (baseAsset 高价值资产, quoteAsset 计价资产)，如 WETH/USDC 以 USDC 计价 WETH，与
  `getPairs` 的 token0<token1 排序可能相反（init 已做方向探测）；
- **报价模型**：lane（E8 定点价）→ 失衡度修正 → 分段线性曲线（c1 正区间 8 档 / c2 负区间
  4 档）选档定价；失衡度 `M = (L2 - S*b/1e4)*1e18/S` 由金库两资产余额（换算到计价单位）
  的偏移实时决定，`P0 = lane*(1e22 + c2_interp(M))/1e22`；
- **双向公式**：正向 `out = A*price1/scale`，反向 `out = A*scale/price1`；反向走 REV
  子程序重新选档（曲线段参数 c/d 交换、delta 恒加 L0.c_eff），并受 last-trade 同块成交
  校正影响；
- **同块成交校正**：`last_trade_block == block.number` 时，正向 `div1` 累加 X、反向
  `a_norm` 累加 X' 重选档（不改金额）；校正后越过末段上界 → 链上 revert COR / 本地 None；
- **边界**：`vault + A > max_output` → IL；`a_norm` 低于/高于曲线段范围 → COR/封顶；
  反向大额封顶时 out 为常量（用末段端点价 + `A_eff = x*D/1e18`）；
- **新鲜度**：`updateTimestamp ∈ [block.timestamp − MAX_UPDATE_AGE, +MAX_UPDATE_LEAD_TIME]`
  否则 `StaleUpdate()` revert；本地模拟必须固定 block time
  （`BEACON_GENESIS_TS + slot×12`）。

## 3. 已核实的链上事实

### 3.1 合约拓扑(主网)

| 角色 | 地址 | 代码量 | 职责 |
|---|---|---|---|
| engine | `0x90f73fEA1Ee2Dc514d4dbAc0bfF7ff04b933767f` | 23KB | pair 管理、quote/swap 核心、读 registry lane |
| swapper | `0xb1076fE3AB5e28005C7c323Bac5AC06a680d452e` | 10KB | 执行层:`quoteAmounts`/`fermiSwapWithCallback`/`getPairsWithStatus` |
| IPropAMM wrapper | `0x5979458912F80B96d30D4220af8E2e4925A33320` | 14KB | 标准 `IPropAMM`:`quote(address,address,uint256)`、`swap`、`swapExactOut`、`quoteExactOut`、ERC-165 |
| registry | `0xDA7AFeEd01fe625cF15D187A19F94B45F00b8C5f` | 4KB | PrioUpdateRegistry:lane 存储、EIP-712 签名更新 |
| trader vault | `0x585d44727129B9C69791B10238Ca605932938B4F` | Safe | 全部 pair 共享流动性金库 |

函数选择器清单(实测,与 Tycho `ethereum-fermiswap` ABI 对照):

- engine:`getPairs()` `0x767eb5ef`、`quote(address,address,int256,address)` `0xc9e270d0`、`isActive` `0xae131deb`、`unlocked` `0x5a33733c`、`traderVault` `0x51ed0ee3`、`prioRegistry` `0xe8275d6e`、`maxParameterAge` `0x42b1e432`、`swapper` `0x2b3297f9`、`registerPair`/`unregisterPair`/`setPairActive`、管理函数
- swapper:`quoteAmounts(address,address,int256)` `0x300aa47f`、`fermiSwapWithCallback` `0xc27eeada`、`fermiSwapWithAllowances` `0xca89d04b`、`getPairsWithStatus` `0x87015b87`、`isEnabled` `0xca7004e7`、`fermi` `0x822d8ce0`、`setFermi` `0x4239db68`
- wrapper:上述 IPropAMM 全套 + `swapExactOut` `0x2822bb06`、`quoteExactOut` `0x7020df95`、`supportsInterface` `0x01ffc9a7`
- registry:`updateState(address,uint256,uint32,uint256[])` `0xa9114b0f`、`batchUpdateStateWithSignature` `0xe50de8ea`、`getState(uint256,uint32,uint32)` `0x16c83adc`、`addUpdater` `0x43d24a5e`、`removeUpdater` `0x04b07a5e`、`isUpdater` `0x75ceb837`、`MAX_UPDATE_AGE` `0x0260ee36`、`MAX_UPDATE_LEAD_TIME` `0xb278d9b0`、`DOMAIN_SEPARATOR` `0x3644e515`、`UPDATE_TYPEHASH` `0xd3fdd87d`、`eip712Domain` `0x84b0196e`

### 3.2 Pair 清单(engine `getPairs()` 实测,8 对全部 active)

WETH/USDC、WETH/USDT、WBTC/USDC、WBTC/USDT、USDC/USDT、cbBTC/USDC、cbBTC/USDT、WBTC/cbBTC。

### 3.3 报价状态机制(registry lane)

- lane 索引:`laneIndex = keccak256(abi.encode(tokenA, tokenB))`(每个地址 32 字节 ABI 填充);
- lane 状态 = `{updateTimestamp(uint32), flag, fairPriceE8}`(实测 E8 缩放:USDC/USDT≈0.9991、WETH 系≈2373-2375、WBTC 系≈764xx;同批更新共享同一 updateTimestamp);
- 更新入口:`updateState(laneIndex, updateTimestamp, slots[])` / EIP-712 签名批量 `batchUpdateStateWithSignature`;做市商经 Titan `ws/sendquoteupdate` 私有流提交签名交易;
- registry 即 ERC-8324 草案形态的 PrioUpdateRegistry(updater 白名单 + 全局 MAX_UPDATE_AGE/LEAD_TIME + EIP-712 域)。

### 3.4 报价链路与新鲜度

```
Taker → IPropAMM wrapper.quote/swap → swapper → engine.quote(tokenIn, tokenOut, int256 amountSpecified, sender)
engine: 读 prioRegistry lane(fairPriceE8, updateTimestamp) → 新鲜度检查 → PairParams(levels 分档) → 输出
```

- 新鲜度:`updateTimestamp ∉ [block.timestamp − MAX_UPDATE_AGE, block.timestamp + MAX_UPDATE_LEAD_TIME]` → `StaleUpdate()` revert;
- 模拟必须固定 block time = `BEACON_GENESIS_TS(1606824023) + slot×12`(与 Titan 快照的 slot 对齐),否则报价过期;
- `amountSpecified` 有符号:正 = exactIn,负 = exactOut;wrapper 的 3 参 quote 为 IPropAMM 标准(内部转 4 参);
- 做市商私有流 API key 门控;Titan 对 >400ms 未更新的报价从 overrides/price-levels 流中淘汰。

### 3.5 Titan 数据流(官方文档 + 2026-08-24 实测)

官方文档:`https://docs.titanbuilder.xyz/propamms`(总览)、`/propamms/takers.md`、`/propamms/makers.md`。

**overrides 流(主数据源,taker 公开无认证)**:
- WS `wss://{eu|ap|us}.rpc.titanbuilder.xyz/ws/pamm_quote_stream`;
  RPC `titan_getPammStateOverrides` @ `https://{region}.rpc.titanbuilder.xyz/data`;
- **连接即推送,无需订阅消息**(实测 1.3s 内收首条);每条消息为 JSON 文本完整快照,最新者胜;
- 消息结构:顶层 `slot`(beacon,十进制)、`blockNumber`(WS 十进制数字 / RPC hex 字符串)、
  `timestamp(ns)` + venue 地址 key(`0x`+40 hex)→ `{stateOverride: {account: {balance?, nonce?, code?, state?, stateDiff?}}}`;
  `stateOverride` 可直接作为 `eth_call` 第三参数;
- **实测:Fermi 在流中出现两个 key**——swapper `0xb1076fE3...`(文档 Top-level stream address)
  与 wrapper `0x59794589...`,逐条轮换;同一条 WS 消息一般只含一个 venue,RPC 快照为全部 venue 合并视图
  (2026-08-24 实测 8 个 venue key:含 Fermi 两个 key + Kipseli/Bebop/Tempest/Metric 等);
- 流为 maker opt-in;公开流当前含 FermiSwap/Kipseli/bopAMM,其余 maker 需白名单;
- **淘汰机制**:maker 端 >400ms 未更新的报价会被 Titan 从 overrides 与 price-levels 流同时淘汰
  (makers 文档;maker 目标重报价节奏 50ms)。

**price-levels 流(辅助/交叉验证)**:
- WS `/ws/pamm_price_levels` + RPC `titan_getPammPriceLevels`;每条消息为完整快照,旧消息作废;
- 结构:顶层 `slot`/`blockNumber`/`timestamp` + `pamms: [{pamm, pairs: [{tokenIn, tokenOut,
  orderBook: [{amountIn, amountOut, variant: "Simulated"|"Interpolated"}]}]}]`;
  `Simulated` = EVM 合成吃单实测,`Interpolated` = 线性样条插值(有微小近似误差);
- **实测(2026-08-24)**:连接 0.8s;6 个 pamm(含 Fermi,以 wrapper `0x59794589...` 为 key);
  主流 pair 15-21 rung,部分方向仅 3-4 rung;
- 用途:本地模拟的交叉验证、对拍"当前可成交价"、辅助路由决策;不作为精确模拟输入。

**quote helpers(可选,RPC)**:
- `titan_getPammQuoteVenue(venue, tokenIn, tokenOut, amountIn)`(mirror LambdaClass `quoteVenueV1`)、
  `titan_getPammQuote(tokenIn, tokenOut, amountIn)`(扫全部 pAMM 取最优);
  基于最新 price-levels 快照,返回 `{tokenIn, tokenOut, amountIn, amountOut, pamm, router, blockNumber, slot, timestamp}`;
  适合机会发现,不适合本地精确 Swap 模拟。

**maker 流(`/ws/sendquoteupdate`,API key 门控,仅做市商)**:
- `Authorization` header 携带 API key;消息为 protobuf `PWebsocketQuoteUpdateV1Args`
  (RLP 签名更新 tx + block_number + 16 字节 replacement_uuid + 单调 seq + asset_pairs + quote_address + pool_addresses);
- 条件包含模式下"无吃单不上链";Titan 保证 taker 吃单前同一区块先包含最新 quote 更新;
- 我们作为 taker 不消费此流,仅记录协议语义。

**Titan 区块级保证(与 bundle 交易相关)**:
- builder 对每个候选 bundle 按最新 pAMM 状态评估,每次报价更新重评估直到入块;
- taker 交易前,最新 quote 更新保证先落同一区块;freshness buffer b 保护 maker 防抢跑。

**Fermi Oracle 门面(记录)**:Titan 总览页列出 Fermi Oracle `0x26e5A56f807d4C937B0b815266B135F09B4Bf312`
(含 getPairs/quote/getState/prioRegistry 全套选择器,prioRegistry=0xDA7AFeEd,2026-08-24 实测);
与流地址(swapper/wrapper)不同,仅作合约清单,不参与 taker 接入。

### 3.6 当前环境注意点

2026-08-23 实测:registry lane 最近更新约在 2026-08-16,近 2000 块无 Fermi 成交日志(模拟环境特性)。
带 overrides 的 quote 调用需把 block time 固定到 lane 时间戳窗口内才能通过新鲜度检查;
已确认 4 参数 `eth_call`(state overrides + block override)生效,错误从 `StaleUpdate()` 推进到
`Panic(0x11)`(报价计算深处,剩余为 lane→pair 槽位映射与 quote 数学逆向)。

## 4. 架构总览(双数据源)

```
                        ┌──────────────────────────────┐
                        │        state_space           │
                        │                              │
  链上事件(newHeads→logs) │  ┌────────────────────────┐  │
  PairRegistered/Swapped │  │ FermiPropPool 状态      │  │
  updateState 交易/Transfer│  │ lanes / params / 余额   │  │
                        │  └────────────────────────┘  │
                        │         ▲          ▲         │
                        │         │(对账/校准) │(实时报价) │
                        │  ┌──────┴───┐  ┌───┴────────┐ │
                        │  │事件同步器  │  │Titan 流源   │ │
                        │  └──────────┘  └────────────┘ │
                        └──────────────────────────────┘
  数据源 A(链上):engine/registry/vault 事件与交易 → 可回放、可对账
  数据源 B(Titan):overrides 快照(lane + 余额)→ 实时、最新者胜、无历史
```

## 5. 数据结构设计

### 5.1 TitanOverridesSnapshot(state_space 层,所有 PropAMM venue 共用)

```rust
pub struct TitanOverridesSnapshot {
    pub slot: Option<u64>,          // beacon slot,新鲜度基准
    pub block_number: Option<u64>,
    pub timestamp_ns: Option<u64>,
    pub per_pamm: HashMap<Address, PammOverrides>,  // venue → 合约 → override
}
pub struct PammOverrides { pub accounts: HashMap<Address, AccountOverride> }
pub struct AccountOverride {
    pub balance: Option<U256>,
    pub nonce: Option<u64>,
    pub state_diff: HashMap<B256, B256>,  // storage slot → value
}
```

### 5.2 FermiPropPool(amms 层)

```rust
pub struct FermiPropPool {
    // 部署地址(engine/swapper/wrapper/registry/vault)
    // pair 级状态:
    //   lanes: Vec<FermiLane>            // 每 pair: fair_price_e8/update_timestamp/flag
    //   pair_params: Vec<FermiPairParams> // fee + buy/sell levels(6 字段 × N)
    //   vault_balances: HashMap<Address, U256>
    // 版本: quote_slot / last_synced_block
}
```

- 池实例粒度:deployment 级单实例(与 BinaryFi 一致,共享金库/registry 状态);按需扩展 virtual sub-pool;
- `exposed_pair` 语义与 BinaryFi 相同(可限定对外暴露的 pair)。

## 6. 同步流程

### 6.1 链上事件流(newHeads → logs,现有管线)

| 事件/调用 | 来源合约 | 用途 |
|---|---|---|
| `PairRegistered`/`PairUnregistered`/`PairActiveSet` | engine | pair 发现与启停 |
| `updateState`/`batchUpdateStateWithSignature` 调用 | registry | 报价上链提交(稀疏但可确认),断线校准 |
| ERC20 `Transfer`/WETH `Deposit`/`Withdrawal` | token 合约 | vault 余额对账 |
| `Swapped`(IPropAMM wrapper)/swap 内部事件 | wrapper/engine | 成交对账 |

### 6.2 Titan 报价流接入方案(M4,基于官方文档 + 2026-08-24 实测)

```
Titan WS overrides 流 ──┐
                        ├─> TitanOverridesSnapshot(Arc 共享缓存, slot 单调守卫)
RPC 快照 rebase(冷启动/断线)┘
                        │
                        ▼  M4 消费者(state_space 挂载, 每 venue 快照驱动)
  1. venue 过滤:Fermi = swapper 0xb1076fE3 + wrapper 0x59794589 两个 key
  2. 解码 stateDiff:registry 槽位(M3.1 公式) → 各 pair 的 FermiLane 更新
  3. balance override → vault 余额缓存(仅模拟辅助; 权威余额仍以链上事件为准)
  4. apply_titan_lane 版本守卫(update_timestamp 不回卷; slot 守卫在上游)
  5. 重算受影响 pair 的 spot/quote → 触发下游变化检测(复用 HookRegistry)
断线/空闲超时 → RPC titan_getPammStateOverrides 拉最新快照 rebase → 重连 WS
```

接入要点(实测确认):

- **端点就近**:eu/ap/us 三区域,选离部署最近区域;WS 空闲超时 30s 判活(无官方心跳,
  实测连接即推、消息间隔 ~0.5-1s);重连前先 RPC rebase(流无历史);
- **字段宽容**:WS `blockNumber` 为十进制数字,RPC 为 hex 字符串;`stateOverride`/`state_override`
  两种命名并存于官方示例——解析器双兼容(现有 `parse_u64`/`parse_pamm_overrides` 已覆盖);
- **venue 双 key**:Fermi 的 swapper 与 wrapper 都会出现在流中,按 `per_pamm` 合并,
  lane 解码只看 registry `stateDiff`(与 key 无关);
- **price-levels 交叉验证**(M5):与本地 quote 模拟对拍"当前可成交价",偏差超阈值告警;
- **block time 固定**:模拟用 `BEACON_GENESIS_TS + slot×12`,与 lane 时间戳窗口对齐,否则 `StaleUpdate`;

### 6.3 双源合并与校准

- 报价(lane 价格/时间戳):以 Titan 流为准(链上 latest 过时);
- 余额/成交:以链上事件为准(Titan balance override 仅作模拟辅助);
- 交叉校准:链上 `updateState` 事件时间戳 vs 流 slot,落后时触发 refresh;
- 版本守卫:流更新不得回卷(旧 slot 覆盖新 slot),与 resync 新鲜度守卫同款。

### 6.4 M4 实施计划与实现状态（2026-08-25；M4.1-M4.4 ✅，M4.5 数学链路生产对拍 ✅，M4.5.1 真实 WS 长跑验证 ✅）

#### 6.4.1 目标与范围

将 Titan overrides 流接入本地实时同步，使 Fermi 8 对 pair 的 lane 报价与 Titan
最新快照对齐（无链上事件可观察的高频报价），并驱动下游变化检测。范围：

- 消费 `pamm_quote_stream`（WS）+ `titan_getPammStateOverrides`（RPC rebase）；
- 快照 → 各 pair lane / vault 余额 override → FermiPropPool 更新 → 变化检测通知；
- 断线/静默恢复（RPC rebase）+ 链上低频校准；
- price-levels 解析器（M5 交叉验证的前置，一并落地，不参与同步）。

不做：bundle 构造、maker 流接入、router 集成（分属其它里程碑）。

**实现状态（2026-08-24）**：
- M4.1 ✅ price-levels 解析器（`TitanPriceLevelsSnapshot` + parse + RPC + WS 订阅 + slot 守卫，3 单测）；
- M4.2 ✅ Fermi 快照应用器（`amms/fermi_prop/titan.rs`：venue 双 key 合并 → 槽位查找 → lane 应用，
  7 单测含真实快照 fixture）；
- M4.3 ✅ state_space 挂载（`state_space/titan_consumer.rs` + `TitanPammStreamConfig` +
  `StateSpaceBuilder::with_titan_pamm_stream`，`ensure_background_tasks` 内 Ethereum 主网
  **自动检测**（存在 Fermi 池即启用，无需手动调用；显式 Some(config) 强制启用），3 单测）；
- M4.4 ✅ 链上校准（`reconcile_lanes`：`eth_getStorageAt` 读 8 槽位，链上更新则以链上为准刷新）+
  断线 rebase（`subscribe_overrides_stream` 内建 RPC rebase）；
- M4.5 ⏳ 生产 WS 实时流 + `eth_call` 对拍（部署环境验证项，不纳入单元测试）。

#### 6.4.2 架构（在现有基础设施上增量）

```
titan_stream.rs（M1，已有）                     amms/fermi_prop/titan.rs（M4 新增）
  subscribe_overrides_stream ──┐               apply_titan_snapshot(...)
  (WS + slot 守卫 + 重连)      │                ├─ venue 过滤(swapper/wrapper)
  fetch_overrides_snapshot(RPC)│                ├─ stateDiff → 8 对 lane
                               ▼                ├─ vault balance override
                    TitanOverridesSnapshot       └─ 汇总 affected 地址
                               │
                               ▼
        state_space::mod.rs（M4 挂载, ensure_background_tasks）
        spawn_titan_pamm_stream: state(Arc<RwLock<StateSpace>>) + hooks
          快照到达 → state.write() 短临界区:
            for pool in Fermi pools: apply_titan_snapshot(pool, snapshot)
          释放锁 → hooks.notify(affected)   # 复用现有变化检测
          断线 → RPC rebase → 重连(流内自恢复)
```

#### 6.4.3 模块设计

**A. `src/amms/fermi_prop/titan.rs`（新增，Fermi 快照应用器）**

```rust
/// 单个快照对 Fermi 部署的应用结果
pub struct FermiTitanApplyOutcome {
    pub affected_pools: Vec<Address>,   // virtual_address 列表(仅实际变化的)
    pub lanes_applied: usize,
    pub balances_applied: usize,
    pub slot: Option<u64>,
}

/// 将 Titan overrides 快照应用到本 pool（lane + 余额 override）。
/// - lane: 用 M3.1 公式计算本 pair 的 registry 槽位，在快照 stateDiff 中查找；
///   命中则 apply_titan_lane（update_timestamp 版本守卫），返回是否变化；
/// - 余额: 快照中 vault 地址的 balance override → apply_vault_balances
///   （仅模拟辅助，权威余额仍以链上事件为准）；
/// - 返回 affected 判定 = lane 变化 || 余额变化。
pub fn apply_titan_snapshot(
    pool: &mut FermiPropPool,
    snapshot: &TitanOverridesSnapshot,
) -> FermiTitanApplyOutcome;
```

实现要点：
- venue 判定：`snapshot.per_pamm` 中 key ∈ {swapper, wrapper} 任一存在即可（Fermi 双 key
  实测交替出现；stateDiff 取并集按槽位合并，后到覆盖）；
- lane 槽位查找：`fermi_registry_lane_slot(pool.engine_address, pool.token_a, pool.token_b)`
  → 在 registry（`pool.registry_address`）账户的 `state_diff` 中取 32 字节 → `FermiLane::from_slot_word`
  （解码已在 M3.1 验证）；槽位不在 stateDiff 中 = 该 pair 无新报价，跳过；
- 版本守卫双层：上游 slot 单调（accept_snapshot）+ 此处 `apply_titan_lane` 的
  update_timestamp 不回卷；同批更新共享 timestamp、价格可能微变——timestamp 相等即接受（现有实现已覆盖）；
- 池未 init（lane.fair_price_e8 == 0）时首个快照直接接受（无回卷风险）。

**B. `src/state_space/titan_stream.rs`（扩展，price-levels 解析）**

```rust
pub struct TitanPriceLevelsSnapshot {
    pub slot: Option<u64>,
    pub block_number: Option<u64>,
    pub timestamp_ns: Option<u64>,
    pub pamms: Vec<TitanPammLadder>,
}
pub struct TitanPammLadder {
    pub pamm: Address,
    pub pairs: Vec<TitanPairLadder>,
}
pub struct TitanPairLadder {
    pub token_in: Address,
    pub token_out: Address,
    pub order_book: Vec<TitanLevel>, // amountIn/amountOut/variant(Simulated|Interpolated)
}
pub fn parse_price_levels(raw: &Value) -> TitanStreamResult<TitanPriceLevelsSnapshot>;
pub fn fetch_price_levels_snapshot(rpc_url: &str) -> ...;
pub fn subscribe_price_levels_stream(config) -> ...; // 同 overrides 的守卫/重连语义
```

**C. `src/state_space/mod.rs`（挂载 + 配置）**

```rust
// StateSpaceManager 新增字段:
pub titan_config: Option<TitanPammStreamConfig>,   // None = 关闭(默认)

pub struct TitanPammStreamConfig {
    pub ws_url: String,        // 默认 eu 区域
    pub rpc_url: String,
    pub idle_timeout: Duration,   // 30s(400ms 淘汰下断流必然触发)
    pub reconnect_delay: Duration,
    pub reconcile_interval: Duration, // 链上校准周期,默认 30s
}

// 启用语义（M4.3 实现）:
// - 默认 None → 自动检测:state 中存在需要 Titan 流的 PropAMM 池
//   （`titan_consumer::pool_requires_titan_stream`，目前 = Fermi）即自动启用,
//   无需手动调用;新增 venue 时在该函数扩展;
// - Some(config) → 显式配置并强制启用（可定制区域/空闲超时/校准周期）。

// ensure_background_tasks 内、chain_id == 1 且配置启用时 spawn:
pub async fn run_titan_pamm_stream_task(
    config, state: Arc<RwLock<StateSpace>>, hooks: HookRegistry<Vec<Address>>,
) {
    loop {
        // WS 流 + RPC rebase 由 subscribe_overrides_stream 自带;
        // 每个快照: state.write() 短临界区遍历 Fermi pools 应用,
        //   affected 非空 → hooks.notify(affected);
        // 周期 reconcile: eth_getStorageAt 读 8 个 lane 槽位,
        //   update_timestamp 更新/回卷检查 + 日志。
    }
}
```

- 复用 `background_started` AtomicBool 幂等挂载（与 canonical_head_tracker 等同级）；
- 不阻塞 realtime 日志路径：Titan 更新独立 seq/日志（`titan_update_applied`）；
- 遍历范围收敛：先收集 state 中全部 `AMM::FermiPropPool` 地址（数量少，8 对），
  再逐 pair 应用；新增 pair（PairRegistered）由现有 discovery 路径负责，消费者每次遍历即见。

#### 6.4.4 数据流与状态合并规则（更新 6.2 图）

- 报价 lane：以 Titan 流为准（链上 latest 过时）；同 slot 多 venue 消息逐条应用（slot 相等通过守卫）；
- vault 余额：**不来自 Titan 流**——stateOverride 的 `balance` 是原生 ETH 余额，
  ERC20 金库余额需从 token 合约 `stateDiff`（balance 槽位）解码，Titan 实测快照不含
  （2026-08-24 确认）。权威 ERC20 账本 = init `balanceOf` + 链上事件（Transfer/Swapped 对账）
  + reconcile `eth_getStorageAt` 校准（M4.4 已实现）；`apply_vault_balances` 保留为防御性接口；
- 交叉校准：reconcile 周期读链上 8 槽位 + 最近 `updateState` 事件时间戳：
  - 链上 timestamp > 本地 → 以链上为准刷新（流可能落后/漏消息）；
  - 链上 timestamp 长期 < 本地 → 正常（报价走私有流不上链），仅日志；
- price-levels：M5 与本地 quote 对拍用，不写入 pool 状态。

#### 6.4.5 关键时序与并发

- 快照处理必须快（400ms 淘汰节奏）：state.write() 临界区内只做
  slot 查表 + U256 解码 + HashMap 更新（无 RPC、无锁嵌套）；
- hooks.notify 在锁外执行（与 realtime 路径一致，避免死锁）；
- WS 消息间隔 ~0.5-1s（实测），单条处理 < 1ms，无积压风险；
- 与 pending_sync_worker / maintenance 的潜在写冲突：统一走
  `StateSpace::get_mut_cow` + 版本守卫（update_timestamp 单调），旧值覆盖新值被拒绝。

#### 6.4.6 配置与环境

```toml
# 示例(生产 Ethereum 主网):
[state_space.titan]
enabled = true
region = "eu"                    # eu|ap|us
idle_timeout_secs = 30
reconcile_interval_secs = 30
```

- **默认无需配置**：Ethereum 主网 + Fermi discovery 自动启用（eu 区域默认参数）；
- 需要定制区域/校准周期时：`with_titan_pamm_stream(Some(TitanPammStreamConfig::new(ws, rpc)))`；
- 端点常量已在 `titan_stream.rs`（DEFAULT_OVERRIDES_*），区域仅前缀不同。

#### 6.4.7 测试计划（只跑 fermi/state_space 相关，不跑全库）

1. **单测（fermi_prop/titan.rs）**：
   - 真实快照 fixture（2026-08-24 实测 payload）→ 8 对 lane 全部应用、价格与快照逐位一致；
   - venue 双 key（swapper/wrapper）各自驱动同一批 lane 更新，幂等；
   - 旧 update_timestamp 拒绝（回卷守卫）；同 timestamp 新价格接受；
   - 槽位不在 stateDiff（该 pair 无报价）→ 跳过、不影响其它 pair；
   - vault balance override 应用；事件账本优先语义。
2. **单测（titan_stream.rs）**：price-levels 解析（真实 payload）、RPC/WS 字段宽容。
3. **集成（对拍，含 anvil，不放全库跑）**：
   - 生产 WS 订阅 60s 收集真实快照 → 驱动本地 pools → 每个 pair 本地 quote
     vs 带 overrides 的 `eth_call`（block time = slot）逐位一致；
   - 断线重连：RPC rebase 后 slot 守卫正确拒绝旧快照。
4. **验证脚本**：`cargo test -p amms --lib fermi_prop` / `state_space::titan_stream` 定向执行。

#### 6.4.8 里程碑拆分与验收

| 子任务 | 内容 | 验收 |
|---|---|---|
| M4.1 | price-levels 解析器 + RPC/WS + 单测 | 真实 payload 解析全通过 |
| M4.2 | `fermi_prop/titan.rs` 应用器 + 单测 | 8 对 lane 与快照逐位一致、守卫生效 |
| M4.3 | state_space 挂载消费者 + 配置 + hooks 通知 | 快照→pool 更新→affected 通知链路打通 |
| M4.4 | 链上校准 + 断线 rebase 验证 | 断流 30s 内恢复、旧快照被拒 |
| M4.5 | 真实流集成对拍（生产 WS） | 本地 quote vs eth_call 100% 对齐 |

#### 6.4.9 风险与回退

- **Fermi 流短期不可用/权限变化**：`enabled=false` 即退回纯链上模式（稀疏更新），
  报价陈旧但系统可用；Titan 公开流当前含 FermiSwap（实测），白名单风险低；
- **同批 timestamp 不变价格微调**：版本守卫按 timestamp 判定可能吞掉同批后续微调——
  Titan 侧同批即最新，slot 守卫保证消息级单调，可接受；
- **双 venue key 内容冲突**：按槽位并集、后到覆盖，日志记录冲突次数；
- **性能**：8 对 × 8 槽位，O(1) 查表，无热点。

## 7. 模拟逻辑（M3.1：trace + 生产 eth_call 级逆向，100% 对齐）

### 7.1 engine_quote 复刻（本地，已实现）

与 engine 字节码逐位对齐（`debug_traceCall` 逆向 + 生产 RPC `eth_call` 对拍；
基准 block `0x18a0d7b`，lane = 246288406772，WETH/USDC 对）。公式：

```
前置（vault 余额 → 失衡度 → 基准价）：
  scale  = 1e(8 + dec_diff)                    # WETH/USDC = 1e20
  L1     = vault(token_b)                      # ★ 以 quote 资产为单位（trace 实证）
  L2     = vault(token_a) * lane / scale       # base 资产折合 quote 计价
  S      = L1 + L2
  M      = (L2 - S * b / 1e4) * 1e18 / S      # ★ M4.6 修正：失衡基准 = S*b/1e4（b=pair_params.b），
                                                 #   非 S/2；WETH 系 b=5000 → 恰等于 S/2（旧公式巧合成立），
                                                 #   WBTC/cbBTC 系 b=3333（trace 实证 @25827361/25827458）
  delta_c2 = c2 曲线对 M 的插值（4 档，见下；M>0 → -2M，M<0 → -0.4M）
  P0     = lane * (1e22 + delta_c2) / 1e22     # ★ 非 lane*K/1e22（旧 K 常量已废弃）

正向（token_in == token_a，WETH→USDC 例）：
  div1   = A * lane / scale
  a_norm = div1 * 1e18 / D                     # D = pair_params.c（WETH 系 = 3e12）
  找段 i：c1[i].y < a_norm <= c1[i].x          # 8 档 c1
  p1     = (a_norm - y) * 1e18 / (x - y)
  delta2 = c + p1 * d / 1e18 + a               # ★ M4.6 修正：常数项 = pair_params.a（=2e17），
                                                 #   非 c2[0].c；cbBTC 系 c2[0].c=0 但链上仍加 2e17
                                                 #   （trace 实证 @25827458），WETH/WBTC 的 c2[0].c 恰为 2e17
  price1 = P0 * (1e22 - delta2) / 1e22
  out    = A * price1 / scale

反向（token_in == token_b，USDC→WETH 例；A 为 quote 原生单位）：
  a_norm = A * 1e18 / D
  a_norm > c1 末段.x（=1e18）→ 封顶：a_norm = 末段.x，A_eff = a_norm * D / 1e18（out 持平）
  price1 = P0 * (1e22 + delta2) / 1e22
  out    = A_eff * scale / price1
```

边界与截断（trace/生产 eth_call 实证）：

- **COR**：`a_norm <= c1[0].y`（下界）→ revert `COR`。正向 @ A < ~4.06e12 wei；
  反向 @ A <= 10000 USDC；
- **IL**：仅正向路径。`max_output[token_a] > 0 && vault(token_a) + A > max_output` → revert `IL`。
  `max_output` 为引擎槽位 8 的 mapping（key = `keccak256(abi.encode(token, 8))`，按 **base 资产**
  索引）；WETH = 1.8e21（槽位 `0x5cc08d...cb26`），故正向 IL 边界 = 1.8e21 − vault(WETH)
  = 253005317793142188091 @block 0x18a0d7b。合成 vault 高失衡（r>=0.816）正向 revert 即由此触发；
- **输出截断**：`out = min(out, vault(token_out))`（反向大额精确等于 vault 余额，eth_call 实证）；
- **反向无 IL**：超大金额恒返回封顶值 1214462937530800654669 wei（A >= 3e12 USDC，
  生产 eth_call @block 0x18a0d7b）。

段/曲线数据（WETH/USDC，@block 0x18a0d7b，引擎存储槽位 stride 3：
`slot0 = (x高128, y低128)`、`slot1 = (a高128, b低128)`、`slot2 = (c高128, d低128)`；
**真实 ABI 顺序：x=上界、y=下界、c=截距、d=斜率，无需交换**）：

| c1 L | y（下界） | x（上界） | c | d |
|------|-----------|-----------|-----|-----|
| 0 | 3.333e9 | 1.667e15 | 5e17 | 1e17 |
| 1 | 1.667e15 | 1.667e16 | 6e17 | 4e17 |
| 2 | 1.667e16 | 3.333e16 | 1e18 | 5e17 |
| 3 | 3.333e16 | 1e17 | 1.5e18 | 1e18 |
| 4 | 1e17 | 1.667e17 | 2.5e18 | 2.5e18 |
| 5 | 1.667e17 | 3.333e17 | 5e18 | 3e18 |
| 6 | 3.333e17 | 6.667e17 | 8e18 | 7e18 |
| 7 | 6.667e17 | 1e18 | 1.5e19 | 1.5e19 |

| c2 L | y（下界） | x（上界） | c | d |
|------|-----------|-----------|-----|-----|
| 0 | -1e18 | -5e17 | 2e17 | 0 |
| 1 | -5e17 | 0 | 2e17 | -2e17 |
| 2 | 0 | 5e17 | 0 | -1e18 |
| 3 | 5e17 | 1e18 | -1e18 | 0 |

> ⚠️ **M3.1 重大修正（2026-08-24）**：旧版 `P0 = lane * K / 1e22`（K=9999327544673108912560）
> 是错误的——K 实际是 `(1e22 - 2M)/1e22` 的近似，M 随 vault 余额实时变化，无法用常量近似；
> 旧版 `c_eff/d_eff` 交换、`delta 恒加 c1[0].d=5e17`、COR 上界等亦为错误理解。
> 现公式 68/69 点全对齐（唯一 1 点为 vault 截断，已由 `capped_out` 覆盖）。

### 7.1.1 同块成交 last-trade 校正（M4.5，2026-08-25 trace 级实证）

engine 每 pair 存储"最后成交"记录（存储槽 7 的嵌套 mapping）：
`slot = keccak256(abi.encode(sub_key, keccak256(abi.encode(laneIndex, 7))))`，
值布局 `(last_trade_x << 64) | last_trade_block`（low 32 位 = 成交区块号）。
`sub_key = 0` 正向路径、`sub_key = 1` 反向路径（实测槽位见 `types.rs`）。

- **生效条件（anvil 存储注入实证 @25827361）**：仅当 `last_trade_block == block.number`
  （同块成交）时校正生效；相差 ≥1 块即忽略。本地用 `last_synced_block == 成交块` 判定，一致。
- **正向（sub_key=0）**：`div1 = A*lane/scale + X`，再 `a_norm = div1*1e18/D`。
- **反向（sub_key=1）**：`a_norm = (A + X')*1e18/D` 重新选档，但 `a_eff` 保持原始 `A`
  （校正只影响分档、不影响金额；trace 实证 @25828239 WETH/USDT，X'=13221602161）。
- **反向封顶与校正互斥（M4.6 补充实证 @25828950）**：`a_norm_raw = A*1e18/D`。
  `a_norm_raw > 末段.x` → 封顶（无校正路径；A_eff = 末段.x*D/1e18，out 持平）。
  否则若同块成交：`a_norm = (A+X')*1e18/D`，校正后 `a_norm > 末段.x` 时引擎
  **无段可匹配 → revert COR**（本地返回 None，与链上 COR 对齐；WBTC A=1e12 同块成交实证）。

### 7.1.2 失衡度与 delta2 常数项修正（M4.6，2026-08-25 生产对拍 + anvil trace）

4-pair 1800 块漂移对拍暴露 WBTC/cbBTC 系差异，anvil fork + `debug_traceCall` 逐值实证：

1. **M 公式**：`M = (L2 - S*b/1e4) * 1e18 / S`（SDIV 截断向零）。WETH 系 b=5000 时
   `S*b/1e4 ≡ S/2`，旧公式（S/2）对 WETH 成立；WBTC/cbBTC 系 b=3333 必须用 b。
2. **delta2 常数项**：`delta2 = c + p1*d/1e18 + a`（a = pair_params.a = 2e17）。
   旧公式误用 `c2[0].c`：cbBTC 系 c2[0].c=0 但链上仍加 2e17（打包参数槽字段提取实证）。
3. **c2 段匹配**：`y < M <= x`（内部序 x=上界、y=下界），负区间段可能 c=d=0
   （cbBTC 前两段）→ `delta_c2 = 0`，`P0 = lane`。

### 7.2 100% 对齐验证状态（M3.1）

- **正向 WETH→USDC**：生产 RPC `eth_call` 8 点 out 对拍（A=1e13~5e19，全部逐位一致，
  内嵌 `engine_quote_forward_matches_chain`）；失衡度 sweep（r=0.05/0.1/0.25/0.5/0.75，
  M 覆盖 ±4e17）16 点对拍（`engine_quote_imbalance_sweep_matches_chain`）；
- **反向 USDC→WETH**：生产 RPC `eth_call` 8 点（A=1e8~1e15）+ sweep 反向 7 点 + 大额封顶常量；
- **边界**：COR/IL 阈值、vault 截断、反向封顶均有测试覆盖；
- **跨 pair 泛化（M3.1 + M4.6）**：registry lane 直读公式对所有 pair 生效；WBTC/USDC、
  cbBTC/USDC 曲线 quote 已纳入漂移测试（2026-08-25 全通过）；
- **漂移验证（2026-08-25，`tests/fermi_prop/mainnet_sync_drift.rs`）**：4-pair
  （WETH/USDC、WETH/USDT、WBTC/USDC、cbBTC/USDC）1800 块漂移全 PASSED——每个检查点
  vault 余额与链上 `balanceOf(vault)` 逐位一致、`engine_quote` 与 fresh-lane-override
  `eth_call` 逐位一致；WETH/USDC 2500 块长程回归 PASSED（10/10 检查点、2643 事件）。
  只跑 `cargo test --test fermi_prop`（不跑全库）；
- **正向 a_norm > 1e18 分支**：引擎走 0x4238 双档 + 0x48cb 牛顿-拉弗森，正向在 IL 生效时不可达
  （IL 边界远小于曲线上限），本地防御性 None + TODO(M5 真实流验证)。

## 8. 模块文件结构

```
src/state_space/titan_stream.rs    # M1:Titan 流基础设施(overrides + price-levels + RPC/WS + slot 守卫)
src/state_space/titan_consumer.rs  # M4:流消费者(state 应用 + hooks 通知 + 链上校准)
src/amms/fermi_prop/
├── mod.rs                         # M2/M3:FermiPropPool + AMM trait + quote 复刻 + apply_titan_lane
├── titan.rs                       # M4:快照应用器(venue 过滤 + 槽位查找 + lane 应用)
├── types.rs                       # ABI、lane/pair params 结构、槽位公式
└── factory.rs                     # discover/getPairs/isActive + init_batch + 周期同步
docs/fermi_prop_internal.md        # 本文档
```

## 9. 里程碑与开发节奏

- **M1 基础设施** ✅：`state_space::titan_stream`（WS 订阅 + RPC 快照回退 + slot 单调守卫 + 重连）+ 解析器 + 4 个单测（真实 payload fixture）。2026-08-23 完成。
- **M2 状态模型** ✅：`src/amms/fermi_prop/`（mod.rs/types.rs/factory.rs），`FermiPropPool` per-pair 结构 + 链上 init（getPairs/getPairParams/isActive/registry getState/ERC20 balanceOf）+ pair 事件同步（PairActiveSet/Swapped/Transfer）+ AMM trait 全套 + 注册到 AMM enum/Factory/state_space filters + 10 个单测。2026-08-23 完成。
- **M3 报价模拟** ✅：engine_quote 曲线精确复刻（trace 级逆向）+ anvil eth_call 对拍测试（正向/反向/边界全覆盖）。2026-08-23 完成。
- **M3.1 lane 映射** ✅：registry lane→槽位映射破解（`keccak256(abi.encode(engine, laneIndex))`），
  8/8 pair 双数据源验证 + 存储槽直读上线（init 首选，getState 兜底）。2026-08-24 完成。
- **M4 实时同步** ✅（M4.1-M4.4）：Titan 流接入 pool 更新（`fermi_prop/titan.rs` 应用器 +
  `state_space/titan_consumer.rs` 消费者 + `with_titan_pamm_stream` 配置挂载）+ 下游变化检测
  （hooks.notify(affected)）+ 链上校准（`reconcile_lanes`）+ 断线 RPC rebase。2026-08-24 完成。
- **M4.5 生产验证（数学链路）** ✅（2026-08-25）：本地 quote vs `eth_call`（state overrides +
  固定 block time）逐位对拍 + 4-pair 漂移全 PASSED（实证见 §9.5/§9.6）。
- **M4.5.1 真实 WS 长跑验证** ✅（2026-08-25）：`tests/fermi_prop/ws_live_verify.rs` 连接
  真实 Titan overrides WS，5 分钟生产实测 PASSED（详见 §9.7）；长时漂移持续观察。
- **M5 上线验证** ⏳：与 Titan price-levels 交叉验证、真实 swap 回放校验（待链上恢复成交活动）。

## 9.1 M2 关键实证（2026-08-23）

- **getPairParams 方向敏感**：`getPairParams(WETH, USDC)` 正常返回，`getPairParams(USDC, WETH)` 返回空。engine 的报价方向 = (baseAsset 高价值资产, quoteAsset 计价资产)，与 `getPairs` 返回的 IPropAMM 标准地址排序（token0 < token1）**可能相反**。`init` 已做方向探测：getPairParams 失败时交换 token_a/token_b 并重算 virtual_address/lane_index。
- **lane 价格方向语义**：`fair_price_e8` = 1 baseAsset 以 quoteAsset 计价的 E8 定点。全部实测值自洽：WETH 系 ≈ 2373-2385、WBTC/cbBTC 系 ≈ 766xx、USDC/USDT ≈ 0.9998。
- **PairParams 结构**（WETH/USDC 实测）：`a=5e17, b=5000, c=d=3e12`；`c1` 正区间 8 段、`c2` 负区间 4 段，每段 (x1, x2, a, b, c, d) 为 int128 曲线段。WETH 系 `b=5000`，WBTC/cbBTC 系 `b=3333`，`a/c/d` 按交易对规模不同。
- **事件签名**（keccak 实测）：PairRegistered `0x04a8c4a4...`、PairUnregistered `0xc76e5bce...`、PairActiveSet `0xc098775b...`、Swapped `0x1eeaa4ac...`。
- **lane→槽位映射（M3.1 已破解，2026-08-24）**：registry lane 状态槽 =
  `keccak256(abi.encode(engine, laneIndex))`，即嵌套映射
  `mapping(caller => mapping(bytes32 laneIndex => uint256))`（外层 key = 调用方 engine，
  内层 key = laneIndex）。推导依据：engine.quote trace 中 registry 帧在 KECCAK256 前
  `MSTORE 0xa0 = engine`、`MSTORE 0xc0 = laneIndex`，`KECCAK256(0xa0, 0x40)` 结果直接
  SLOAD；8/8 pair 计算槽位与 Titan stateDiff 绝对槽位逐一命中；活链
  `eth_getStorageAt(registry, slot)` 返回合法打包 lane（值比快照新 8.5h，价格自洽）。

## 9.2 M3 关键实证（2026-08-23，trace 级）

- **quote 数学全破解**：`div1 = A*lane/1e(8+dec_diff)` → `a_norm = div1*1e18/D` → 8 档 c1 分段
  `price1 = P0*(1 ∓ delta/1e22)` → `out = A*price1/scale`（正向）/ `out = A*scale/price1`（反向）；
- **段参数交换**：公式用 `(c_eff, d_eff)`，等于 `getPairParams` 返回的 `(d, c)`（存储槽位
  slot2 高 128 位 = c_eff）；`delta` 恒加 L0.c_eff（5e17，WETH 系）；
- **IL 检查**：`max_output[tokenIn]`（槽位 8 mapping，key=keccak(abi.encode(token,8))）+
  vault(tokenIn) 余额，`vault + A > max_output` → `IL`；仅正向路径执行（反向 trace 不经过）；
- **COR 检查**：`a_norm <= L0.x1` → `COR`；反向边界 a_norm==x1（A=10000）链上输出异常
  （比正常小 1e6 倍），本地判 None；
- **反向大额封顶**：`a_norm > L7.x2` → 用 L7.x2 端点价 + `A_eff = x2*D/1e18`，out 恒为常量
  （WETH/USDC @fork = 1234075341762331184233 wei）；
- **vault 余额即 trace 中神秘常量**：WETH=1565110434027136947144、USDC=742928817572（@fork）；
- **跨 pair 参数已获取**：WBTC/USDC `a=5e17, b=3333, c=d=1e12, c1=6 档`（getPairParams 实测），
  待 lane 读取打通后对拍。

## 9.3 M3.1 关键实证（2026-08-24，registry lane→槽位映射）

- **公式**：`slot = keccak256(abi.encode(engine_address, laneIndex))`；
  `laneIndex = keccak256(abi.encode(tokenA, tokenB))`（ABI 32 字节左填充）；
- **打包格式**：`(updateTimestamp << 224) | (flag << 216) | fairPriceE8`；
  同批更新共享 updateTimestamp（Titan 快照 8/8 槽位均为 `0x6a8ae2b7`，flag=1）；
- **验证**：8/8 pair 计算槽位 = Titan stateDiff 绝对槽位（WETH/USDC
  `0xb4db...4107` 等）；活链 `eth_getStorageAt` 复读返回更新的合法 lane；
- **代码**：`fermi_registry_lane_slot(engine, a, b)` + 8 组真实槽位/价格单测
  （`registry_lane_slot_matches_titan_state_diff`）；`init` 优先存储槽直读，getState 兜底；
- **意义**：本地可对任意 pair 精确解码 Titan stateDiff（按 slot→lane 路由），
  也可独立拉取链上 lane 校准/对拍，不再受 getState 调用方与新鲜度限制。

## 9.4 M3.2 关键实证（2026-08-24，P0 失衡度公式修正，生产对拍）

- **废弃** `FERMI_ENGINE_K` 常量（旧 `P0 = lane*K/1e22` 错误）；
- **P0 真式**：`P0 = lane * (1e22 + c2_interp(M)) / 1e22`，其中
  `M = (L2 - S/2) * 1e18 / S`（int256 截断向零），`L1 = vault(token_b)`、
  `L2 = vault(token_a)*lane/scale`（quote 计价）、`S = L1 + L2`；M>0 时 delta_c2 = -2M，
  M<0 时 = -0.4M（c2 4 档线性插值）；trace 逐位证实（M=190801990854573678 →
  delta=-381603981709147356 @block 0x18a0d7b）；
- **段序修正**：c1/c2 段字段真实 ABI 顺序为 x=上界、y=下界、c=截距、d=斜率；
  `delta2 = c + p1*d/1e18 + c2[0].c`（X2=2e17），COR 判定用下界 y；
- **对拍**：生产 WS+eth_call（fresh lane override）双向多 A 逐位一致；合成 vault 余额
  sweep 覆盖 M∈[-4e17, +3e17] 与反向 vault 封顶；Rust 单测内嵌全部向量；
- **验证脚本**（一次性，未入库）：`/tmp/fermi_verify3.py`（68/69 全对拍）、
  `/tmp/fermi_sweep_m3.py`、`/tmp/fermi_trace_prod.json`（正向 trace）、
  `/tmp/fermi_trace_rev.json`（反向 trace）、`/tmp/fermi_implied.py`（隐含 delta1 反推）。

## 9.5 M4.5 关键实证（2026-08-25，last-trade 同块成交校正，trace 级）

- **last-trade 存储槽推导**：engine 每 pair 存"最后成交"记录，槽 =
  `keccak256(abi.encode(sub_key, keccak256(abi.encode(laneIndex, 7))))`（存储槽 7 的
  嵌套 mapping；`sub_key=0` 正向、`sub_key=1` 反向，槽位公式见 `fermi_prop/types.rs`）；
  值布局 `(last_trade_x << 64) | last_trade_block`（low 32 位 = 成交区块号）。推导依据：
  anvil fork + 存储注入 + `debug_traceCall` 观察 KECCAK256 输入与 SLOAD 槽位逐一吻合；
- **同块判定（anvil 存储注入实证 @25827361）**：仅当 `last_trade_block == block.number`
  时校正生效，相差 ≥1 块即忽略；本地用 `last_synced_block == 成交块` 判定，与链上一致；
- **正向（sub_key=0）**：`div1 = A*lane/scale + X`，再 `a_norm = div1*1e18/D`；
- **反向（sub_key=1）**：`a_norm = (A + X')*1e18/D` 仅用于**重新选档**，`a_eff` 保持原始
  A（校正不影响金额；trace 实证 @25828239 WETH/USDT，X'=13221602161）；
- **反向封顶与校正互斥（@25828950 WBTC/USDC REV A=1e12）**：`a_norm_raw = A*1e18/D`；
  `a_norm_raw > 末段.x` → 封顶（不经过校正路径，out 持平常量）；否则同块成交时校正后
  `a_norm > 末段.x` → 无段可匹配 → 链上 revert COR，本地返回 None 对齐；
- **漂移测试**（只跑 `cargo test --test fermi_prop`，不跑全库）：4-pair 1800 块 PASSED
  （全部检查点 vault 余额 + quote 逐位一致）；WETH/USDC 2500 块长程回归 PASSED
  （10/10 检查点、2643 事件）。

## 9.6 M4.6 关键实证（2026-08-25，失衡度/delta2 三项根因修复，生产对拍 + anvil trace）

4-pair 1800 块漂移对拍暴露 WBTC/cbBTC 系与 WETH 系差异，三项根因修复后本地 quote 与
链上逐位一致（代码在 `src/amms/fermi_prop/mod.rs`）：

1. **imbalance_m 基准**：`M = (L2 - S*b/1e4) * 1e18 / S`（SDIV 截断向零）。WETH 系
   b=5000 时 `S*b/1e4 ≡ S/2`，旧公式（S/2）对 WETH 巧合成立；WBTC/cbBTC 系 b=3333
   必须用 b。
   - 实证 @25827361 WBTC/USDC REV A=1e10：chain=12709898，旧 local=12709893（错 5e5）；
     lane=7867021438426、USDC vault=3213197061112、WBTC vault=28632379、b=3333；
     `M_chain=-326338600829916333`（旧 S/2 公式 = -493038600830070301）；
     `delta_c2_chain=195822742772227023`、`p0_chain=7867178778854`、
     `delta2→price1 ≈ 7867883538391`。
2. **delta2 常数项**：`delta2 = c + p1*d/1e18 + a`（a = pair_params.a = 2e17），旧公式
   误用 `c2[0].c`；cbBTC 系 c2[0].c=0 但链上仍加 2e17（打包参数槽字段提取实证）。
   - 实证 @25827458 cbBTC/USDC REV A=1e10：chain=12551774，旧 local=12552025（错 251）；
     lane=7966328275988、USDC vault=3211068564265、cbBTC vault=536130742、b=3333；
     `M=-215905927172908183`、`delta2_chain=844444444444444444 =
     644444444444444444 + 2e17`、`c2[0].c=0`。
3. **REV 分支同块成交校正 + 封顶互斥**（详见 §9.5）：`a_norm_raw > 末段.x` 时封顶（仅
   无校正路径）；否则同块成交校正后越界 → revert COR → 本地 None。
   - 实证 @25828950 WBTC/USDC REV A=1e12：链上 COR revert，错误旧 local=202127756；
     `sub1 X'=1020501321`、`trade_block=25828950`（同块）。
4. **c2 段匹配**：`y < M <= x`（内部序 x=上界、y=下界）；负区间段可能 c=d=0（cbBTC
   前两段）→ `delta_c2 = 0`、`P0 = lane`。

## 9.7 M4.5.1 关键实证（2026-08-25，真实 Titan WS 长跑验证）

- **脚本**：`tests/fermi_prop/ws_live_verify.rs`（`cargo test --test fermi_prop ws_live_verify
  -- --ignored --nocapture`；`#[ignore]` 守卫，只跑本用例不跑全库）；
- **对比方法**：Titan lane 链上看不到 → `state override` 注入 registry lane 槽位 +
  `update_timestamp` 改写为对拍块时间戳（绕过 `StaleUpdate`），链上在**固定块**（= 本地
  账本已完整回放的 `last_synced_block`）执行 `eth_call`；本地以同一 lane + 链上 last-trade
  槽（同块成交校正输入）计算 → 隔离验证"余额账本 + 曲线数学"两件事；
- **5 分钟生产实测（WETH/USDC）**：687 条快照（93 条含 Fermi lane），16 次 lane 更新
  （价格实时跳动 2496.59→2497.12→2496.58），17+1 个检查点全部 balance=OK / quote=OK，
  lane 同步 0 不一致，verdict PASSED；
- **10 分钟多 pair 生产实测（2026-08-25，WETH/USDC + WBTC/USDC + cbBTC/USDC）**：
  297 条快照（106 条含 Fermi lane），20 次 lane 更新（三 pair 价格实时跳动，WBTC/cbBTC
  系 b=3333 曲线被真实报价覆盖），60 个检查点全部 balance=OK / quote=OK，lane 同步 0
  不一致，verdict PASSED；
- **覆盖率**：检查点正向 0.01/1/10 base、反向 100/1e4/1e6 quote 基础单位（正常价/大额
  封顶/COR-IL revert 三类路径）；`FERMI_WS_VERIFY_SECS`/`FERMI_WS_VERIFY_CHECK_EVERY_SECS`/
  `FERMI_WS_VERIFY_PAIRS` 可调（支持多 pair 并行：WETH/USDC + WBTC/USDC + cbBTC/USDC 等）。

## 10. 风险与注意事项

- 闭源合约:quote 数学需字节码逆向(engine 23KB/swapper 10KB),工作量与 BinaryFi 相当;
- ~~lane→pair 槽位映射~~ ✅ 已解决（M3.1）：`slot = keccak256(abi.encode(engine, laneIndex))`，
  8/8 pair 与 Titan stateDiff / 活链 eth_getStorageAt 双验证;
- 当前模拟环境 Fermi 流未活跃(约一周前最后更新):验证需固定 block time 到 lane 时点;上线前需真实更新流;
- Titan 流无历史:断线只能回最新快照;需 slot 版本 + 链上 updateState 交叉校准;
- 高优先费交易输出置零(反抢跑):searcher 路径需避开;
- 新 venue 白名单演进:router 白名单(6 venue)与 price-levels 流(6 venue)集合不同,Fermi 先聚焦自身。

## 11. 参考

- Titan docs:`/propamms`、`/propamms/takers.md`、`/propamms/makers.md`
- lambdaclass `propamm-router-contracts`(IPropAMM + SDK 的 overrides 处理)
- Tycho `ethereum-fermiswap` substreams(事件/函数 ABI 参考)
- ERC-8324 Priority Update Registry(PUR)草案

## 12. 逆向工具链记录（2026-08-25 实测）

- **生产 RPC**：`https://ethereum-mainnet.core.chainstack.com/06920df668e96f928404674b359b251f`；
  仅用于 `eth_call`/`eth_getStorageAt` 对拍与事件回放。注意 **Chainstack 的
  `debug_traceCall` 不支持 state override** → 需要 override 注入的场景一律走 anvil fork；
- **state override 注入链路**：`anvil --fork-url <prod> --fork-block-number N` →
  `anvil_setStorageAt(engine/pair 槽, 值)` 注入 fresh lane / last-trade / 打包参数槽
  （值必须是 64 位十六进制字符填充，如 `0x` + 64 hex）→ `debug_traceCall`（anvil 内建
  structLogs 可用）逐指令核对 SLOAD/KECCAK256/算术；
- **关键 trace 文件**（未入库，/tmp 一次性）：
  - `/tmp/fermi_anvil_trace.json`：WBTC/USDC @25827361（REV A=1e10，M/delta2 实证）
  - `/tmp/fermi_cbbtc_trace.json`：cbBTC/USDC @25827458（REV A=1e10，delta2 常数 a 实证）
  - `/tmp/fermi_trace_prod.json` / `/tmp/fermi_trace_rev.json`：生产正向/反向 trace（M3.1）
- **一次性验证脚本**（未入库）：`/tmp/fermi_verify3.py`（68/69 全对拍）、
  `/tmp/fermi_sweep_m3.py`（失衡度 sweep）、`/tmp/fermi_implied.py`（隐含 delta1 反推）；
- **漂移测试**：`tests/fermi_prop/mainnet_sync_drift.rs`（`cargo test --test fermi_prop`，
  环境变量 `FERMI_DRIFT_BLOCK_RANGE`/`FERMI_DRIFT_CHECK_INTERVAL`/`FERMI_DRIFT_PAIRS` 可调；
  4-pair 1800 块 + WETH/USDC 2500 块回归已全 PASSED）。
- **WS 长跑验证**：`tests/fermi_prop/ws_live_verify.rs`（`#[ignore]`，`cargo test --test
  fermi_prop ws_live_verify -- --ignored --nocapture`）；2026-08-25 生产实测 PASSED——
  5 分钟单 pair（WETH/USDC：687 快照 / 16 lane 更新 / 17 检查点）+ 10 分钟三 pair
  （WETH/USDC + WBTC/USDC + cbBTC/USDC：297 快照 / 20 lane 更新 / 60 检查点），全部
  余额与 quote 逐位对齐，详见 §9.7。

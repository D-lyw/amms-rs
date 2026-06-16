# AMM 动态状态同步审计

> 分析日期: 2026-06-16
> 分析目标: 审查 `amms-rs` 各 AMM 协议模块在运行期是否能够仅依赖事件驱动完整维护动态状态，还是需要 `AsyncUpdate` / `Resync` / 周期性链上读取作为补偿链路。

## 目录

1. [审计口径](#一审计口径)
2. [三类分层结论](#二三类分层结论)
3. [第一类：事件后必须补链上读](#三第一类事件后必须补链上读)
4. [第二类：热路径事件可跑，但存在无事件漂移](#四第二类热路径事件可跑但存在无事件漂移)
5. [第三类：当前实现视为事件驱动足够](#五第三类当前实现视为事件驱动足够)
6. [维护建议](#六维护建议)
7. [关键代码索引](#七关键代码索引)

---

## 一、审计口径

本审计不以“协议理论上是否可以只靠事件同步”为标准，而以 **`amms-rs` 当前实现的真实行为** 为标准。

判断一个协议是否“无法仅靠事件驱动百分百获取动态数据”，采用以下口径：

1. `sync()` 在处理事件后显式返回 `SyncAction::AsyncUpdate` 或 `SyncAction::Resync`。
2. 协议实现了 `update()`，且 `update()` 会额外回链上拉取运行期动态字段。
3. `state_space/sync_services.rs` 中为该协议注册了后台周期刷新任务，且注释或实现明确说明存在“无事件漂移”或“事件流拿不全”的动态状态。

这三条中只要命中任一条，就说明该协议 **不能简单地被视为“纯事件完备”**。

### 1.1 框架级基础定义

`SyncAction` 的语义定义见 [src/amms/amm.rs](../src/amms/amm.rs)：

- `None`: 事件应用后无需额外动作。
- `AsyncUpdate`: 事件应用后需要异步补拉链上状态。
- `Resync`: 本地状态被认为已不完整或无效，需要完整重同步。

同时，`AutomatedMarketMaker::update()` 默认实现是 no-op；只有协议显式覆盖它，才说明该协议存在运行期额外补数需求。

---

## 二、三类分层结论

### 2.1 总表

| 分类 | 协议 |
|------|------|
| **第一类：事件后必须补链上读** | CurveNG, CurveLegacy, AlgebraIntegral(动态费插件部分), RocketPool |
| **第二类：热路径事件可跑，但存在无事件漂移** | BalancerV2, BalancerV3, AerodromeSlipstream(费率/费模块配置), FluidDex, Pendle, 以及同时属于第一类的 CurveNG / CurveLegacy / RocketPool |
| **第三类：当前实现视为事件驱动足够** | UniswapV2, SushiV2, PancakeV2, AerodromeV2, UniswapV3, PancakeV3, UniswapV4, PancakeInfinity, ERC4626, Ekubo, Sky |

### 2.2 协议级速查表

| 协议 | 分类 | 事件无法完全覆盖的动态数据 | 现有补偿链路 |
|------|------|----------------------------|--------------|
| CurveNG | 第一类 + 第二类 | `balances`, `stored_rates`, `price_scale`, `D`, `future_A_gamma_time`, `last_timestamp`, 部分 admin/fee runtime data | `AsyncUpdate`, `Resync`, `curve_rate_sync_task` |
| CurveLegacy | 第一类 + 第二类 | `balances`, `stored_rates`, `price_scale`, `D`, 部分 meta/runtime data | `AsyncUpdate`, `Resync`, `curve_rate_sync_task` |
| AlgebraIntegral | 第一类 | dynamic fee plugin context, `plugin`, `pluginConfig`, `community_fee`, `unlocked`, 动态费快照 | `AsyncUpdate` + `update()` |
| RocketPool | 第一类 + 第二类 | `total_eth_balance`, `reth_supply`, `excess_balance`, `maximum_deposit_amount`, `deposit_pool_balance`, `deposit_fee_rate`, `redeemable_eth` | `AsyncUpdate` + `update()` + `rocketpool_sync_task` |
| BalancerV2 | 第二类 | token rates from rate providers | `balancer_v2_rate_sync_task` |
| BalancerV3 | 第二类 | token rates, `swap_fee` | `balancer_v3_rate_sync_task`, `balancer_v3_fee_sync_task` |
| AerodromeSlipstream | 第二类 | `fee`, dynamic fee config, fee module globals | `slipstream_fee_sync_task`, `slipstream_fee_config_sync_task` |
| FluidDex | 第二类 | borrow/withdraw limits, `centerPrice` | `fluid_dex_limits_sync_task` |
| Pendle | 第二类 | `sy_exchange_rate`, `is_expired`, `_storage` 聚合态重校准 | `pendle_sync_task` |
| UniswapV2 / SushiV2 / PancakeV2 / AerodromeV2 | 第三类 | 无协议级额外动态字段 | 事件增量 + 异常时重同步 |
| UniswapV3 / PancakeV3 | 第三类 | 无协议级额外动态字段 | 事件增量 + `Resync` |
| UniswapV4 / PancakeInfinity | 第三类 | 无协议级额外动态字段 | 事件增量 + drift probe / `Resync` |
| ERC4626 | 第三类 | 当前实现未定义额外动态字段 | 事件增量 |
| Ekubo | 第三类 | 当前实现未定义额外动态字段 | 事件增量 |
| Sky | 第三类 | 近似静态 | 无运行期补数链路 |

### 2.3 应如何理解这三类

- **第一类** 不是“偶尔补一下更稳”，而是 **事件本身不够**，协议设计或当前实现决定了必须走补偿链路。
- **第二类** 的特点是：热路径的套利检测通常可以先跑，但协议里存在某些动态字段会“在没有订阅到对应事件的情况下变化”，所以长期运行必须后台刷新。
- **第三类** 并不等于“绝对不会漂移”，而是当前实现中 **没有协议级的额外动态字段补数需求**；一旦出错，通常靠 `Resync`、drift probe 或完整重建处理，而不是因为协议本身缺少必要事件。

---

## 三、第一类：事件后必须补链上读

## 3.1 CurveNG

### 3.1.1 结论

CurveNG 明确不是纯事件完备协议。无论是 StableSwap 还是 TwoCrypto / TriCrypto，当前实现都在多个事件路径后返回 `AsyncUpdate`，并且还存在某些事件只能 `Resync`。

### 3.1.2 事件为何不够

主要原因有三类：

1. 事件字段不足以还原池子完整动态状态。
2. 某些管理/手续费相关变更不会给出足够的币种维度信息。
3. Curve 池子的部分运行时数据会在没有标准 swap 事件的情况下变化。

典型例子：

- StableSwap `RemoveLiquidityOne` 无法仅靠事件确定完整本地状态。
- StableSwap `ClaimAdminFees` 事件不包含 coin index，无法知道扣的是哪一种币。
- Crypto 类型池子的 `price_scale`、`D`、时间参数等需要后续链上读取校准。

### 3.1.3 代码表现

- StableSwap 多个事件返回 `AsyncUpdate` / `Resync`，见 [src/amms/curve_ng/mod.rs](../src/amms/curve_ng/mod.rs)。
- `update()` 会额外拉取：
  - `balances`
  - `stored_rates`
  - `price_scale`
  - `D`
  - `future_A_gamma_time`
  - `last_timestamp`

### 3.1.4 维护含义

对于 CurveNG，不能把“事件增量更新逻辑已经写好了”理解为“后续不需要补链上读”。`AsyncUpdate` 和后台 rate sync 都是协议正确性的一部分，不是可有可无的优化。

## 3.2 CurveLegacy

### 3.2.1 结论

CurveLegacy 与 CurveNG 一样，明确属于必须补链上读的协议。

### 3.2.2 事件为何不够

CurveLegacy 同时覆盖 StableSwap 与 CryptoSwap 两套语义，问题来源包括：

1. 部分事件只给出余额变化，不给出完整运行时参数。
2. `RemoveLiquidityOne` 这类事件在某些场景缺少 coin index。
3. `price_scale`、`D`、`stored_rates`、meta 相关价格变量不能完全靠事件稳定维护。

### 3.2.3 代码表现

- 多个事件路径直接返回 `AsyncUpdate`。
- 对某些信息缺失场景直接返回 `Resync`。
- `update()` 会补拉：
  - CryptoSwap: `D`、`balances`、`price_scale`
  - StableSwap: `balances`、`stored_rates`、部分 meta/runtime 数据

### 3.2.4 维护含义

CurveLegacy 的事件流逻辑主要负责“先把本地状态推近正确值”，不能视为最终真相源。只要涉及 lending/rebasing/runtime rate 的变体，就要默认它需要后续链上校准。

## 3.3 AlgebraIntegral

### 3.3.1 结论

AlgebraIntegral 的核心 CL 池状态并不属于“整体事件不完备”，但它的 **动态费插件子系统** 明确不是纯事件完备。

### 3.3.2 哪部分事件不够

问题集中在 plugin-side state：

- `FeeConfiguration`
- `Plugin`
- `PluginConfig`

这些事件虽然能告诉我们“动态费上下文发生变化”，但并不能把本地所需的完整 fee context 全部恢复出来，因此当前实现会把配置标脏，再异步回读插件和池子的最新状态。

### 3.3.3 `update()` 会补哪些数据

- `plugin`
- `safelyGetStateOfAMM()`
  - `last_fee`
  - `pluginConfig`
  - `activeLiquidity`
  - `nextTick`
  - `previousTick`
  - `sqrtPrice`
  - `tick`
- `globalState()`
  - `community_fee`
  - `unlocked`
- `isUnlocked()`
- 动态费快照

### 3.3.4 维护含义

如果未来只看 swap/mint/burn 路径，很容易误判 AlgebraIntegral 是“纯事件协议”。更准确的说法是：

- **池子本体状态大体事件完备**
- **动态费插件状态不完备，必须补链上读**

## 3.4 RocketPool

### 3.4.1 结论

RocketPool 是最典型的“事件只告诉你变了，但不知道变成什么”的协议之一。

### 3.4.2 事件为何不够

RocketPool 的兑换能力依赖协议级会计状态与存款池状态的组合，而不是某个池子局部事件就能完整表达。

本地需要的关键动态字段包括：

- `total_eth_balance`
- `reth_supply`
- `excess_balance`
- `maximum_deposit_amount`
- `deposit_pool_balance`
- `deposit_fee_rate`
- `redeemable_eth`

这些值都不能仅从订阅到的 deposit-pool 事件精确推导。

### 3.4.3 代码表现

- `sync()` 直接无条件返回 `AsyncUpdate`
- `update()` 通过批量链上调用整套刷新上述字段

### 3.4.4 维护含义

RocketPool 不是“事件同步 + 偶尔纠偏”，而是“事件只是触发器，真正状态要靠后续链上读”。

---

## 四、第二类：热路径事件可跑，但存在无事件漂移

这一类协议的特点是：

- 套利热路径通常可以直接使用事件增量更新后的本地状态。
- 但协议中存在某些运行时字段会在没有被当前订阅流完整覆盖的情况下变化。
- 如果长期不做后台刷新，本地状态会慢慢偏离链上真值。

## 4.1 BalancerV2

### 4.1.1 结论

BalancerV2 当前不是纯事件完备协议。

### 4.1.2 原因

当前实现只同步 Vault 侧余额事件，不同步 pool-side rate cache 更新事件。对于带 rate provider 的 token，rate 变化并不会通过当前事件流完整反映出来。

### 4.1.3 后台补偿链路

存在专门的 `balancer_v2_rate_sync_task`，用于周期性刷新 rate provider 对应的 rate。

### 4.1.4 维护含义

如果未来发现 BalancerV2 spot price 长时间缓慢偏移，但没有任何 sync 异常日志，优先怀疑 rate provider 漂移，而不是先怀疑 Vault balance 事件丢失。

## 4.2 BalancerV3

### 4.2.1 结论

BalancerV3 也不是纯事件完备协议。

### 4.2.2 原因

热路径可以同步 swap / pool balance change，但以下动态字段需要后台刷新：

- token `rate`
- `swap_fee`

原因包括：

1. rate provider 自身变化不会完整体现在当前事件流里。
2. `swap_fee` 可能由治理更新，或本身是动态费。

### 4.2.3 后台补偿链路

- `balancer_v3_rate_sync_task`
- `balancer_v3_fee_sync_task`

### 4.2.4 维护含义

BalancerV3 的“事件同步正确”并不等价于“报价一定正确”；rate 和 fee 都可能单独漂移。

## 4.3 AerodromeSlipstream

### 4.3.1 结论

AerodromeSlipstream 的核心 CL 状态基本能靠事件维护，但 **费率与动态费配置** 不是完全事件完备。

### 4.3.2 事件已覆盖的部分

- `sqrt_price`
- `tick`
- `liquidity`
- `ticks`
- `observations` 的本地缓存维护

### 4.3.3 事件未完全覆盖的部分

- `fee`
- dynamic fee module config
- fee module globals

当前实现里 even though `CustomFeeSet` 事件能同步一部分 fee 变化，仍明确不依赖它作为唯一真相源，因为动态 fee 有可能在我们的事件流水线中出现漂移。

### 4.3.4 后台补偿链路

- `slipstream_fee_sync_task`
- `slipstream_fee_config_sync_task`

### 4.3.5 维护含义

Slipstream 不能简单归类成“和 UniswapV3 一样”。更准确地说：

- CL 池子主状态像 V3
- 费模块运行时状态更像“需要独立补数的外挂系统”

## 4.4 FluidDex

### 4.4.1 结论

FluidDex 不是纯事件完备协议。

### 4.4.2 原因

虽然 swap 和 `LogOperate` 事件可以更新大量本地状态，但协议存在时间相关的动态变量：

- borrowable / withdrawable limits 会随时间扩张
- `centerPrice` 会缓慢漂移

这类变化即使没有新的事件，本地状态也会过时。

### 4.4.3 后台补偿链路

`fluid_dex_limits_sync_task`

### 4.4.4 维护含义

FluidDex 的事件同步更像“离散修正”，不是“完整连续真值流”。它天然需要周期性拉取 resolver 数据。

## 4.5 Pendle

### 4.5.1 结论

Pendle 事件热路径可用，但并不纯事件完备。

### 4.5.2 事件已覆盖的部分

- `total_pt`
- `total_sy`
- `last_ln_implied_rate`

### 4.5.3 事件未覆盖的部分

- `sy_exchange_rate`
- `is_expired`
- `_storage` 中聚合出来的部分运行时状态重校准

其中最关键的是 `sy_exchange_rate` 会缓慢变化且没有事件通知。

### 4.5.4 后台补偿链路

`pendle_sync_task`

### 4.5.5 维护含义

Pendle 看起来事件字段很多，但仍不能因此误判为纯事件协议。只要底层 SY 是计息资产，就必须默认 exchange rate 会“无事件漂移”。

## 4.6 同时属于第一类与第二类的协议

以下协议既存在 **事件后立即补读**，又存在 **长期后台漂移兜底**：

- CurveNG
- CurveLegacy
- RocketPool

维护上应理解为：

- 即时 `AsyncUpdate` 解决“当前事件后的短时正确性”
- 后台周期任务解决“长时间运行下的慢性偏移”

---

## 五、第三类：当前实现视为事件驱动足够

这一类协议在当前 `amms-rs` 设计中，没有协议级“必须额外补数”的动态字段链路。若本地状态出错，主要依靠：

- 事件增量维护
- `Resync`
- drift probe
- 完整 `sync_all_pools` / 重新初始化

而不是因为协议本身还缺一条专门的异步读取通道。

## 5.1 V2-like

包括：

- UniswapV2
- SushiV2
- PancakeV2
- AerodromeV2

核心动态状态就是 reserves，事件中能直接拿到或足够推导，当前实现没有额外 `update()` 或后台 sync task。

## 5.2 V3-like CL

包括：

- UniswapV3
- PancakeV3

核心动态状态 `sqrt_price / tick / liquidity / ticks` 由 swap/mint/burn 增量维护。出错时返回 `Resync`，但不是因为存在协议级“无事件漂移”的额外动态字段。

## 5.3 V4 / Infinity

包括：

- UniswapV4
- PancakeInfinity

当前设计视 PoolManager 日志为核心真相源，运行期未实现专门 `update()` 覆盖，也没有协议专属后台 sync task。它们的 drift probe / resync 语义属于 **完整性保护**，而不是“事件天生不够”。

## 5.4 其他

- `ERC4626`
  当前实现用 `Deposit/Withdraw` 维护储备关系，没有专门异步补数链路。

- `Ekubo`
  实现了 `update()` 接口，但当前是 no-op，等价于“当前版本把事件视为足够”。

- `Sky`
  当前实现被当作近似静态转换器，不依赖运行期事件同步。

### 5.5 这一类的维护注意点

“当前视为事件驱动足够”不等于“永远不会出问题”。如果后续发现：

- 某协议新增了动态费模块
- 某协议存在链下或链上隐式 rate 漂移
- 某协议的某些 runtime view 值会在无事件下改变

则该协议应从第三类上调到第二类或第一类。

---

## 六、维护建议

## 6.1 新协议接入时的判断顺序

接入新协议时，建议按下面顺序判断它应该落在哪一类：

1. 核心报价所需动态字段有哪些？
2. 这些字段是否都能从订阅到的事件流中唯一恢复？
3. 是否存在治理参数、利率、oracle、rate provider、插件、费模块、时间衰减变量？
4. 这些变量是否会在“没有当前订阅事件”的情况下变化？
5. 如果事件应用失败或信息不够，是否应该 `AsyncUpdate` 还是直接 `Resync`？

## 6.2 协议分类的维护优先级

- **第一类**
  最高优先级保证 `AsyncUpdate` / `Resync` 正确性和可观测性。

- **第二类**
  重点保证后台任务的覆盖率、批量调用效率、失败告警和 fail-closed 语义。

- **第三类**
  重点保证事件增量逻辑与 drift probe 的一致性。

## 6.3 什么时候要重新审计

以下情况发生时，应重新审计本文件对应协议的分类：

- 协议升级新增动态费、hook、oracle、rate provider、wrapper 层。
- `sync()` 新增 `AsyncUpdate` / `Resync` 路径。
- 新增后台 `sync_task`。
- 实盘发现“没有事件异常日志，但报价长期偏移”的现象。

---

## 七、关键代码索引

### 7.1 框架层

- `SyncAction` 定义: `src/amms/amm.rs`
- `AutomatedMarketMaker::update()` 默认 no-op: `src/amms/amm.rs`
- 后台同步任务集合: `src/state_space/sync_services.rs`

### 7.2 第一类协议

- CurveNG: `src/amms/curve_ng/mod.rs`
- CurveLegacy: `src/amms/curve_legacy/mod.rs`
- AlgebraIntegral: `src/amms/algebra_integral/mod.rs`
- RocketPool: `src/amms/rocketpool/mod.rs`

### 7.3 第二类协议

- BalancerV2: `src/amms/balancer_v2/mod.rs`
- BalancerV3: `src/amms/balancer_v3/mod.rs`
- AerodromeSlipstream: `src/amms/aerodrome_slipstream/pool.rs`
- FluidDex: `src/amms/fluid_dex/mod.rs`
- Pendle: `src/amms/pendle/mod.rs`
- 周期任务入口: `src/state_space/sync_services.rs`

### 7.4 第三类协议

- UniswapV2: `src/amms/uniswap_v2/mod.rs`
- SushiV2: `src/amms/sushi_v2/mod.rs`
- PancakeV2: `src/amms/pancake_v2/mod.rs`
- AerodromeV2: `src/amms/aerodrome_v2/pool.rs`
- UniswapV3: `src/amms/uniswap_v3/mod.rs`
- PancakeV3: `src/amms/pancake_v3/mod.rs`
- UniswapV4: `src/amms/uniswap_v4/mod.rs`
- PancakeInfinity: `src/amms/pancake_infinity/mod.rs`
- ERC4626: `src/amms/erc_4626/mod.rs`
- Ekubo: `src/amms/ekubo/pool.rs`
- Sky: `src/amms/sky/mod.rs`

---

## 附录：一句话判断法

为了长期维护时快速判断，可用下面这条经验规则：

- **如果一个协议的关键报价字段会在没有当前订阅事件的情况下变化，它就不属于纯事件完备协议。**
- **如果一个协议在 `sync()` 后还需要 `AsyncUpdate`，那它一定不属于纯事件完备协议。**
- **如果一个协议只在异常时 `Resync`，但平时不需要额外补数，它通常属于第三类。**

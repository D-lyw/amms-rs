# CurveLegacy 初始化识别与 MetaPool 检测说明

## 1. 目标

`CurveLegacyPool::init()` 现在承担的不只是“把链上字段抄下来”，而是完整决定：

- 这个池子属于 `StableSwap` 还是 `CryptoSwap`
- 它是否是 `MetaPool`
- 如果是 `MetaPool`，它依赖的 `base pool` 是谁
- `underlying_coins` 的展开顺序是什么
- 本地是否已经具备安全可用的 quote / simulation 前置条件

对应地，`CurveLegacyFactory::init_batch()` 还要保证：

- 批量初始化时不会丢失这些识别逻辑
- `MetaPool` 依赖的 `base pool` 会作为顶级一等池补回结果集

这份文档只描述**当前代码版本**的识别结构，供后续维护参考。

## 2. 初始化主流程

`CurveLegacyPool::init()` 中与“类型识别”直接相关的主流程分为 3 层：

### 2.1 Family 分类

入口函数：

- `classify_pool_family()`

目标：

- 判定池子应该按 `CurveLegacyPoolType::StableSwap`
  还是 `CurveLegacyPoolType::CryptoSwap` 处理

规则：

- 若 `gamma / D / mid_fee / out_fee / fee_gamma` 这 5 个 Crypto 能力全部存在，则判定为 `CryptoSwap`
- 若只存在其中一部分，则直接 `fatal init error`
- 否则只要 `A()` 存在，则判定为 `StableSwap`

这里采用 **fail closed** 策略，而不是“尽量猜”。原因是：

- 半残的 Crypto 参数集最危险
- 一旦误判为 Crypto，本地报价会在不完整参数下运行，风险比跳过池子更大

### 2.2 Meta topology 检测

入口函数：

- `detect_meta_topology()`

目标：

- 识别池子是否为 `MetaPool`
- 若是，则解析并物化：
  - `base_pool_address`
  - `base_lp_token`
  - `base_token_index`
  - `underlying_coins`
  - `base_pool_view`

检测顺序固定如下：

1. `base_pool()` 直接探针
2. `coins[last]` 本身就是 base pool 合约
3. `coins[last]` 是 Curve LP Token，通过 `minter()` 解析 base pool
4. Ethereum old `StableMeta` registry fallback

顺序不能随意改：

- 前 2 条是更直接、更低歧义的探针
- 第 3 条是旧工厂 LP-token meta pool 的关键兜底
- 第 4 条是最特殊的 Ethereum 老池兜底，应始终放最后

### 2.3 子类型与可支持性校验

入口函数：

- `classify_stable_subtype()`
- `validate_supported_topology()`

目标：

- 对 `StableSwap` 进一步分类为 `Plain / Lending / Meta`
- 对所有已识别出的 `MetaPool` 做本地可模拟性校验

`validate_supported_topology()` 当前要求：

- `base_pool_address / base_lp_token / base_token_index` 必须齐全
- `underlying_coins.len()` 必须大于 `coins.len()`
- `base_pool_view` 必须已经物化
- `base_pool_view.pool_type` 必须是 `StableSwap`

如果这些条件不满足，直接报 `fatal init error`，避免把“不完整 meta”带入生产链路。

## 3. 各条 Meta 检测路径的适用场景

### 3.1 `base_pool()` 直接探针

入口：

- `probe_base_pool_address()`

适用：

- 新版 Legacy MetaPool
- Arbitrum 等链上较新的 MetaPool 部署

优点：

- 直接
- 歧义低

### 3.2 `coins[last]` 本身是 base pool

入口：

- `try_init_stable_base_pool_candidate()`

适用：

- 某些池子在 `coins[last]` 中直接放 base pool 合约地址

特点：

- 用现有普通 `CurveLegacyPool` 初始化流程，直接尝试把它当 StableSwap 池子初始化

### 3.3 LP Token `minter()` 路径

入口：

- `probe_lp_token_minter()`

适用：

- 旧工厂部署的 MetaPool
- `coins[last]` 不是 base pool 合约，而是 base pool LP Token

逻辑：

- 先尝试“新版 LP Token: `get_virtual_price()` + `minter()`”
- 再回退到“旧版 LP Token: 直接 `minter()`”

这是 Ethereum old `CryptoMeta` 的关键识别路径。

### 3.4 Ethereum old `StableMeta` registry fallback

入口：

- `detect_stable_meta_via_registry_fallback()`

适用：

- Ethereum 主网最老那批 StableMeta
- 它们既不暴露 `base_pool()`，也无法通过最后一个 coin 直接初始化为 base pool

当前设计约束：

- 仅在 `chain_id == 1` 时启用
- 使用硬编码 Curve AddressProvider
  `0x0000000022D53366457F9d5E68Ec105046FC4383`
- 通过 `get_registry()` 找 main registry
- 再用 `is_meta / get_n_coins / get_underlying_coins / get_pool_from_lp_token / get_lp_token`
  补全 topology

这里是 **fail-fast**：

- 一旦进入这个 fallback，若 AddressProvider / Registry 解析失败，不会静默降级成 plain pool
- 会直接返回 `fatal init error`

原因：

- 这条路径本身就是为“最老 StableMeta 特殊 case”准备的
- 若这里失败还静默吞掉，最容易把真实 meta 池误识别成 plain

## 4. `underlying_coins` 的语义

当前约定：

- `coins` 保持链上 direct coin 语义
- `n_coins` 保持 direct coin 数量
- `balances` 与 `n_coins` 一一对应
- `underlying_coins` 只表示扩展后的底层币空间

对于 MetaPool：

- `underlying_coins` 的顺序必须与链上的 `exchange_underlying(i, j)` 索引一致
- `expand_underlying_with_base_pool()` 负责把 base pool 的 coins 按 `base_token_index`
  展开到 meta pool 的 underlying 空间

## 5. Batch Init 的额外职责

`CurveLegacyFactory::init_batch()` 当前虽然还是“并发逐池 `init()`”，但语义上已经必须保证两件事：

### 5.1 不得绕过单池识别逻辑

也就是说，任何未来如果恢复 dedicated batch RPC path，都必须完整覆盖：

- family 分类
- meta topology 检测
- stable subtype 分类
- base_pool_view 物化
- supported topology 校验

### 5.2 必须补回 base pool 顶级依赖

入口：

- `collect_missing_base_pool_dependencies()`

目标：

- 把 `MetaPool` 的 `base pool` 作为顶级池加入返回结果

原因：

- 上层 `state space / graph / execution` 需要把 base pool 当作独立一等池同步维护
- 不能只把它藏在 meta pool 内部镜像里

## 6. 测试结构

当前与初始化识别直接相关的测试主要分成几类：

### 6.1 固定业务输入样本

- `tests/curve_legacy/pool_index_ethereum.rs`

作用：

- 验证 `pool_index_1.json` 中真实使用到的 Ethereum plain Legacy pools 不被误识别成 meta

### 6.2 DB 快照矩阵

- `tests/curve_legacy/db_snapshot_matrix.rs`

作用：

- 覆盖 Ethereum / Arbitrum / Base 上更多 Legacy pool 类型
- 包括 Ethereum 全量 legacy meta matrix
- 以及 DB 标签异常样本校验

### 6.3 Fork case 验证

- `tests/curve_legacy/meta_pool_fork.rs`
- `tests/curve_legacy/meta_pool_fork_ethereum.rs`

作用：

- 验证 Arbitrum / Ethereum 的 stable meta 与 crypto meta 真实 fork case
- 同时验证：
  - 检测是否正确
  - `underlying_coins` 是否正确展开
  - 本地 quote 与链上 quote 是否一致
  - `with_amms` / state-space 是否补回 base pool 顶级依赖

### 6.4 Crypto 精度回归

- `tests/curve_legacy/recalculate_precision.rs`

作用：

- 验证 Legacy Crypto 本地数学实现与链上 quote 的一致性
- 其中 `Eth-LDO-USDC` 用例专门锁住了 2-coin legacy crypto 的 1 wei rounding 回归

## 7. 维护建议

后续维护时，建议遵守下面几条：

### 7.1 不要把“识别”和“附着状态”混在一起改

建议保持：

- `detect_meta_topology()` 只负责“发现 topology”
- `apply_meta_topology_state()` 只负责“把 topology 写回池状态”

这样比较容易定位问题，也方便测试。

### 7.2 不要把特殊 fallback 提前

尤其是：

- LP token `minter()` fallback
- Ethereum old StableMeta registry fallback

这些都应该保留在“越特殊越靠后”的顺序。

### 7.3 新增链/新工厂 case 时，优先补测试矩阵

如果未来再发现新的 LegacyPool 边界情况，建议先补：

- 对应链上的 fork case
- DB snapshot matrix 样本

再改检测逻辑。否则很容易修掉一个点、打坏另一个点。

### 7.4 若未来重做 dedicated batch init

必须把这份文档第 2 节的主流程完整映射过去，不能只搬运静态字段拉取逻辑。

否则最容易出现的问题是：

- 单池 `init()` 正确
- batch `init_batch()` 漏字段
- 生产里真正跑批量初始化时反而退化

## 8. 当前结论

以当前版本而言，`CurveLegacy` 初始化识别逻辑已经从“分散的探针拼接”收敛成了较清晰的结构：

- 先分 family
- 再识别 meta topology
- 再做 subtype/supportability 校验
- batch path 负责补回 base pool 顶级依赖

代码结构已经比之前清晰很多，但这套逻辑涉及多个历史兼容分支，不适合只靠读源码记忆维护。

因此：

- **代码里保留关键顺序注释是必要的**
- **配套维护文档也是必要的**

这份文档就是为长期维护准备的结构说明。

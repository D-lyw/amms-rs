# ElfomoFi propAMM（XLayer）集成调研与精准模拟设计文档

> 目标：参照仓库 `binaryfi_prop`/`caliber_prop` 的架构模式，将 XLayer 上新上线的
> ElfomoFi PropAMM 集成为 `elfomo_prop` 模块，且**本地 Swap 模拟与链上逐位一致**。
>
> 结论先行：**不需要**妥协为 KyberSwap 式"黑盒采样拟合"。链上报价是
> **确定性的分段线性函数**（orderbook 档位阶梯），状态可读、公式可逆，
> 完全走 `binaryfi_prop` 同款路线（反汇编 + 采样对拍 + 逐位验证）即可。
> Kyber 的 15 点采样法只是外部拟合，档位间线性插值、超出采样区间截断，
> 无法做到逐位一致，仅适合做市商路由场景。
>
> 调研日期：2026-09-01。验证锚点块：`0x423c2b8` = 69452472（XLayer）。

## 1. 合约架构与地址（XLayer，已链上实测）

| 角色 | 地址 | 说明 |
|---|---|---|
| Router（交互入口） | `0xf0f0f0F0FB0d738452EfD03A28e8be14C76d5f73` | 普通合约（非代理）；报价/列池/swap 全走它；swap 事件由它 emit |
| Factory 代理 | `0xffffffbb2d432b8acb4c57d556c0c721a431d038` | TransparentUpgradeableProxy |
| Factory 实现 | `0x406644607f87ecf0adc4c0c9c64705a9de1c5e31` | `getOrderbook`(Router-only)、`swap 0x519341bb`、pair→pool 映射 |
| 金库 | `0xbb1b19f138db3925883a96ff7a304277460e0c99` | 极简代理 → Gnosis Safe 实现 `0x29fcb43b...`，仅持币 |
| OKX ElfomoAdapter | `0xe415dd1c60719400726f9712b904fff522cf9cc6` | OKX DEX 聚合器适配器（开源），实测调用路径 |
| Pair：xETH | `0xe7b000003a45145decf8a28fc755ad5ec5ea025a` | 18 位小数 |
| Pair：USDT0 | `0x779ded0c9e1022225f8e0630b35a9b54be713736` | 6 位小数 |

官方文档：<https://docs.elfomo.fi/integration>（标注 BSC/Base，但同一 Router 地址已部署 XLayer）。

### 关键事件

| 事件 | 签名/说明 |
|---|---|
| `ElfomoTrade`（Router emit，topic0 `0xbe65a3f1f381da16732df786f571604a72b7c122cff3ae2b355566ddf01e2528`） | data = [executor, receiver, fromToken, toToken, fromAmount, toAmount]；topics = [quoteId, partnerId]（实测套利交易 data[0]=adapter） |
| `updatePrices` 空事件（Pool emit，topic0 `0xc5d08cbe6fd3ebc24e5a483616dddbc63b2aff5c082c7d697603ab521079f809`） | 每块 1 笔（MM keeper 调 `0xae7e8d81`），data 空，仅作价格漂移实时触发信号（详见 §3.3） |
| `PairAdded(address,address)`（Factory） | topic0 `0xc26cc795...`（已实测） |

## 2. 接口层（文档 + 链上实测确认）

```
getSupportedPairs() -> TokenPair[]                       // 实测返回 1 对 (xETH, USDT0)
getAmountOut(fromToken, toToken, fromAmount) -> toAmount // 实测双向报价正常
getAmountIn(fromToken, toToken, toAmount) -> fromAmount
swap(fromToken, toToken, int256 specifiedAmount, uint256 limitAmount,
     receiver, uint256 partnerId)                        // selector 0x598edcad（adapter 实测）
swapWithContractBalance(fromToken, toToken, uint256 minAmountOut, receiver, partnerId)
swapWithCallback(fromToken, toToken, int256, uint256, receiver, partnerId, bytes)
```

- `specifiedAmount` 正 = exact-in，负 = exact-out；`limitAmount` = 最小 out / 最大 in。
- 聚合器可自行加价（加价部分月底 USDC 返佣）；approve 目标 = Router。
- 内部：`Router.getAmountOut` → STATICCALL `Factory.getOrderbook(from,to)`（Router-only，
  eth_call 可伪装 `from=Router` 直读）→ 返回**两侧各 3 档 (size, price)** → Router 本地计算。

## 3. 报价模型（固定块采样实证：分段线性 orderbook）

### 3.1 精确报价模型（固定块 `0x423c2b8` 逐位对拍锁定；真实链 10 块 + anvil vault 全量扫描复验）

`Factory.getOrderbook(xETH, USDT0)`（selector `0x0a6e04cb`，公开可调）返回
两个 `(size, price)[]` 数组，与对拍公式的档位**完全一致**（本块）：

```
arr0（xETH→USDT0，size=输入量）：(0.6e18, 2473060529144115)
                                  (3.0e18, 2472986332134450)
                                  (4161015515317950639, 2472862670451675)
arr1（USDT0→xETH，size=输出量）：(0.6e18, 2473406781855885)
                                  (1740462501000862186, 2474964919058850)
                                  (0.6e18, 3709850483250000)
```

**正向 exact-in（`getAmountOut(xETH, USDT0, in)`）**：逐档
`out += floor(take_i × price_i / 1e24)`，`take_i = min(剩余输入, size_i)`，
总量封顶 `min(总输出, vault USDT0 余额)`。验证 13/13 精确命中（含
0.6e18 / 3.6e18 档界、封顶 19192415251）。

**反向 exact-in（`getAmountOut(USDT0, xETH, in)`）**：`need_i = ceil(size_i × price_i / 1e24)`；
`剩余 ≥ need_i` 时 `out += size_i`，否则 `out += floor(剩余 × 1e24 / price_i)`；
封顶 `min(out, vault xETH 余额)`。`s1+s2+s3 == vault xETH 余额`
（= 2940462501000862186，整仓背书）。验证 20/20 精确命中（含 B1=1484044070、
B2=5791627703、cap=8017537993 边界）。

**正向 exact-out（`getAmountIn(xETH, USDT0, to)`）**：
容量 `C = Σ floor(size_i × price_i / 1e24)`（本块 19192415251）；`to > C → 0`；
逐档 `rem ≥ level_out_i` 时 `in += size_i`（取满），否则 `in += ceil(rem × 1e24 / price_i)` 终止。
验证含 `to = C-3..C`、第 2/3 档边界全部逐位命中。

**反向 exact-out（`getAmountIn(USDT0, xETH, to)`）**：
`to > vault xETH 余额 → 0`；逐档 `rem ≥ size_i` 时 `in += need_i`（取满），
否则 `in += ceil(rem × price_i / 1e24)` 终止。验证含 `to = vault-2..vault+1` 边界命中。

**orderbook 生成公式（2026-09-01 破解，`build_orderbook` 逐位一致）**：

`debug_traceCall` 实证：Pool 每次读取 orderbook 都**实时 3 次 staticcall
`token.balanceOf(vault)`** —— orderbook 不是持久化状态，而是
`(price_seed a, vault_usdt0, vault_xeth)` 的**读时纯函数**，本地必须同构重算：

- `a = slot1 >> 32`；`q = (a >> 22) & 0x3f`；`qs = q>=32 ? q-64 : q`；
  `low = a & 0x3fffff`；`base = (100000 + qs) × low`；每档 `price = slope × base`。
- **from→to 档位**（size=输入量）：宽度/偏离按该 pool 的 ladder 参数**现算**
  （`U=unit, T=band_count, SL=spread_level, F=spread_penalty`，完整规则见 §5.2.2）；
  `rem = vault_usdt0 × 1e24`，逐档 `cap = rem // price`、`s = min(width[i], cap)`，
  `rem -= ceil(s×price/1e24) × 1e24`，`s < width[i]` 即停（余量档）。
- **to→from 档位**（size=输出量）：同一套 ladder 参数；`rem = vault_xeth`，
  逐档 `s = min(width[i], rem)`，`s < width[i]` 即停；**尾档恒显示**
  （`s = min(U, rem)`）。
- **加宽开关由金库余额决定，不是静态配置**：`SL·U ≥ 本侧容量` 时该侧 `i≥1` 的档位
  偏离再 `+F`（前缀只有 1 档时该档也 `+F`）。真实池 `U=0.6e18, T=30, SL=5, F=60`：
  `vault_xeth ∈ (0, 5U]` 时 to→from 已加宽；`from→to` 在容量 `(T−1)U − vault_xeth ≤ 5U`
  时加宽（即 vault 接近 `(T−1)U` 时）。
- **对拍结果（2026-09-11 重做，覆盖旧"打表"结论）**：anvil fork XLayer +
  `eth_call` stateOverride 全网格（`T∈{1..6,10,30,200}` × `F∈{0,60,120}` ×
  `SL∈{0,1,5,29,30}` × 9 档 vault 组合 + 真实池状态）共 **30,240 组逐位一致**；
  其中 490 组固化为 `src/amms/elfomo_prop/fixtures/ladder_grid.json` + 单测
  `test_chain_ladder_grid_bit_exact`（零 RPC 回归）。真链当前 head 亦用
  "真实 `(seed, vault)` → 本地重算 == 链上 `getOrderbook`" 逐位复核通过。

### 3.2 价格每块漂移

`latest` 与固定块的采样边际价有微小差异（~0.01% 量级），确认官方文档
"Prices are updated every block based on a mix of the ElfomoFi oracle and
additional onchain signals" —— **状态每块都在变，本地不能自持状态太久**。

### 3.3 报价更新机制（2026-09-01 链上实证：每块 1 笔 `updatePrices` 交易，种子在 calldata 里）

这是本地实时驱动的核心抓手（锚点块附近连续 25 块 + 真实链 10 块命中）：

- **每块恰好 1 笔** `updatePrices(uint256)`（selector `0xae7e8d81`）由 MM keeper
  （`0x8121003eb12a97900d1e84097f864420a9a95923`）发给 **Pool**
  （`0x02dcdf…9459a`）。
- Pool 同步 emit 一条**空 data 事件**，topic0
  `0xc5d08cbe6fd3ebc24e5a483616dddbc63b2aff5c082c7d697603ab521079f809`；
  同时仅 SSTORE `slot1` = `(a << 32) | ts`。
- **关键破解：calldata 参数就是价格种子。** 实测 `arg ≈ (a << 32) | (ts-1)`，
  `a = arg >> 32` 直接等于 Pool `slot1 >> 32`。因此**从 flashblocks 原始交易
  即可本地解析出种子 `a`**，再用 §3.1 公式 + 本地金库余额重算整本 orderbook，
  **关键路径零 RPC**。
- base 配置在 `slot 0x68841630655ba9ff80839ef53d68d0d812abc4b78dc8e3a7ce833922727118cd`
  （值 `0x…003c0005001e00000000000000000853a0d2313c0000000000021200`，不变）。

## 4. 为什么 KyberSwap 的采样法不够（本模块不做）

Kyber 实现（`kyberswap-dex-lib/pkg/liquidity-source/elfomofi/`）：
- 列池 = `getSupportedPairs()`；状态 = 对 `getAmountOut` 采样 15 个金额点
  （10 的幂次网格），相邻点线性插值重建"边际 orderbook"；模拟按档消耗。
- 缺陷：① 档内真实边际价可能是任意阶梯，两点插值 ≠ 链上真值；② 采样区间外
  直接判"流动性不足"，无法表示封顶后的真实行为；③ 档位消耗状态是本地近似，
  链上状态每块漂移 → 本地与链上必然发散。
- 结论：只适合路由报价，不适合套利引擎的"逐位一致"要求。

## 5. 同步与模拟架构（参照 binaryfi_prop / caliber_prop）

### 5.1 模块结构（`src/amms/elfomo_prop/`）

```
elfomo_prop/
  mod.rs      // ElfomoFiPropPool：AutomatedMarketMaker impl、quote 公式、
              //   build_orderbook（(seed, vault) 读时纯函数）、模型自证、覆盖率自证
  types.rs    // OrderbookLevel / OrderbookSnapshot / ElfomoLadderConfig（每 pair ladder）
  factory.rs  // DiscoverySync：getSupportedPairs 列池 + getOrderbook 快照
  ledger.rs   // VaultDeltaLedger（金库增量 checkpoint + redo log）
```

### 5.2 同步策略（两条数据源：raw-tx 种子 + ElfomoTrade 金库增量，RPC 仅对账兜底）

报价更新机制（§3.3）决定**只有两个状态源**：

| 状态 | 唯一来源（实时） | 通道 |
|---|---|---|
| 价格种子 `a` | `updatePrices(uint256)` 交易的 calldata（`a = arg >> 32`） | flashblocks 原始交易流（零 RPC） |
| 金库余额 `(usdt0, xeth)` | Router `ElfomoTrade` 的 `fromAmount/toAmount` 增量 | 日志流（零 RPC） |

1. **raw-tx 种子通道（价格，主）**：`xlayer_flashblocks` 按
   `to ∈ elfomo_pools` + selector `0xae7e8d81` 拦截**已确认**的
   `updatePrices` 交易 → `parse_update_prices_calldata` 解出种子 →
   `apply_price_seed` 按本地金库余额重算整本 orderbook（读时纯函数，逐位一致）。
2. **`ElfomoTrade` 通道（金库余额，主）**：Router emit，topic 预筛 +
   `resolve_elfomo_targets`（按 `router_address` + 事件里的 `fromToken/toToken`
   判 pair）路由到对应 Pool → 有符号增量写入 `VaultDeltaLedger`，余额由账本派生，
   orderbook 随之自动重算。
3. **周期对账（兜底，45s）**：`start_elfomo_prop_sync_task`，默认间隔
   `DEFAULT_ELFOMO_RECONCILE_INTERVAL = 45s`（`with_elfomo_sync_interval` 可覆盖），
   锁外按目标块 `fetch_snapshot_at` → 锁内对 **current existing** `merge_snapshot`
   （`快照(S) + Σ_{块>S} 事件增量`，rebase 而非覆盖），覆盖断流/漏帧；
   失败退避上限 300s。AsyncUpdate / Resync 两条 pending 路径对 Elfomo 也走同一形状
   （`maintenance::execute_elfomo_snapshot_reconcile`），**不用块级水位做新鲜度闸门**。

#### 5.2.1 为什么 Pool 的 `updatePrices` 空事件被彻底移除

Pool 每块 emit 一条 `0xc5d08cbe…f809` 事件：**只有 1 个 topic、`data` 为 0 字节、
无 indexed 参数** —— 它携带零信息量，价格种子在**同一笔交易**的 calldata 里。
所以它既不可能独立兜底（同源同因），又会把"每块一次全量 RPC 重拉"常态化：

- `sync_events()` 现在只返回 `ELFOMO_TRADE_EVENT`；`build_query_chunks` 只注册
  `router_address`（不再注册 `pool_address`），get_logs 回填/订阅都不会再拉它；
- `sync()` 不再对该事件返回 `SyncAction::AsyncUpdate`。

移除后"提取通道静默失效"就没有任何自动信号了，因此补了两条**零 RPC 自证**：

- **种子覆盖率自证**：flashblocks 流里**块号跃迁**时（不依赖 slice `index` 语义，
  重连后从中间 slice 开始也成立）由 `StateSpace::observe_elfomo_seed_coverage`
  调 `ElfomoFiPropPool::observe_block`，按块收口"上一块有没有 raw-tx 种子"
  （同块多 slice 不重复计数；流重连整段跳过的块按缺失计）。连续
  `ELFOMO_SEED_COVERAGE_ALERT_BLOCKS = 5` 块无种子 → `error!` 一次，
  恢复后自动复位。**只告警，不自动重拉**（自动重拉会把提取侧的 bug 掩盖掉）。
- **模型自证**：每次拿到链上档位（init / 对账合并）时用 `build_orderbook` 逐位
  重算对拍；不一致 → `model_verified = false`，`simulate_swap*` /
  `calculate_price` / `has_sufficient_liquidity` 全部拒绝该池（宁可不报价，
  也不出错价），下一次对拍一致自动恢复。

#### 5.2.2 ladder 是 per-pool 链上数据：init 时逐池读取（接入新 pair 零代码）

链上 ladder 是 **per-asset 存储**，且 pool 合约直接暴露读接口：

```
IElfomoFiPool.getMetadata(address asset)
    → (uint8, uint8, uint8, uint8, uint16, uint128, uint16, uint16, uint8, address, uint256)
底层 = storage[keccak256(pad32(asset) ‖ pad32(0x04))]   // 一条 32 字节 word
```

存储 word 的字节布局（本模块只用前 4 项，其余字段语义未解、不参与报价）：

| 字段 | 位置（BE） | 含义 |
|---|---|---|
| `spread_penalty` | byte 5 | `F`：加宽时追加的偏离量 |
| `spread_level` | bytes 6..8（u16） | `SL`：加宽开关（与 vault 比较） |
| `band_count` | bytes 8..10（u16） | `T`：档位数量 |
| `unit` | bytes 10..26（u128） | `U`：档位带宽单位 |
| `decimals` | 返回值字段 1 | base asset 精度 |

**本地实装**：`ElfomoFiPropPool::init` 对 `[token_x, token_y]` 依次调
`getMetadata`，取第一个 valid（`U>0 && T>0`）的结果作为**本池自己的**
`ElfomoLadderConfig`；工厂不再登记/注入 ladder，`ElfomoFiPropFactory` 只传
池子基本信息（`token_x/token_y/pool/vault`）。读取失败 → 保持 invalid ladder →
本地报价为空 + `model_verified=false`（fail-closed），并打一条可检索 warn。

##### 档位生成规则（2026-09-11 全网格逐位实证，30,240 组 bit-exact）

设 `U=unit`、`T=band_count`、`SL=spread_level`、`F=spread_penalty`，两侧共用：

```text
PREFIX = profile.widths      // 前缀宽度表（逐档截断到容量）
D      = profile.deviations  // 逐档偏离（1e-5），dev_i = D[i]
尾档斜率：from→to = 50000，to→from = 150000；普通档斜率 100000 ∓ dev_i
```

**`PREFIX`/`D` 不是协议常量，而是池子的动态存储**（2026-09-11 二次实证）：

- 位置：`mapping(uint256 => uint256)` @ slot0，key = 本 pair 在
  `Pool.getSupportedPairs()` 中的下标（XLayer 单 pair = 0）→
  槽位 `keccak256(pad32(key)‖pad32(0))` = `0xad3228b6…5fb5`。
- 一条 word 的打包布局（分块实测解码 + 逐字段置位验证）：

  ```text
  byte31          = n（档数，本例 6）
  byte30          = widths[0]
  deviations[k]   = u16 little-endian @ byte (28 − 4k)   k = 0..n-1
  widths[k+1]     = u16 little-endian @ byte (26 − 4k)   k = 0..n-2
  ```

- 实测样本：
  - 块 `70352699` 及更早：`0x…00320064002800140019000a000f000a000a00050007000106`
    → `widths=[1,5,10,10,20,100]`、`deviations=[7,10,15,25,40,50]`（= 首轮逆向的快照）；
  - 块 `70352700` 起：`0x…003c0064002d00140023000a0019000a00140005000f000106`
    → `deviations=[15,20,25,35,45,60]`，宽度表不变。
- 改写来源：keeper 发 `0xd4ff31bd(uint256[] keys, uint256[] words)` 到 Pool
  （`keys[i]` = pair 下标，`words[i]` = 新 word）；**该交易同样只 emit 那条空事件**
  `0xc5d08cbe…`，信息全在 calldata。

因此 profile 与 `price_seed` 完全同构：`init` 逐池从链上读取（key 由
`getSupportedPairs()` 现算），运行期由 raw-tx `0xd4ff31bd`（零 RPC）+ 周期快照
（字段级水位 `profile_block`）持续保鲜；**代码里没有任何写死的宽度/偏离表**。

- **from→to**：容量 `C = (T−1)·U − vault_xeth`
  - `C > 0`：前缀按 `PREFIX` 逐档截断到 `C`（不足一档给残余档），尾部再补一档 `5T·U`；
  - `C == 0`（即 `vault_xeth ≥ (T−1)·U`）：链上只给**一档** `min(T·U − vault_xeth, 预算)`
    残余档，价恒为尾档价；`vault_xeth ≥ T·U` → 该方向不报价。
- **to→from**：容量即 `vault_xeth`；前缀容量 `vault_xeth − U`，尾部再补一档 `U`
  （`vault_xeth < U` 时前缀为空，只有尾档，size = vault_xeth）。
- **加宽**：`SL · U ≥ 本侧容量`（from→to 用 `C`、to→from 用 `vault_xeth`）时，
  该侧 `i≥1` 的档位 `dev_i += F`；**前缀只有 1 档时该档（`i==0`）也 `+F`**。
- **预算消耗**（from→to）：`rem -= ceil(size·price / 1e24) · 1e24`，`size` 受剩余
  `vault_usdt0` 与档宽共同封顶。

**接入新 pair 的步骤（零代码改动）**：部署配置里加
`ElfomoPairConfig{token_x, token_y, pool_address, vault_address}` 即可；ladder（含
profile word）由 `init` 自动从该 pool 的 `getSupportedPairs()` + `getMetadata` +
slot0 读取。keeper 改表由 raw-tx/快照实时同步；模型自证用**快照自带的 profile**
对拍（不能用可能已更新的本地 profile，否则会假阴性误停报价）。

##### 多链核查（2026-09-11 实测：Base / BSC）

- Router / Factory 在 Base、BSC 的**同地址都有部署**；Base 的 pool **同样暴露
  `getMetadata(address)`**、存储布局一致（WETH/USDC、cbBTC/USDC 均可正常解码出
  `unit/band_count/spread_level/spread_penalty`）。
- **差异 1（已修）**：XLayer 对非 base asset 返回**全零**，Base 的 pool 直接 **revert**
  （`0x672215de`）。`fetch_ladder` 因此改为**逐 asset 容错**：某个 asset 失败/非法就
  试下一个，不把第一个 asset 的错误当成整体失败（否则 `token_x` 恰是 quote 的 pair
  会让 init 整体失败）。
- **差异 2（未支持，已 fail-closed）**：Base 是**另一套 ladder 变体**。metadata word
  byte28（ABI 字段 `f3`）在 Base = 1；在 XLayer 上把该字节改成非 0 会让
  `getOrderbook` **revert**（改成别的值报 `not implemented`），说明它是实现版本/模式
  开关。实测 Base WETH/USDC 的偏离头为 **`[2,3,15,25,40,50]`**（XLayer 为
  `[7,10,15,25,40,50]`，仅前两项不同）——即 `D` **不是跨部署常量**。
- **差异 3（未支持）**：Base 的价格种子不满足 `slot1 >> 32`（槽位打包不同，待另行逆向）。
- 结论：本地模型在 Base 会**逐位对拍失败 → fail-closed 不报价**（不会出错价）。要真正
  支持 Base 需按差异 2/3 补一版变体实现（含自己的 `D` 与种子读取），当前 XLayer 路径
  不受影响。

#### 5.2.3 其它已知边界（不是故障）

- **价格种子只走 flashblocks**：如果 StateSpace 用非 flashblocks 的实时源
  （如纯 WS logs）跑 XLayer，则价格只由 45s 对账刷新（金库增量仍走日志实时）。
  生产按默认（flashblocks）配置不受影响。
- **`elfomo_pools` 集合在订阅建立时快照一次**（与 caliber 同形态）：运行期新增
  pool 需要重连订阅才会进入 raw-tx 提取；期间靠 45s 对账兜底。


### 5.3 模拟器设计（读时重算，与链上同构）

- 本地真状态 = `price_seed`（raw-tx 种子）+ `vault_usdt0` + `vault_xeth`
  （ElfomoTrade 双向记账 + 兜底快照 rebase）。`levels` 只是缓存。
- **没有"本地档位消耗"这一层状态**：orderbook 是 `(seed, vault)` 的读时纯函数，
  每笔成交只改金库余额，下一笔报价自动按新余额重建（`LevelConsumed` 已删除）。
- **每次 `simulate_swap` 都按当前 `(seed, vault)` 实时重算 orderbook**
  （`build_orderbook` 纯函数），再走四向 quote 精确公式（整数运算，含逐档
  截断、封顶），与链上每次读取实时 `balanceOf(vault)` 同构——不缓存档位递减。
- `simulate_swap_mut` 消费后按成交额递减金库；与 `ElfomoTrade` 事件账本对齐。

## 6. 逆向工作清单（状态）

| # | 任务 | 方法 | 状态 |
|---|---|---|---|
| R1 | 定位 per-pair pool 合约地址 | `pairKey = (a&mask)+(b&mask)+0x0146109eced2816f22a2937a116619ffffffffffff`（可交换，sum 溢出检查）；`slot = keccak256(pad32(pairKey)‖pad32(0x65))`（**0x65=101 十进制**，勿用 65）；`pool = address(storage[slot])`。已实测 `factory.storage[0x6dd7c5…] = 0x02dcdf4171939ac0fe28e48e8758649311e9459a`；trace 确认 factory `0x0a6e04cb`/`0x561fa97d` 直接 STATICCALL 该 pool | ✅ 完成 |
| R2 | quote 精确公式（含档位 size 语义、截断、封顶） | 见 §3.1。正向 13/13、反向 20/20 逐位对拍（固定块 `0x423c2b8`）；公式全在 Pool（`0x561fa97d` 返回 packed 5-word，Factory/Router 仅透传） | ✅ 完成 |
| R3 | 档位状态更新机制（每块谁写？） | 已实证：**每块 1 笔 `updatePrices(0xae7e8d81)`** 由 MM keeper `0x8121003e…5923` 发 Pool；calldata 参数即价格种子（`a = arg >> 32`，与 slot1 高 32 位一致）；orderbook 是 `(seed, vault 余额)` 读时纯函数（§3.1/§3.3）。raw-tx 本地直算已落地，零 RPC | ✅ 完成 |
| R4 | 反向 exact-out 公式与 limit 语义 | 见 §3.1。双向 exact-out 逐位对拍通过（含封顶/超容量返回 0 语义）；swap 内负 specifiedAmount 语义待编码后随 `ElfomoTrade` 事件对拍 | ✅ 完成 |
| R5 | 验证矩阵：固定块双向 × 多金额 × 封顶/边界 | `elfomo_prop` 单元测试 15/15 命中链上采样（锚点块 `0x423c2b8`，含小额/档界/封顶/超容量=0）+ orderbook 生成公式真实链 10 块全对 | ✅ 完成 |
| R6 | "空事件"是否可作独立状态源 | ❌ 否：仅 topic0、`data` 0 字节、零信息量，且与价格种子出自**同一笔交易**；已从 `sync_events`/query chunks 移除，改为覆盖率自证（R7） | ✅ 完成（2026-09-10 重构） |
| R7 | 主通道失效的自证方式 | 块边界收口（`observe_block`）+ 连续 5 块无种子 `error!`；**只告警不重拉**，收敛交给 45s 对账 | ✅ 完成 |
| R9 | ladder（宽度/偏离表）是不是全局常量 | ❌ 不是：链上按 asset 存于 `storage[keccak256(pad32(asset)‖pad32(0x04))]`，pool 暴露 `getMetadata(asset)`。已改为 **init 时逐池读取**（工厂不再登记 ladder），接入新 pair 零代码 | ✅ 完成（2026-09-11 重构） |
| R10 | ladder 生成规则是否完全逆向（含 `spread_level`） | ✅ `PREFIX/D` 常量 + 容量截断 + **vault 相关的加宽开关**；anvil fork 全网格 30,240 组 bit-exact，490 组固化 fixture + 单测零 RPC 回归 | ✅ 完成（2026-09-11） |
| R8 | 本地模型是否与链上脱节 | 每次拿到链上档位逐位对拍（`verify_model_against_chain`），不一致即全局拒绝报价（`model_verified=false`），一致自动恢复 | ✅ 完成 |

## 7. 已知事实速查（供编码/文档引用）

- 套利交易 `0x3a608dfefedf19731f01ba93945df8475fa9559eb40f5bae07334f991369e6f0`
  （块 `0x423b0c9` = 69447881，status=0x1）：ElfomoFi 段 xETH→USDT0，
  `0.12151322923155881` xETH → `300147468` USDT0（6dp）；Router emit ElfomoTrade
  （topic0 `0xbe65a3f1…`，data=[executor,receiver,fromToken,toToken,fromAmount,toAmount]，
  topics=[quoteId,partnerId]），无 pool 级日志。同块 updatePrices calldata 种子
  `0x143c4e5` + 父块金库（usdt0=19492562722、xeth=2818949271769303366）本地
  重算报价 == 300147468（回归测试已锚定）。
- 合约：Router `0xf0f0f0f0fb0d738452efd03a28e8be14c76d5f73`（报价/swap 入口）、
  Factory 代理 `0xffffffbb2d432b8acb4c57d556c0c721a431d038`（实现
  `0x406644607f87ecf0adc4c0c9c64705a9de1c5e31`）、Pool `0x02dcdf4171939ac0fe28e48e8758649311e9459a`
  （非代理）、Vault `0xbb1b19f138db3925883a96ff7a304277460e0c99`（Gnosis Safe，仅持币）。
- Pool 内 per-asset orderbook 存储：`storage[keccak256(pad32(asset)‖pad32(0x04))]`
  （xETH 槽 `0x6884…cd` 有值，USDT0 槽为 0——USDT0 侧由 vault 余额背书无档位）。
  已核实 trace slot 与值；MM 每块更新档位（价格漂移实测存在）。
- 报价更新：MM keeper `0x8121003eb12a97900d1e84097f864420a9a95923` 每块 1 笔
  `updatePrices(uint256)`（`0xae7e8d81`，**calldata 参数 = `(a<<32)|(ts-1)`，
  `a = arg>>32` 即价格种子**）发 Pool；Pool emit 空事件
  topic0 `0xc5d08cbe6fd3ebc24e5a483616dddbc63b2aff5c082c7d697603ab521079f809`
  并 SSTORE slot1 = `(a<<32)|ts`。orderbook = f(a, vault 余额) 读时纯函数。
- orderbook 生成：`a=slot1>>32`；`base=(100000+qs)×low`（qs 见 §3.1）；
  宽度/偏离由该 pool 的 `getMetadata` 参数现算（`PREFIX/D` + 容量截断 +
  vault 相关加宽，完整规则见 §5.2.2）。anvil fork 全网格 30,240 组 bit-exact
  （490 组固化为 fixture 单测），真链当前 head 逐位复核通过。
- ladder 读取：`Pool.getMetadata(asset)`（selector `0x2a50c146`），
  **每个 pool 在 init 阶段各取各的**；xtETH/USDT0 实测
  `[0, 18, 2, 0, 0, 0.6e18, 30, 5, 60, 0, 0]`。
- vault 余额读法：**`token.balanceOf(vault)`**（vault 是 Gnosis Safe，
  对 vault 合约调 `balanceOf` 会 revert）。
- 算术：pool 用 OZ `Math.mulDiv`（512 位），本地模拟必须全精度整数乘除。
- Factory selectors（反汇编）：`getOrderbook 0x561fa97d`（Router-only，5 参数
  `(from,to,0,Router,0)`，返回 packed）、`getOrderbook 0x0a6e04cb`（公开，返回
  标准 ABI 两个 `(size,price)[3]`）、`addPair 0xb6f3e087`、`swap 0x519341bb`、
  `getSupportedPairs 0xd527c998`（Router，返回 `(token0,token1)[]`，无 pool 地址）。
- 反汇编/采样临时产物：`/private/tmp/elfomo/`（router/factory/pool .hex/.dis、
  verify_rev3.py、r4_exactout.py 等；`/tmp` 可能被系统清空需重拉）。

## 8. 模块落地状态（2026-09-01）

- `src/amms/elfomo_prop/types.rs`：ABI（Router/Factory getOrderbook/vault balanceOf）+
  `OrderbookLevel`/`OrderbookSnapshot`（`LevelConsumed` 已于 2026-09-10 删除：
  读时重算模型下不存在本地档位消耗状态）。
- `src/amms/elfomo_prop/mod.rs`：`ElfomoFiPropPool`（AutomatedMarketMaker impl），
  四向 quote 纯函数（fwd/rev × exact-in/out）、`build_orderbook` 生成公式
  （种子+金库余额读时重算）、`parse_update_prices_calldata`（raw-tx 解种子）、
  `apply_price_seed`（本地直算）、L2 `fetch_orderbook_snapshot`
  （getOrderbook + slot1 种子 + `token.balanceOf(vault)`）、
  单元测试 16/16 + factory 2/2（链上逐位对拍数据内嵌，含真实套利交易账本回归锚点
  `test_real_arb_tx_ledger_replay`）。
- fork 对拍测试（`tests/elfomo_prop/xlayer_fork_test.rs`）：Phase 1 双向 quote
  27/27、Phase 2 orderbook 9 块逐位、Phase 3a exact-in、Phase 3b exact-out
  （网格从本地 orderbook 动态派生）、Phase 4 flashblocks 历史回放种子直算
  （±1 wei 容忍）；长跑 `ws_live_verify`（`#[ignore]`，env 门控）验证
  raw-tx → 本地直算 → 模拟全链路。
- `src/amms/elfomo_prop/factory.rs`：`ElfomoFiPropFactory`（多 pair 独立 pool，
  参照 caliber_prop：`ElfomoPairConfig{token_x, token_y, pool_address,
  vault_address}` 部署配置传入，`new(pairs,…)`/`new_default`，`discover()` 遍历
  pairs 返回多池骨架），已注册 `AMM`/`Factory` 枚举与 `Variant::init_batch`。
- 多 pool 口径：`ElfomoFiPropPool` 持有 `token_x/token_y`（pair 定义），
  `skeleton()`/`Default` 带参；模块内 token 判断、报价、orderbook 快照、
  swap 模拟全部按 pair 字段而非硬编码 xETH/USDT0。
- 实时驱动（已落地，关键路径零 RPC）：
  - **L3 主通道**：`xlayer_flashblocks.rs` `elfomo_pools` 集合 + 按 selector
    `0xae7e8d81` 拦截已确认 `updatePrices` raw-tx → `ElfomoTxEvent{pool, seed,
    tx_index}` → `StateSpace::apply_elfomo_updates` → `apply_price_seed` 本地
    重算 orderbook；同块空 data 更新事件在提取侧过滤（不触发 AsyncUpdate）。
  - `ElfomoFiPropPool::sync()` **只处理** `ElfomoTrade` → 金库双向记账 + 缓存重算；
    Pool `updatePrices` 空事件已从 `sync_events` 与 query chunks 移除，
    `sync()` 对它返回 `SyncAction::None`；`last_synced_block` 单调不回退。
  - `build_query_chunks` 只注册 `router_address`（ElfomoTrade）；Pool 地址不再订阅。
  - `resolve_elfomo_targets` 按 `router_address` + 事件 `fromToken/toToken` 判 pair
    （同 Router 多 pair 不串池）；命中已知 Router 即终结分发链。
  - `sync_services::start_elfomo_prop_sync_task`（周期对账，默认 45s，
    `with_elfomo_sync_interval` 可覆盖）；`maintenance` 的 AsyncUpdate / Resync
    对 Elfomo 走专用 `execute_elfomo_snapshot_reconcile`（锁外 fetch → 锁内对
    current existing `merge_snapshot`），不再被块级水位闸门饿死，
    `SkippedStale` 日志降为 `debug`。
  - 覆盖率自证：`StateSpace::observe_elfomo_seed_coverage`（flashblocks 块边界）
    → `ElfomoFiPropPool::observe_block`。
  - `init()` 的 decimals **走链上 `IERC20Metadata::decimals()`**（读失败回退已知
    pair 常量）；`calculate_price` 的缩放按 `10^(24 + dec_y − dec_x)` 推导
    （不再硬编码 `/1e12`）。
  - 模型自证：`verify_model_against_chain`（逐位对拍 → `model_verified`），
    未通过时 `simulate_swap*` / `calculate_price` 返回 Err、
    `has_sufficient_liquidity() == false`。
- **ladder 通用化（2026-09-11 重构）**：`ElfomoLadderConfig` 只保留
  `unit/band_count/spread_level/spread_penalty`（旧的深度表/斜率表常量删除）；
  `ElfomoFiPropPool::fetch_ladder` 在 `init` 时对 `[token_x, token_y]` 调
  `getMetadata` 逐池取参；`build_orderbook_with` 是唯一的生成实现（含 vault 相关
  加宽）；`factory.with_ladder/ladders` 已删除。回归锚点：
  `src/amms/elfomo_prop/fixtures/ladder_grid.json`（链上实测 490 组）+
  单测 `test_chain_ladder_grid_bit_exact`。
- 账本语义（已确认）：`ElfomoTrade` 事件 data 的 fromAmount/toAmount 是**实际
  成交额**，与 router `swap(int256 specifiedAmount)` 的符号无关（负值=exact-out、
  正值=exact-in，两种模式事件均携带实际 input/output）；本地按事件实际金额
  处理金库余额即可。真实套利交易 `0x3a608dfe…`（块 69447881）已回归锚定：
  同块 updatePrices 种子 + 父块金库余额 → 本地报价 == 事件 toAmount
  （xETH→USDT0，121513229231558820 → 300147468）。

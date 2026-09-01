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
- **from→to 档位**（size=输入量）：深度 `DEPTH1=[0.6e18, 3e18, 6e18,
  4859537498999137814, 9e19]`，斜率 `[99993, 99990, 99985, 99975, 50000]`；
  `rem = vault_usdt0 × 1e24`，逐档 `cap = rem // price`、`s = min(DEPTH1[i], cap)`，
  `rem -= ceil(s×price/1e24) × 1e24`，`s < DEPTH1[i]` 即停（余量档），
  最后一档吃完全部剩余。
- **to→from 档位**（size=输出量）：深度 `DEPTH2=[0.6e18, 3e18, 6e18, 6e18,
  12e18, 60e18, 0.6e18]`；`rem = vault_xeth`，逐档 `cap = rem - 0.6e18`、
  `s = min(DEPTH2[i], cap)`，`s < DEPTH2[i]` 即停；**尾部 0.6e18 恒显示**
  （`s = min(0.6e18, rem)`）。
  斜率按档位数：`n=1 → [150000]`；`n=2 → [100067, 150000]`；
  `n=3 → [100007, (s₂≤1.8e18 ? 100070 : 100010), 150000]`；
  `n≥4 → [100007, 100010, 100015, 100025, 100040, 100050]` 依次 +5，尾部 150000
  （n≥4 无小 size 加宽异常，已 anvil 扫描 30+ vault 值验证）。
- 对拍结果：真实链 10 个块（含最新）fromTo/toFrom **全对**；anvil vault
  余额 0.5e18~200e18 全量扫描 size/斜率全部匹配。

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
  mod.rs      // ElfomoFiPropAmm：AutoMakerMaker impl、quote 公式、simulate_swap
  types.rs    // OrderbookLevel/OrderbookState/SyncState
  factory.rs  // DiscoverySync：getSupportedPairs 列池 + getOrderbook 快照
```

### 5.2 同步策略（三层：raw-tx 本地直算为主，RPC 仅兜底）

报价更新机制（§3.3）决定主通道是 **raw-tx 解析 → 本地重算** 的块级实时，
RPC 只在"实在没有办法"时兜底：

1. **L3 — flashblocks 原始交易流（主通道，零 RPC）**：`xlayer_flashblocks`
   流按 `to ∈ elfomo_pools` 且 selector `0xae7e8d81` 拦截已确认的
   `updatePrices` 交易，`ElfomoFiPropPool::parse_update_prices_calldata`
   解出种子 `a` → `apply_price_seed` 按**本地金库余额**重算整本 orderbook
   （读时纯函数，逐位一致）；同块该交易 emit 的空 data 事件在提取侧被过滤
   （避免冗余 AsyncUpdate）。`ElfomoTrade`（Router，topic 预筛）驱动金库余额
   递减（`vault_usdt0 -= toAmount` / `vault_xeth -= toAmount`），orderbook
   随余额自动缩放。**关键路径完全本地，无 RPC。**
2. **L1 — 事件通道（无 raw-tx 时的回退）**：Pool `updatePrices` 空事件
   （topic `0xc5d08cbe…`，query chunk 注册 `pool_address`）本身不含种子 →
   `ElfomoFiPropPool::sync()` 返回 `SyncAction::AsyncUpdate` → pending worker
   调 `update()` 重拉 `getOrderbook` + slot1 种子 + `token.balanceOf(vault)`
   真值。仅 flashblocks 断流/漏提取时触发；gap catch-up（NewHeadsPull）也走此路。
3. **L2 — 周期快照（最后兜底）**：`start_elfomo_prop_sync_task`（可配
   `with_elfomo_sync_interval`，默认回退 `non_event_sync_interval`）低频重拉
   整档回正 + 种子 + vault 余额，覆盖极端断流/漏块；失败退避上限 300s。

事件注册（`build_query_chunks`）：ElfomoTrade 由 **Router** emit、
updatePrices 事件由 **Pool** emit，两者地址都必须注册进 topic chunk
（默认分支只注册 `amm.address()`=pool_address，Router 事件会漏收）。

### 5.3 模拟器设计（读时重算，与链上同构）

- 本地真状态 = `price_seed`（raw-tx 种子）+ `vault_usdt0` + `vault_xeth`
  （ElfomoTrade 递减 + 兜底快照）。`levels` 只是缓存。
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
  fromTo 斜率 `[99993,99990,99985,99975,50000]`、深度
  `[0.6e18,3e18,6e18,4859537498999137814,9e19]`；toFrom 深度
  `[0.6e18,3e18,6e18,6e18,12e18,60e18,0.6e18]`（尾部恒显 0.6e18），斜率按
  档位数分档（§3.1）。真实链 10 块 + anvil 全量扫描验证。
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
  `OrderbookLevel`/`OrderbookSnapshot`/`LevelConsumed`。
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
  - `ElfomoFiPropPool::sync()` 命中 Pool `updatePrices` 空事件（`0xc5d08cbe…`）
    → `SyncAction::AsyncUpdate`（L1 回退，仅无 raw-tx 时）；`ElfomoTrade` →
    金库余额递减 + 缓存重算；`last_synced_block` 单调不回退。
  - `build_query_chunks` 注册 `pool_address`（update 事件）+ `router_address`
    （ElfomoTrade 事件）。
  - `sync_services::start_elfomo_prop_sync_task`（L2 周期兜底，可配
    `with_elfomo_sync_interval`）。
- 账本语义（已确认）：`ElfomoTrade` 事件 data 的 fromAmount/toAmount 是**实际
  成交额**，与 router `swap(int256 specifiedAmount)` 的符号无关（负值=exact-out、
  正值=exact-in，两种模式事件均携带实际 input/output）；本地按事件实际金额
  处理金库余额即可。真实套利交易 `0x3a608dfe…`（块 69447881）已回归锚定：
  同块 updatePrices 种子 + 父块金库余额 → 本地报价 == 事件 toAmount
  （xETH→USDT0，121513229231558820 → 300147468）。

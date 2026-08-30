# BinaryFi propAMM 内部逻辑逆向文档（XLayer）

> 目标：精确复刻 XLayer 上 BinaryFi propAMM 的链上 `quote()`/引擎报价，
> 使本地 `engine_quote`/`simulate_swap` 与链上**逐位一致**。
>
> 方法：引擎字节码反汇编 + `debug_traceCall` structLogs 符号分析 +
> 链上多资产多金额采样对拍（详见 §11 工具链）。
>
> 验证基线（新引擎锚点块 `0x402f480` = 67302528，asset2 + asset6 双资产逐位对拍；
> fork 测试锚点 67302485：132/132 线性 quote、22/22 大额 probe 一致）。

## 1. 合约架构与地址

| 角色 | 地址 | 说明 |
|---|---|---|
| 池子 / quote / Swap | `0x4a8e34cfe4f643132e8de0e9752054a9ac862816` | = **PAmm1010Router**（官方 Router，quote recipient 即池子自身；2026-08-19 迁移） |
| 引擎 | `0x6558dbe4c1bb50ed881e54105242491d03a98118` | `update(...)` 交易提交带签名价格；emit Update 事件（data 为空；2026-08-19 迁移） |
| 金库 | `0x9b169052Ee1569Ec5bDF51DbF48D2962526cF6D9` | 持有 12 种资产（USDT0/xSOL/xETH/CRCLx/DOG/NVDAx/…） |
| 旧默认 Router（已废弃） | `0xa1945aa291a99ea996b9c41cc645e30c1c01d190` | 仅作历史参考；生产用 poolindex 配置（= 池子地址） |

关键事件：
- Swap：`0xcd3829a3813dc3cdd188fd3d01dcf3268c16be2fdd2dd21d0665418816e46062`
- Update：`0xaf186e2e77ac28f0c051cdd1e2b3b92924e34b314650186bbc14742e373751c8`（topic1 = asset index，data 为空）

## 2. 引擎 update calldata 布局

选择器 `0x024b94f6`，从 calldata 第 5 字节起的定长布局（与链上真实交易逐字对齐）：

```
index(32) | offset(32) | blockNumber(32) | price(32) | a(32) | b(32)
data0(32) | data1(32) | data2(32) | data_len(32) | sig_len(32) | sig(65)
```

- `price`：USDT0 计价定点价（2 位小数，内部价格 = price × scale/10000；asset[2] scale=100000，其余 10000）
- `a` / `b`：ask / sell 方向点差原始字段（透传，不直接使用）
- `data0` = **sellLadder**，`data1` = **buyLadder**（左对齐 256 位多档字段，见 §3），`data2` 透传
- 点差偏移：`askOffsetRaw = (data1 >> 240) / 16`、`bidOffsetRaw = (data0 >> 240) / 16`
  （**是除以 16，不是 `& 0xfff`**；`& 0xfff` 的结论已被链上数据推翻）
- 尾部为 EIP-191 签名（65 字节），引擎按签名校验调用者

flashblocks 实时流解析：`enrich_update_log_data` 通过 `keccak256(raw) == tx_hash`
定位原始交易，RLP 解码后解析上述布局，把 **7 个 word** 注入日志 data：

```
price | blockNumber | data0 | data1 | data2 | askOffsetRaw | bidOffsetRaw
```

`sync()` 从日志 data 的 offset 64..128 读 data0/data1，调用 `apply_l2_update_full`
一次到位（价格 + 点差 + 多档阶梯），**零 RPC**。

## 3. ladder 编码（data0 / data1）

- 每档 **24 bit**（12 bit weight + 12 bit qty），从高位左对齐，最多 **10 档**；
  全零档为终止符
- 前 16 位（`data >> 240`）是点差偏移字段（ladder 空间单位）
- weight 需按 `scale/10000` 折算为内部价格单位（asset[2]=100000 → ×10，其余 ×1）
- qty = 引擎储备 R 的倍数（每档容量 = qty × R）

真实样例（asset6 update @ block `0x402fdb5`，scale=10000 不折算）：
```
data0 = 0x01808801c08801d088094ba8... → sell ladder [(24,136),(28,136),(29,136),(148,2984)]
data1 = 0x01806001a08509f2c6...       → buy  ladder [(24,96),(26,133),(159,710)]
```

asset2 sell ladder calldata：`0x0c91f10e93c800...` → `[(201,241),(233,968)]`
（scale=100000 → weight 实际 ×10：2010/2330）。

## 4. SELL 报价（i → USDT0）——多档阶梯

链上逐位验证的精确公式（ladder + 引擎储备 R 已知时）：

```
rem = in − in×fee_ppm/1e6              // 按账户费率输入侧先扣（整数除法）
out = Σ_k (price − w_k) × min(rem, qty_k × R) × 10^(d0−2) / 10^di
rem -= min(rem, qty_k × R)              // 逐档递减
```

- 首档 weight = 小额报价偏移（w=0 时退化为单档线性 `price×rem×10^(d0−2)/10^di`）
- 每档输出 = `(price − w) × consume × 10^(d0−2) / 10^di`，EVM 向零截断
- 输出超 USDT0 金库余额 → **归零**（`sell_zero_over_vault`，链上实测）
- ladder / R 未知 → 回退单档线性：`out = in × raw × 10^(d0−2) / 10^di`，
  `raw = price − sellOff`（无费），受 `maxIn` 截断

## 5. BUY 报价（USDT0 → j）

```
ask = price + askOff        // askOff = askOffsetRaw × scale/10000
rem  = in − in×fee_ppm/1e6  // 按账户费率输入侧先扣（整数除法，与 SELL 同款）
q0j  = floor(10^(dj+2) / ask)                         // 小额报价状态 in=10^d0（无费）
linear = floor(rem × 10^(dj+2) / (ask × 10^d0))
```

- **必须先扣费再线性报价**，不能 `in × (1e6−fee_ppm)/1e6` 替代（非整倍数输入有
  ~1e-4% 偏差；tx1/tx2 失败交易逐位对拍，fee=1000：in=44,291,018 →
  324,080,619,644,034,278、in=122,156,425 → 891,476,871,940,974,505；
  fee=200 实测 asset4~9 BUY 全 -200.0ppm 逐位一致）
- 输出再按阶梯上限截断：
  - 饱和型（阶梯容量 ≤ 金库余额）：`out = min(linear, maxOut)`
  - 超阈值归零型（阶梯容量 > 金库余额）：`linear > 金库余额 → 0`，否则原样
- 饱和输出 = 阶梯全容量输出，**与 fee 无关**（fee 只影响剩余输入，不影响
  满档 consume；块 0x40832F5 fee=200 实测 asset8 大额 router==agg 平顶相等）

## 6. 跨资产（i → j，均非 USDT0）两段式

```
v   = floor(in × raw_i × 10^(d0−2) / 10^di)            // 第一段 SELL（含 maxIn 截断 + 归零）
out = floor(v × 10^(dj−d0+2) / ask_j)                  // 第二段 BUY，无额外费率因子（实测）
```

第二段不再扣费是关键差异（费已由第一段 SELL 输入侧扣一次；直接 `0→j` 才扣一次），
本地 `engine_quote` 与引擎一致。

## 7. 引擎储备 R 与 maxIn / maxOut

- 引擎储备 R 来自**引擎存储槽**（asset6 = 29.4e15、asset2 = 20000），
  长期稳定、**不随 Swap 变化**；可用 `getAssetConfig` 读取，或从快照大额
  probe + ladder 反推
- `maxIn_i`（SELL 输入上限）= `ladderWeight_sell × engineReserve`（= Σ qty×R）；
  快照用 100 整枚 probe 精确恢复（候选 R 对拍 + 精确 maxInput）
- `maxOut_j`（BUY 输出上限）= 阶梯总容量 `Σ qty×R`（链上实测 asset2 @ 67430640：
  buy_ladders `[(1960,767),(1960,1150),(1960,1183)]`、R=20000 → in≥5e10 平顶
  **62,000,000 = Σqty×R**，逐位一致）或金库余额；快照由大额 probe 观测，
  update 路径由 `buy_ladder_remaining = Σ qty×R` 精确推导，并随 Swap 事件消费递减
- 多档阶梯资产（如 DOG）100 整枚 probe 与单档线性不兼容 → 本地仅小额区
  可精确复刻；生产 update 路径直接携带精确 ladder，无此问题
- **R 反推增强（1.5b）**：100 整枚 probe 未饱和（总容量 > 输入）时闭式公式
  失效，改单调二分求解 —— `out(R)` 关于 R 单调不减（每档
  `consume = min(rem_k, q_k×R)`），下界取闭式饱和解，上界倍增到
  `out(R) ≥ q_big` 后二分最小命中 R（与 probe 逐位一致即采用）。
  多档资产不再回退单档线性（避免高估）。

## 7.5 BUY 实时金库零门槛（P0 修复）

- `buy_capped` 对**封顶后**输出做金库零门槛：`min(linear, maxOut) > 金库余额 → 0`。
  链上实测锚点：饱和型且 `maxOut ≤ 金库` 时 `linear > 金库` 仍返回 `maxOut`
  （5 资产大额 probe 对拍）；金库被 Swap 抽干后（NVDAx 金库≈1.07e12 近空）
  `maxOut=1.301e18 > 金库` → 链上 quote 恒 0，本地必须同样归零（防幻影利润）。
- 归零型（`buy_zero_over_vault`）保持 `linear > 金库 → 0` 不变；金库未知时
  不门控（与 `capped_out`"余额未知不截断"一致）。

## 7.6 非单调阶梯退化检测（P1 修复）

- 快照新增 (0→j) **中额 probe**（`10^(d0+3)`，1000x 小额，编码 `3n²+j`，
  与大额共用 `bigQuotePairs` 列表）：mid 仍线性但 big 输出 < mid 输出
  → 曲线非单调回落（NVDAx 实测：1e9→4.456e18 线性、≥5e9 骤降平顶 1.301e18），
  big 落在退化平顶区，**不是全输入范围的有效 maxOut**；此时清掉 maxOut，
  线性区恢复正确报价（in=867,053,194：本地 1.301e18 → 链上 3.863e18，低估 66%），
  超大额由 7.5 金库零门槛兜底。

## 7.7 报价时效窗口（链上实测，P2 修复）

- binaryFI 池子链上 `quote()` **自带块号时效**（非 Caliber 的 deadline64+window
  墙钟机制，也不依赖本地时钟）：每资产以**最后一次引擎 update 的块号**为基准，
  时效窗口 **5 块**；`当前块 − 最后update块 ≤ 5` 正常报价，差 **≥ 6 块时
  `quote()` 直接返回 0**（不是 revert；引擎内部 `0x6ee50667` 路径 revert
  `0x86fa3e43`，外层 quote 捕获转 0）。
- 引擎 per-asset `lastUpdateBlock` 存储位置：`keccak256(abi.encode(assetId, 9))`
  槽 +0（另：+1 打包 scale/decimals/address，+3 打包 price，+4 sellLadder，
  +5 buyLadder）。**每次 update 交易都会写 lastUpdateBlock**——即使价格/ladder
  与上次相同（实测 67430645 与 67430647 两笔内容完全相同的 NVDAx update 都
  刷新了时效）。
- 逐块实测（直接对链上历史块 eth_call，块 67430638–67430652）：
  - NVDAx（asset8）：lastUpdate=67430638 → 67430640–43 正常（差 2–5），
    **67430644（差 6）起返回 0**；67430645 被 update → 恢复，67430647 再 update
  - SPYx（asset6）：lastUpdate=67430639 → 67430640–44 正常（差 1–5），
    **67430645（差 6）起返回 0**；67430647 被 update → 恢复
  - asset2：lastUpdate=67430640 → **67430642 → 67430643** → 67430648（每 1–3 块
    就被 MM 重新 update，5 块窗口从不过期，67430640–52 全程正常报价）
  - asset3：lastUpdate=67430640 → 67430644 → 67430647 → 67430648（同样从不过期）
- **“asset2/3 差 8 块仍新鲜”不是异常**：67430648 时 asset2 的真实 lastUpdate 已是
  67430643（差 5）、asset3 已是 67430647（差 1），窗口内自然新鲜；之前把它当
  时效异常是把 67430640 误当最后一次 update，忽略了中间的 update 事件。
- 本地判定用模块自身数据：`price_updated_block[asset]`（L2 flashblocks 增强注入
  的 calldata blockNumber、L3 canonical 用**事件块号** = 引擎最后一次 update 该
  资产的块）与 `last_synced_block`；`last_synced_block − price_updated_block > 5`
  → 过期。
- 门控位置：`engine_quote`（覆盖 `simulate_swap`/`simulate_swap_mut`）、
  `max_achievable_out`（覆盖 `simulate_swap_exact_out`，过期直接拒绝）、
  `calculate_price`（过期 spot = 0.0，prefilter 放弃路径）。过期返回 0，
  **不触发 AsyncUpdate**（AsyncUpdate 仍是 L3 数据源补缺机制，与时效无关）。
- 边界：`price_updated_block == 0`（快照/锚定路径，无 update 日志佐证）不判过期，
  避免快照初始化池被误杀；快照**不**写 `price_updated_block`（周期快照不能把
  过期资产"保鲜"——链上只由引擎 update 推进时效）。L3 canonical 路径无 raw
  bytes 时**仍用事件块号推进时效时钟**（与链上每次 update 写 lastUpdateBlock
  一致），价格本体由 AsyncUpdate 快照补缺。

## 7.8 BUY 阶梯容量进报价路径（P2 修复）

- 新增 `buy_ladder_remaining[asset]`（BUY 剩余容量 = `Σ qty×R`）：
  update 路径由 `apply_l2_update_full` 从 buy_ladders × R 精确重置；快照路径在
  饱和型大额 probe 处覆盖为观测 maxOut（容量以快照为权威）；归零型清空
  （金库零门槛接管）；退化型/未截断保留 update 推导值（精确容量，asset2 实测
  大额 probe 未饱和但 Σqty×R 即链上平顶）。
- Swap 事件（`anchor_rate`）：0→j 与跨资产第二段按 `amount_out` 消费 j 的剩余
  容量，直到下一次 update/快照重置——容量变化**通过交易实时更新**，不再等快照。
- `buy_capped`/`max_achievable_out`/`ladder_cap_known` 封顶优先取
  `buy_ladder_remaining`，其次快照 `max_outputs`，再叠加 7.5 金库零门槛。

## 7.9 按账户费率（fee_ppm）参数化（P0 修复）

根因：引擎费率是 **per-account storage**（`getFee(account)` = `0xb88c9148`，
聚合器白名单 0 费率），本地此前硬编码 999/1000 报价——fee 实际为 200 时
本地每个 quote 高估 ~0.08%（0.1%−0.02%），是 08-09 两笔失败套利交易的
直接原因（tx 0x93abd8… / 0x9216f8…，fee=1000 时逐位一致）。

- 费率时间线（ppm）：锚点块 `0x402f480` = 500 → 失败交易窗口
  `0x405B7E9`/`0x4062DAE` = 1000 → `0x4073356`（2026-08-10 16:16 UTC，
  `setFee` 由 `0x2db200f40f47…` 调用）起 = 200。默认兜底 `BINARYFI_DEFAULT_FEE_PPM` = 1000。
- 公式（块 `0x40832F5`，fee=200 实测）：BUY `rem = in − in×200/1e6` 后线性
  报价，asset4~9 全 -200.0ppm 逐位一致（asset9 `7,324,397,568,300,007 →
  7,322,932,688,786,347`）；SELL XETH `1,875,910 → 1,875,534`、CRCLx
  `66,990 → 66,976`。**输入侧整数除法扣费**，不能用 `in×(1e6−fee)/1e6` 替代
  （非整倍数输入 ±1 舍入，实测 8 采样仅输入侧口径全一致）。
- 饱和输出与 fee 无关（fee 只减剩余输入、不影响满档 consume）：块 0x40832F5
  asset8 大额 router==agg 平顶相等；`buy_capped` 直接 `min(fee'd linear, 观测
  饱和值)`，无需再折算。
- 本地获取/同步：
  1. 批量合约 `GetBinaryFiPropStateBatchRequest` 新增 `getFee(recipient)` 字段
     （`Snapshot.fee`，非 0 才覆盖本地），快照报价反推统一无费口径，与 L2
     calldata 无费价格一致；**bid/ask 用含费 quote 带费率直接反推**
     （`recover_ask_eff`/`recover_ask_big` 带 `fee_ppm`，bid =
     `out×10^dj/(fee_rem(10^(dj−4))×10^(d0−2))`），不走 `unfee_quote` 二次
     舍入——NVDAx 实测 unfee 路径 ask 偏差 1 → 输出差 0.004%；
  2. `FeeUpdated` 事件（topic0 `0xc58b3024a07432cfc160ea128eebc11329d444bb61bf7098f09bb6567b943c66`，
     data 前 32 字节 = 新 fee ppm）实时同步：`sync()` L0 分支更新 `fee_ppm`，
     并**同步重导全部 rates**（spot/预过滤立即对齐，不依赖异步窗口）；实际
     变更时返回 `AsyncUpdate` 触发一次快照重锚（含费 maxOut/大额 probe 需按
     新费率重新观测）。事件路由依赖 `state_space::resolve_binaryfi_targets`
     新增 `BINARYFI_FEE_EVENT` 分支（单 topic 事件走 `topics.len()==1` 分发
     通道），同一 engine 的全部虚拟子池一起更新。
- 存量状态兼容：`fee_ppm` serde 兜底 1000，旧序列化状态行为与历史一致。

## 8. 三层数据同步（事件驱动，无轮询）

| 层 | 来源 | 处理 |
|---|---|---|
| L1 | Swap 事件 | **部署级共享金库账本**：`reserves[in]+=in`、`reserves[out]-=out`、
  `buy_ladder_remaining[out]-=out`（任何 Swap 都动共享 vault，跨 pair 也全局
  可见）；费率/价格锚定仍限本 exposed pair（防跨 pair 污染价格） |
| L2 | flashblocks raw tx 增强 | 解析 `0x024b94f6` calldata → 注入 7 word → `apply_l2_update_full`，零 RPC |
| L3 | 无 raw bytes 的 update 日志 | 标记 stale → `AsyncUpdate` → 批量静态调用拉取 quote 恢复 bid/ask |

周期性快照任务（`binaryfi_sync_interval_secs`，默认 15s）只做**容量/上限观测**，
不做价格覆盖（价格以日志为准，快照补缺 + 保鲜判断防止旧块 quote 覆盖新价格）。
快照额外读取 `vault.balanceOf(asset)`（批量合约新增 `vaultBalances` 字段）并
用它**重锚真实金库余额**（校正非 Swap 途径的金库漂移），余额解析优先级：
`vaultBalances`（真实 ERC20）→ `vaultReserves`（引擎记账）→ `poolBalances`。

### 共享金库（2026-08-19 抽干事故修复）

单 BinaryFi 部署是共享金库：所有虚拟子池的 `reserves` 都代表同一个 vault 的
各资产余额。L1 事件路由与处理必须按部署级口径：

- **路由**：Swap 命中"exposed pair 含任一交易资产"的实例（由"双 token 都在
  pair"放宽为"至少一个 token 在 pair"）。生产 11 个实例均为 `(USDT0, X)`，
  每笔 swap 必触碰 USDT0 → 命中全部 11 实例（USDT0 全局共享，正确）。
- **处理**：命中实例统一 `reserves[in]+=in`、`reserves[out]-=out`、
  `buy_ladder_remaining[out]-=out`；**费率锚定仍限本 pair**，跨 pair swap 只
  更新金库、不动价格，防跨 pair 污染。
- **门控**：`buy_capped`/`sell_zero_over_vault` 金库门控按真实 vault 口径
  （漂移场景比引擎更保守 = 保护方向；无漂移时与链上逐位一致）。

修复前：跨 pair 的 SELL 抽干 USDT0 金库（1.349B→280.7M→4.2M）对本实例
不可见 → 本地金库门控放行必然失败的交易（`transferFrom(vault)` 余额不足
revert，见 `dex-arbitrage/docs/2026-08-19_binaryfi_shared_vault_drain_forensics.md`）。

## 9. 链上实测验证数据（SELL）

asset6 @ 探测块 67302528（price=76870、R=29.4e15）：
```
in=1e14 → 76,869
in=2e19 → 15,363,475,896
in=3e19 → 18,863,967,276（封顶）
```

asset6 @ 0x402fdb5 update（price=76925、ladder 见 §3、R=29.4e15）本地复刻：
```
in=1e14 → 76,862；in=1e18 → 768,625,495；in=5e18 → 3,843,087,511
in=1e19 → 7,685,995,104；in=1.8e19 → 13,827,464,262（跨 4 档）
```

SELL 归零样例（xETH，金库 17,347,345,227）：`in=9.13e18 → 17,343,984,269`、
`in=9.14e18 → 0`。

## 10. 已知限制与坑

- **快照路径无阶梯信息** → 单档线性近似（SKHYx 大额 SELL：sim=140,520,000 vs
  chain=140,529,700，相对差 ~7e-5）；update 日志路径精确，生产以日志驱动
- **xETH BUY 小额**：链上 526,124,634,901,868 vs 公式派生 526,124,627,580,616
  （相对 1.4e-5，仅 xETH 一个资产）；快照 q0j 直接取链上值不受影响
- 点差偏移首档 = `(data >> 240) / 16`，**不是 `& 0xfff`**（易错点）
- `rem = in − in/1000`，in<1000 时与 `in×999/1000` 不同（易错点）
- 买入被禁用资产（0→j quote 恒 0）仍可能收到 update 日志 → 只更新价格，费率保持 0
- 共享金库按部署分组：L1 必须把**所有** Swap（含跨 pair）应用到全部命中实例的
  `reserves`/`buy_ladder_remaining`，价格锚定除外（仍限本 pair）。跳过跨 pair
  更新会导致金库漂移 → 本地报价高估 → 链上 `transferFrom(vault)` 余额不足
  revert（2026-08-19 事故根因）
- 周期快照不能覆盖日志驱动的更新价格（保鲜判断：日志优先、快照补缺）
- **时效只在 L2 日志路径生效**：canonical/L3 路径无 raw bytes → 无 update 块号，
  按"未知不过期"兜底；快照路径的 `price_updated_block` 恒为 0，不会把过期资产
  "保鲜"（链上只由引擎 update 推进时效）
- **NVDAx BUY 小额 ~1e-5 舍入**（块 67430640：sim=4,455,688,302,425,106 vs
  chain=4,455,644,262,075,732，相对 ~1e-5，与 xETH 同类）；快照 ask 恢复的
  已知精度限制，Phase 7 对拍按容差断言，归零端精确

## 11. 逆向工具链记录

- RPC：生产 `https://rpc.xlayer.tech`；tenderly `https://xlayer.gateway.tenderly.co/<key>`
  （`debug_traceCall` 第三参传 `{}`，**勿传 tracer**；内建 structLogs tracer 返回
  `ReferenceError: structLogs is not defined`，自定义 JS tracer 也不支持）
- Alchemy `https://xlayer-mainnet.g.alchemy.com/v2/<key>`：支持 structLogs
- 关键 trace 文件：`/tmp/sell_a6_1e19.json`、`/tmp/sell_a6_2e19.json`、
  `/tmp/sell_a6_5e19.json`、`/tmp/a2_quote_10000.json`
- 反汇编脚本：`/tmp/evm_dis.py`（指向引擎字节码 `/tmp/engine_code_noprefix.hex`，
  注意不要误指向 pool code）

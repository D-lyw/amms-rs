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
| 池子 / quote / Swap | `0x2d651e3fe9470db52d211569a0ab7266c5180de7` | = **PAmm1010Router**（官方 Router，quote recipient 即池子自身） |
| 引擎 | `0xeacf260a16a4e16a758fc1bd126d49d8e02f9996` | `update(...)` 交易提交带签名价格；emit Update 事件（data 为空） |
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
rem = in − in/2000                      // 费率因子先扣（in<2000 时 ≠ in×1999/2000，实测）
out = Σ_k (price − w_k) × min(rem, qty_k × R) × 10^(d0−2) / 10^di
rem -= min(rem, qty_k × R)              // 逐档递减
```

- 首档 weight = 小额报价偏移（w=0 时退化为单档线性 `price×rem×10^(d0−2)/10^di`）
- 每档输出 = `(price − w) × consume × 10^(d0−2) / 10^di`，EVM 向零截断
- 输出超 USDT0 金库余额 → **归零**（`sell_zero_over_vault`，链上实测）
- ladder / R 未知 → 回退单档线性：`out = in × raw × 10^(d0−2) / (2000 × 10^di)`，
  `raw = price×1999 − sellOff×2000`，受 `maxIn` 截断

## 5. BUY 报价（USDT0 → j）

```
ask = price + askOff        // askOff = askOffsetRaw × scale/10000
q0j = floor(10^(dj+2) × 1999 / (2000 × ask))          // 小额报价状态 in=10^d0
linear = floor(in × 10^(dj+2) × 1999 / (2000 × ask × 10^d0))
```

- **必须用精确有理数除法**，不能 `in × q0j / 10^d0` 替代（低小数位资产大额
  quote 与 q0j 线性有差，asset2 实测差 2,237）
- 输出再按阶梯上限截断：
  - 饱和型（阶梯容量 ≤ 金库余额）：`out = min(linear, maxOut)`
  - 超阈值归零型（阶梯容量 > 金库余额）：`linear > 金库余额 → 0`，否则原样
- BUY 方向同样含 `1999/2000` 因子

## 6. 跨资产（i → j，均非 USDT0）两段式

```
v   = floor(in × raw_i × 10^(d0−2) / (2000 × 10^di))   // 第一段 SELL（含 maxIn 截断 + 归零）
out = floor(v × 10^(dj−d0+2) / ask_j)                  // 第二段 BUY，**不含** 1999/2000 因子（实测）
```

第二段不带因子是关键差异（直接 `0→j` 才带），本地 `engine_quote` 与引擎一致。

## 7. 引擎储备 R 与 maxIn / maxOut

- 引擎储备 R 来自**引擎存储槽**（asset6 = 29.4e15、asset2 = 20000），
  长期稳定、**不随 Swap 变化**；可用 `getAssetConfig` 读取，或从快照大额
  probe + ladder 反推
- `maxIn_i`（SELL 输入上限）= `ladderWeight_sell × engineReserve`（= Σ qty×R）；
  快照用 100 整枚 probe 精确恢复（候选 R 对拍 + 精确 maxInput）
- `maxOut_j`（BUY 输出上限）= 阶梯总容量或金库余额，由大额 probe 观测
- 多档阶梯资产（如 DOG）100 整枚 probe 与单档线性不兼容 → 本地仅小额区
  可精确复刻；生产 update 路径直接携带精确 ladder，无此问题

## 8. 三层数据同步（事件驱动，无轮询）

| 层 | 来源 | 处理 |
|---|---|---|
| L1 | Swap 事件 | 更新本地余额；费率以引擎价格精确推导为准 |
| L2 | flashblocks raw tx 增强 | 解析 `0x024b94f6` calldata → 注入 7 word → `apply_l2_update_full`，零 RPC |
| L3 | 无 raw bytes 的 update 日志 | 标记 stale → `AsyncUpdate` → 批量静态调用拉取 quote 恢复 bid/ask |

周期性快照任务（`binaryfi_sync_interval_secs`，默认 15s）只做**容量/上限观测**，
不做价格覆盖（价格以日志为准，快照补缺 + 保鲜判断防止旧块 quote 覆盖新价格）。

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
- `rem = in − in/2000`，in<2000 时与 `in×1999/2000` 不同（易错点）
- 买入被禁用资产（0→j quote 恒 0）仍可能收到 update 日志 → 只更新价格，费率保持 0
- 周期快照不能覆盖日志驱动的更新价格（保鲜判断：日志优先、快照补缺）

## 11. 逆向工具链记录

- RPC：生产 `https://rpc.xlayer.tech`；tenderly `https://xlayer.gateway.tenderly.co/<key>`
  （`debug_traceCall` 第三参传 `{}`，**勿传 tracer**；内建 structLogs tracer 返回
  `ReferenceError: structLogs is not defined`，自定义 JS tracer 也不支持）
- Alchemy `https://xlayer-mainnet.g.alchemy.com/v2/<key>`：支持 structLogs
- 关键 trace 文件：`/tmp/sell_a6_1e19.json`、`/tmp/sell_a6_2e19.json`、
  `/tmp/sell_a6_5e19.json`、`/tmp/a2_quote_10000.json`
- 反汇编脚本：`/tmp/evm_dis.py`（指向引擎字节码 `/tmp/engine_code_noprefix.hex`，
  注意不要误指向 pool code）

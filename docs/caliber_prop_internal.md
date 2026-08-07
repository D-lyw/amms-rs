# Caliber propAMM 报价逻辑逆向文档

> 目标：精确复刻 XLayer 上 Caliber propAMM 合约
> `0x154586B2479b9a11e3d4db90024Dc0e26F097312` 的链上 `quote()`，
> 使本地 `simulate_swap` 与链上报价**逐位一致**。
>
> 方法：字节码反汇编 + 自写 EVM 解释器逐步执行（`docs/caliber_prop_re/`），
> 符号分析 + 链上多 pair 全量交叉验证。
>
> 验证基线（块 66309105，4 个 pair：USD₮0/xETH（335c…）、xSOL/USD₮0（d81a…）、
> USD₮0/xBTC（55c4…）、USD₮0/WOKB（5dda…），pair 集合以链上为准，
> 与链下页面当前展示的 wrapped 股票 pair 不同）：
> - **正向**（token0→token1）：全部金额与链上 `quote()` 一致 ✅
> - **反向**（token1→token0）：4 pair × 14 金额全部与链上 `quote()` 一致 ✅
>   （112 条 quote 零偏差，`cargo test -p amms --test caliber_prop` 全绿）
>
> 历史验证（pos 机制发现前的初版公式）：5 pair × 双向 × 73 金额曾达 730/730；
> 其中反向部分在 `pos=0` 特例下成立。引入真实 `pos` 后补上了 §7 的两处差异，
> 并发现 `pos` 只在 `cfg+7.block == 当前执行块` 时有效（见 §4）。

## 1. 存储布局

pair 的配置按 `pairId` 存于两个基址：

```
cfg  = keccak256(pairId || uint256(6))
data = keccak256(pairId || uint256(7))
```

| 槽位 | 内容 | 说明 |
|---|---|---|
| `cfg+0` | token0 | 低 160 位（`token << 0`，非 96 位移位） |
| `cfg+1` | token1 + decimals | 低 160 位 = token1；`byte@0xa0` = dec_token0，`byte@0xa8` = dec_token1 |
| `cfg+2` | ladder 长度 n | |
| `cfg+3` | window | 末段之后的合成尾段长度（当前 500） |
| `cfg+4` | reserveX | token0 余额 |
| `cfg+5` | reserveY | token1 余额 |
| `cfg+6` | 打包配置 | 低 64 位 = fee（1e6 基数，200 = 2 bps）；`byte@0x40` = per-pair 暂停标志 |
| `cfg+7` | **位置状态 pos** | `[block:32][0:64][pos:96][0:96]`（见 §4） |
| `data+0` | 时间戳 + 参数 | `[uint32 tsY][uint32 tsX][uint32 field1][uint64 field0]` |
| ladder[i] | 段点 | `keccak256(uint256(cfg+2)) + i`，每槽 `[amountIn:128][amountOut:128]` |

注意：token 地址在槽位**低 160 位**，早期脚本用 `>>96` 提取会截断地址导致
`quote()` 因 token 校验失败 revert（custom error `0x0c40208b`）。

## 2. 链上 quote() 的完整行为

`quote(pairId, tokenIn, tokenOut, amountIn)` 依次：

1. **token 校验**：tokenIn/tokenOut 必须等于 pair 的 token0/token1（否则 revert `0x0c40208b`）。
2. **过期检查**：`deadline = ((data0 >> 128) & u32) << 32 | ((data0 >> 96) & u32)`，
   再加全局 slot2（有效窗口，当前 20 秒）。
   `block.timestamp > deadline` → revert `0x2af96ae8`。
   - 当 `data0` 高 32 位（tsY）非零时 deadline 变成 64 位巨大值，**永不过期**
     （合约实际行为，本地必须同样处理）。
3. **暂停检查（两步，反汇编确认）**：
   - `SLOAD(3) & 0xff != 0` → **全局暂停**，revert `0x8507a90d`（pc 0x3cd3 → 0x1dbd）。
   - 否则 `SLOAD(cfg+6) byte@0x40 != 0` → **per-pair 暂停**，revert `0xb69ec3f0`（pc 0x3cdc → 0x3d32）。
   - 解释器证据：`PUSH1 03; SLOAD; AND 0xff; JUMPI 0x1dbd`，0x1dbd 处 `PUSH4 0x8507a90d; MSTORE; REVERT`。
4. **报价计算**：分段公式（见下），输出 `min(out, 对应方向 reserve)`。

## 3. 报价公式（token0 → token1 正向）

约定：`ladder[i] = (x_i, y_i)`，`n = len(ladder)`，`scale = 10^(dec0 - dec1)`，
`fee`、`field0`、`field1`、`window` 见存储布局。所有运算为 EVM uint256，
除法为**向零截断**（`DIV`）。

```
xp = amountIn - trunc(amountIn * fee / 1e6)      // 先算 fee 再减（不是 amount*(1e6-fee)/1e6）
acc = 0
for i in 0..n:
    x_next = ladder[i+1].x            (i < n-1)
           = x_i + window             (i == n-1，合成边界)
    a_i     = 1e6 - (x_i + field1)
    a_next  = 1e6 - (x_next + field1)
    P  = 1e6 * 2 * y_i / (a_i + a_next)
    th = (P * 1e9 * scale + field0 - 1) / field0        // ceil
    if xp >= th:
        acc += y_i                     // 累计（不是覆盖！多段满段时链上是累加）
        xp  -= th
    else:
        r2   = field0 * xp / (1e9 * scale)
        part = r2 * 2 * y_i * a_i / (1e6 * 2 * y_i + r2 * (a_i - a_next))
        acc += part
        return min(acc, reserveY)
// 尾段（超过最后合成边界）：按倒数第二段直线外推
a_last = 1e6 - (ladder[n-1].x + window + field1)
tail   = field0 * xp * a_last / (1e9 * scale * 1e6)
return min(acc + tail, reserveY)
```

### 反向（token1 → token0）—— 有状态，必须先读 pos

链上反向报价**不是**从段 0 开始：合约维护一个"当前位置 `pos`"
（`cfg+7`，即该 pair 已从 token0 侧被兑换掉的累计输出量），
反向兑换从这个位置开始按剩余量计算。**反向公式与正向不对称**，本地必须
读 `cfg+7` 的 pos 才能逐位对齐（§4 详解）。

```
xp = amountIn - trunc(amountIn * fee / 1e6)
pos = (cfg7 >> 96) & (2^96 - 1)
cum = 0
for i in 0..n:
    if pos >= cum + y_i:
        cum += y_i; continue          // 整段已被正向消耗，跳过
    offset = pos - cum                // 段内已消耗量
    R      = y_i - offset             // 段内剩余量（反向可用的量）
    x_next = ladder[i+1].x 或 x_i + window
    a_i     = 1e6 + (x_i + field1)
    a_next  = 1e6 + (x_next + field1)
    a_eff   = a_i + (a_next - a_i) * offset / y_i     // EVM 截断
    delta_eff = a_next - a_eff
    out = xp * 1e6 * 1e9 * scale * 2 * R
          / (field0 * (2 * R * a_eff + xp * delta_eff))
    return min(out, reserveX)
// pos 超过全部段（理论不会发生）：按末段 a 直线外推
a_last = 1e6 + (ladder[n-1].x + window + field1)
tail   = xp * 1e6 * 1e9 * scale / (field0 * a_last)
return min(acc + tail, reserveX)
```

> **注意**：pos 版本反向公式在 `pos=0` 时退化为旧版逐段公式
> （`offset=0, R=y_i, a_eff=a_i, delta_eff=a_next-a_i`，且不会跳过任何段）。
> 本地 `quote_reverse_exact` 已实现完整 pos 版本（§5）。

## 4. pos 机制详解（本次逆向的核心发现）

`cfg+7` 的 256 位布局：

```
cfg+7 = [ block(32bit) | 0(64bit) | pos(96bit) | 0(96bit) ]
         └ 高 64 位       └ bits 96..191   └ 低 96 位
```

- `pos = (cfg7 >> 96) & (2^96 - 1)`（bits 96..191）。
- 高 64 位是最近一次更新该 pair 的区块号（实际只用低 32 位）。
- **有效性规则（EVM trace 确认）**：仅当 `cfg+7.block == 当前执行块` 时，
  反向报价才使用真实 `pos`；否则按 `pos=0`（从段 0 整段）计算。
  本次 4 pair 中仅 pair1 的 `cfg+7.block` 等于当前块（pos 有效），
  pair2/3/4 均按 `pos=0` 计算。
- 例（块 66309105）：pair1 `cfg7 = 0x0000000003f3cbf1 0000000000000000 0f962ef7 000000000000000000000000`
  → block=`0x3f3cbf1`=66309105，pos=`0x0f962ef7`=261500663。

**语义**：`pos` 是 ladder 在 token0→token1 方向已被累计兑换的 `y` 总量。
反向报价从 `pos` 所在段开始，只对**段内剩余量** `R = y_i - offset` 报价，
并用当前位置插值出的斜率 `a_eff`，而不是整段从头计算。这就是"黑箱报价"
与本地 `pos=0` 实现产生差异的根因。

**完整数值示例（pair1 @ 66309105，反向 amount=1）**：

```
pos=261500663, ladder=[(10,2e8),(50,9e8),(300,1e9)], field0=1900370065664,
field1=105, fee=200, dec=(18,6) → scale=1e12, rx=4035636208082082157
xp = 1 - 1*200/1e6 = 1
段0: cum=0, pos >= 0+2e8 → 跳过, cum=2e8
段1: offset = 261500663-2e8 = 61500663, R = 9e8-61500663 = 838499337
     a_1 = 1e6+(50+105) = 1000155, a_2 = 1e6+(300+105) = 1000405
     a_eff = 1000155 + (1000405-1000155)*61500663/9e8 = 1000155 + 17 = 1000172
     delta_eff = 1000405 - 1000172 = 233
out = 1 * 1e6 * 1e9 * 1e12 * 2 * 838499337
      / (1900370065664 * (2*838499337*1000172 + 1*233))
    = 526122805   ← 与链上 quote() 完全一致
```

解释器验证：`docs/caliber_prop_re/verify_pair1_reverse.py` 输出 `526122805`。

**存储读取**：本地 `fetch_exact_snapshot` 读 `cfg+7` 提取 `pos_block`/`pos`，
并按有效性规则决定使用真实 pos 还是 0（已落地，§5）。

## 5. 本地实现（src/amms/caliber_prop）

现状与待办：

- ✅ `fetch_exact_snapshot` / `batch_refresh_snapshots`：`eth_getStorageAt`
  直接读存储，拿 reserve + 原始 ladder + field0/field1/fee/window/scale +
  过期状态 + `cfg+7` pos。**批量路径**（初始化 `init_batch` 与周期对账共用）：
  固定槽位 + ladder 槽位全部走 JSON-RPC batch（`STORAGE_BATCH_SIZE=10`，
  实测生产 `rpc.xlayer.tech` batch 上限 11），RPC 往返从
  每 pool ~10+n 次降到 ~每 10 槽 1 次；与逐槽 `eth_getStorageAt` 同一区块
  逐字段对比 0 偏差（`cargo run --example caliber_prop_batch_probe`）。
- ✅ `CaliberLadderState`（`types.rs`）：已含 `field0`/`field1`/`fee_rate`/`window`/
  `scale`/`pos`/`deadline` 字段并序列化（`deadline` = `data+0` 的 tsX，更新交易写入）。
- ✅ `quote_forward_exact`：正向公式 U256 实现，与链上逐位一致。
- ✅ `quote_reverse_exact`：完整 pos 版本（段跳过 + `a_eff` 插值 +
  `R=y_i-offset` + 跨段循环 + 尾段外推），`pos=0` 时退化为整段公式。
- ✅ 暂停判断：`SLOAD(3) & 0xff` 全局暂停（revert `0x8507a90d`）+ `cfg+6 byte@0x40`
  per-pair 暂停（revert `0xb69ec3f0`），已落地 `fetch_exact_snapshot`。
- ✅ `simulate_swap`：`quote(consumed_in + amount_in) - consumed_out`。
- ✅ 过期/暂停 pair 返回空 ladder → `simulate_swap` 返回 0。
- ✅ 现货价格 = 边际价格 `field0 * (1e6 - (x0 + field1)) / 1e15`。

### 同步策略（2026-08 实时交易驱动）

更新交易 `batchUpdateParameters((bytes32,uint64,uint32,uint64)[])`（selector
`0x008dcc8e`）**不 emit 任何事件**，只能从 flashblocks 原始交易发现：

- ✅ **实时路径（XLayer）**：`xlayer_flashblocks.rs` 逐笔 RLP 定位 `to` →
  匹配 caliber 合约地址集合 → 标准 ABI 解码 calldata（`decode_batch_update_parameters`）
  → `CaliberPropPool::apply_batch_update` 增量刷新 `field0`/`field1`/`deadline`。
  块内按 tx_index 排序（EVM 语义后者覆盖前者），`realtime_head` 簿记复用。
- ✅ **对账路径**：周期任务降频为对账/兜底（默认 45s，
  `with_caliber_reconcile_interval` 可配置），覆盖冷启动、断流回填、储备/pos
  低频变动与漏更新纠正；刷新走 `batch_refresh_snapshots` 批量 JSON-RPC；
  `with_caliber_realtime_sync(false)` 退回纯周期拉取。
- ✅ **真实流验证**：`cargo run --example caliber_prop_flashblocks_probe` 实测通过
  （120s：60 笔更新 / 300 pairs，链上 tx + 存储槽交叉验证零失败）。
- 设计文档：`docs/caliber_prop_realtime_sync_design.md`（M1-M4 已完成）。

## 6. 逆向工具链（已归档，离线可复现）

目录 `docs/caliber_prop_re/`：

| 文件 | 用途 |
|---|---|
| `caliber_code.bin` | 合约字节码（22042 B，离线解释器输入） |
| `caliber_state_66309105.json` | 4 pair 在块 66309105 的真实状态（含 pos） |
| `fetch_state.py` | 从任意 RPC/区块抓取状态，`python3 fetch_state.py --block 66309105 --rpc <url> --out out.pkl` |
| `verify_pair1_reverse.py` | 核心验证：pair1 反向 amount=1 → `526122805`（= 链上） |
| `evm_gen.py` | 参数化 EVM 解释器：换 pair/金额，追踪 SLOAD/算术序列 |
| `model.py` | 正反向模型原型，与链上 `eth_call` 逐金额对照，输出 DIFF |

详见 `docs/caliber_prop_re/README.md`。全部脚本只依赖 Python3 + pycryptodome；
字节码已固化，解释器离线可跑，无需重新抓链。

## 7. 已知未对齐边界（已全部修复，块 66309105）

初版 `pos=0` 单段反向公式在块 66309105 上有 2 处偏差，根因都是**缺少真实 pos 的
分段消费逻辑**，已通过 `quote_reverse_exact` 的 pos 版本修复：

1. **pair1（335c…）反向 `w=1e9`**：pos=261500663 落在段 1（非段 0），
   旧公式从段 0 整段报价 → 差 ~1.5e12。pos 版按 `offset=pos-cum` 从段 1 剩余量
   报价后逐位一致（`chain=mine=525943015783951046`）。
2. **pair2（d81a…）反向 `w≥1e4`**：旧公式单段公式无法处理**段内有限余量 + 跨段**；
   pos 版用 `w=min(xp, R)` 逐段跨段循环后逐位一致
   （`w=1e4: chain=mine=133070`）。

**新发现（EVM trace 确认）**：`cfg+7.block != 当前执行块` 时，链上忽略 pos、
按 `pos=0` 整段计算（见 §4）。本次 pair2/3/4 的 `cfg+7.block` 均不等于当前块，
因此其反向报价必须用 `pos=0` 分支才能对齐——本地 `fetch_exact_snapshot`
已实现该有效性判断。

回归入口：`XLAYER_PROVIDER=http://127.0.0.1:8557 CALIBER_TEST_BLOCK=66309105
cargo test -p amms --test caliber_prop`（3 个测试全绿，112 条 quote 零偏差）。

## 8. 逆向过程中的关键坑

1. **token 槽位是低 160 位**：`cfg+0/+1` 的地址不是 `<<96` 对齐，
   错误提取会得到截断地址 → 链上 revert `0x0c40208b`。
2. **fee 扣减**：`xp = amount - trunc(amount*fee/1e6)`，不是
   `trunc(amount*(1e6-fee)/1e6)`。两者在 `amount*fee` 不能整除 1e6 时差 1。
   例：amount=9999，前者 xp=9998，后者 xp=9997；链上为 9998。
3. **正向满段是累加**：`acc += y_i`（不是 `acc = y_i`）。只有一段满段时两者等价，
   多段时（大额）差一个前面所有段的累计值。
4. **EVM 除法向零截断**：Python `//` 对正数等价；有符号场景需手动处理。
5. **过期判断用 64 位 deadline**：tsY 非零时 deadline 巨大 → 永不过期。
   只读 tsX 会把 wSPCXx 等 pair 误判为过期。
6. **反向报价有状态**：必须读 `cfg+7` 的 pos；`pos=0` 只是特例，真实链上
   pos 几乎总是非零（本次 4 pair 的 pos 在 2.4e7 ~ 2.6e8 区间）。
   且 pos **仅在 `cfg+7.block == 当前执行块` 时有效**，否则按 0 计算——
   直接读 `cfg+7` 拿真实 pos 反而会与链上偏差（pair2/3/4 都是这种情况）。
7. **quote() 的 `min(out, reserve)` 封顶**：在 consumed 追踪中，封顶应使用
   **快照总储备**（`consumed_out + 当前剩余`），否则连续 swap_mut 序列会
   错误地提前耗尽（模拟交易不改变链上真实储备）。
8. **静态反汇编有 +1 偏移**：`cast disassemble` 在 0x3cd0 附近对不上原始字节码
   （`0x3cc0: 60 57 60 40 52...`），以解释器（逐字节执行）为准。
9. **pairId 获取**：完整 64 hex pairId 可从合约事件 `0x36d90ab6...`
   （pair 状态更新）的 topics[1] 抓取（签名 `0x36d90ab6736dbd42ac28b968350d068640e9aea3f7b807679fe64d2a50dcbb03`），或从 indexer graph 数据取。

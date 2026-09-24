# R4 观测：dust 池模拟耗时到底花在哪（实测）

> 复现场景：XLayer 196，`update_seq=6338`，block 71297916，`ms_multi_hop=175`（典型 18~130）。
> 嫌疑池：UniswapV4 `0xbd985f978a1c9dcc8322ce37df82fd625646c4a1`（USDG/USDT0，`fee=9`，`tick_spacing=1`）。
> 观测代码：`src/amms/sim_stats.rs` + `tests/sim_stats_walk.rs`。

## 1. 观测设施

`AMMS_SIM_STATS=1`（或 `sim_stats::set_enabled(true)`）打开后，13 个 V3 系热循环
（uniswap_v3 / uniswap_v4 / pancake_v3 / pancake_infinity / aerodrome_slipstream）
每次模拟按 `(chain_id, kind, pool)` 聚合 **步数 / 空 word 数 / 耗时 / 错误数**。

- 关闭（默认）：热路径只多一次 relaxed 原子读，`Drop` 不碰全局状态。
- 开启：每次模拟一次分片锁 + 两次 `Instant::now()`。开销与单次模拟耗时**成反比**：
  dust 池（105 µs）上 ≈ +1%；**浅池（1.2 µs）上 ≈ +49%**。
  ⇒ 生产上应**按批采样**（开一批、取快照、关掉），不要常开。
- `take_snapshot()` 会清空聚合表 → 天然按批切分。

```rust
amms::amms::sim_stats::set_enabled(true);
// ... 跑一批检测 ...
tracing::info!(top = %amms::amms::sim_stats::summary_top(5), "模拟热点");
```

## 2. 事故形状被逐位复现

`tests/sim_stats_walk.rs::incident_fixture_matches_chain_bit_exactly`

| 项 | 值 |
|---|---|
| 池地址（由 PoolKey 派生） | `0xbd985f978a1c9dcc8322ce37df82fd625646c4a1` ✅ 与事故池一致 |
| 起始状态 | `sqrt=2575468425351407710666126886`，`tick=-68530`，`liq=999616`，`lp_fee=9`，`protocol_fee=0` |
| 输入 | 964,642 raw USDT0 |
| 输出 | **29,748,180** ✅ 与链上 txIndex 31 逐位一致 |
| 结束 | `tick=-58`，`liq=999616` |
| 走步 | **268 步，全部跨空 word** |

即：文档给的 fixture（PoolKey + 状态 B）**是自洽且正确的**，`fee=9 / protocol_fee=0`
下 amms 与链上逐位相同。

## 3. 耗时分解（268 步，release，取 40 次最小值）

| 环节 | ns/步 | 占比 | 268 步合计 |
|---|---|---|---|
| `next_initialized_tick_within_one_word`（走 word） | 12 | **3%** | 3.2 µs |
| `get_sqrt_ratio_at_tick` | 38 | 10% | 10 µs |
| `compute_swap_step`（U256 乘除取整） | 268 | **68%** | 72 µs |
| **完整一次模拟** | 393 | 100% | **105 µs** |

debug 构建同口径：`bitmap=362ns / sqrt=2825ns / swap_step=4841ns / full=7612ns`
⇒ **一次 dust 模拟 2.04 ms（19x 于 release）**。

**关键结论：走 word 只占 3%。** 把 268 步折叠成 1 步最多省 3%，
无法解释也无法修复 175ms。真正的成本在 `compute_swap_step`（U256 大数乘除取整），
而它是**每一步都必须算的真实价格演化**，不是可以跳过的簿记。

## 4. 175ms 的数量级对账

| 构建 | 单次 dust 模拟 | 74 个 `passed_filter` 全为 dust 形状时 |
|---|---|---|
| release | 105 µs | 7.8 ms |
| **debug** | **2.04 ms** | **151 ms** ← 与 175ms 同量级 |

dex-arbitrage 的 `Cargo.toml` 无 `[profile.release]` 覆写、`justfile` 用裸 `cargo run`
（= debug）。**175ms 高度疑似"debug 构建 + 该批 dust 形状模拟偏多"，而不是算法复杂度问题。**
这一点必须由线上开关确认：跑批时打开 `AMMS_SIM_STATS=1`，看：

- 该批总模拟次数 × 单次耗时 vs `ms_multi_hop`；
- 若 `total_ns` 只占 `ms_multi_hop` 的一小部分 ⇒ 瓶颈在模拟之外的图搜索/过滤；
- 若 debug 下对齐、release 下消失 ⇒ 结论即"用 release 跑线上"。

## 5. 折叠（B/R1）不精准：实测 +518 wei

`folding_is_not_bit_exact_518_wei`：同一份数学跑两遍，恒定流动性、无 tick 跨越。

| 方案 | 步数 | 输出 |
|---|---|---|
| 逐 word 走（现网 = 链上） | 268 | **29,748,180** |
| 一步折到下一个已初始化 tick | 1 | 29,748,698（**+518 wei**） |

差额来自 268 次 `ceil` 被抹成 1 次。**折叠不是"误差可控"，是"必然偏离链上"。**
所以需求文档里"折叠 + 与链上对拍 ≤10 wei"的验收标准在数学上不可能满足。

## 6. 对需求文档 R1~R5 的裁定

| 项 | 裁定 |
|---|---|
| R1 折叠到下一已初始化 tick | ❌ 非精准（+518 wei），且只省 3%，收益也在错误的地方 |
| R3 收紧 dust 守卫 | ❌ 自相矛盾：会拒掉这笔真实的 $29.7 机会 |
| R5 裁剪 `tick_bitmap` 同步 | ❌ 危险：缺 word 会被当成空 word ⇒ `liquidity==0` 分支 ⇒ 价格免费移动 |
| R1 伪代码的守卫顺序 | ❌ 守卫放在 `compute_swap_step` 之前会误判部分成交 |
| 文档 175ms 归因 | ⚠️ 未经证实；见 §4 |

## 7. 若要真的省时间（且保持逐位精准）

方向应是**减少 `compute_swap_step` 的调用次数而不改变其语义**：

1. **先确认构建类型**（§4）。若线上是 debug，改 release 即可，零算法风险。
2. **跳过"零输入步"**：当某步 `liquidity == 0`（纯空 word 段）时，价格免费移动到
   word 边界，但**仍要逐 word 走**才能真正免费——这条已经在现网逻辑里。
3. **per-span 累积表（B3）**：只对 `liquidity == 0` 的纯空段做"跨 word 累积"，
   把该段内本该逐 word 发生的 `ceil` 用**精确整数前缀和**一次算对，再落回逐 word 语义。
   仅在**空段内**成立，因此不影响任何有流动性区间的逐 word 结果。
   需要先测：事故这笔的 268 步里有多少步是 `liquidity == 0`——本 fixture 不是（全程 999616），
   所以对**这笔**无效，得看真实 dust 池的分布。
4. 若第 3 条不成立（即空 word 段并非主要形状），则"逐位精准"前提下**没有**免费午餐：
   `compute_swap_step` 就是必须在 release 下跑 105 µs/268 步的实打实计算。
5. **把折叠降级为"预筛"，而不是替代**：折叠的偏差是**系统性偏乐观**（输出偏高，
   本笔 +518 wei / 1.7e-5），所以它**不会漏掉真实机会**，只会多放一些候选进来。
   于是可以：折叠快速筛（1 步）→ 对通过者用逐 word 精确路径复算并只信精确结果。
   精度零损失（最终结果仍逐位对齐链上），只多花少量精确复算；前提是把预筛阈值
   放宽到大于折叠误差上界（本笔为 `268 wei` 输入侧，取 0.01% 量级足够安全）。
   这需要先确认：折叠在"跨已初始化 tick 导致流动性变化"的场景下是否仍单调偏乐观——
   未验证前，预筛只用在恒定流动性路径上。

## 8. 单次模拟的成本模型（release 实测）

| 形状 | 步数 | 单次耗时 |
|---|---|---|
| 浅池（已初始化 tick 就在同 word 内） | 2 | **1.18 µs**（不可变路径） |
| 其中：池深拷贝 | — | 0.03 µs（可忽略，`Arc<bitmap>`） |
| 其中：2 步数学（sqrt+swap_step+word） | 2 | ≈ 0.6 µs |
| 其中：**固定开销**（函数帧 / 状态构造 / 分支） | — | **≈ 0.6 µs** |
| dust 池（268 步全空 word） | 268 | **105 µs**（≈ 89x 浅池） |

**批成本模型**：`batch ≈ S × 1.2µs + N_dust × 104µs`，其中 S = 该批模拟**总次数**。
先算量级：74 个 `passed_filter`、每方向 beam/probe 联合优化（`beam_width 6~8`、
`stage1_log_band 6`、`golden_iterations 10`、`candidates_per_probe 3`）× 3~5 跳
⇒ S 是 **10^4 量级**。`10^4 × 1.2µs ≈ 12ms`…`10^5 × 1.2µs ≈ 120ms`。

**所以"优化哪个逻辑"取决于 S 和 dust 模拟数，这两个数现在由观测直接给出。**

## 9. 优化优先级（按收益/风险排序）

| # | 动作 | 预期收益 | 精度风险 |
|---|---|---|---|
| 1 | **确认线上是 release 还是 debug**。`dex-arbitrage/Cargo.toml` 无 `[profile.release]`，`justfile` 用裸 `cargo run`（debug）。debug 全线慢 **19x** | 最高（可达 10x+） | 无 |
| 2 | **per-batch 单跳模拟 memo**：`simulate_hop(amm, t_in, t_out, amount)` 是**纯函数**（批内池状态冻结），按 `(pool, t_in, t_out, amount)` 缓存 `Result<U256>` | 命中即 1.2µs → ~0.1µs；联合优化/beam 反复打同一条跳 | **零**（纯函数按定义精确） |
| 3 | **dust 池 span 前缀累积表**（本文件 §7.3）：只对 `liquidity` 恒定的段做跨 word 精确前缀和（`Σ ceil` 逐项算好），二分定位 + 1 次 `compute_swap_step` 收尾 | dust 模拟 105µs → ~2µs | **零**（整数前缀和，与逐 word 逐位同值）；代价是每个段建表 ~72µs + 按状态版本失效 |
| 4 | 调低 `golden_iterations` / `stage1_log_scale_probe_bands` | 与 S 成正比 | 影响**最优金额**搜索精度（非链上对拍误差），需单独评估 |
| 5 | 折叠（R1） | 只省 3%（走 word 的份额） | ❌ 非精准，+518 wei |

第 2 条是"最大且零风险"的那条：它不改变任何数值，只是不再重复算同一个纯函数。

## 10. 真实批次归因（决定性）

回放：`dex-arbitrage/crates/core/src/bin/replay_xlayer_71297916_dust_perf.rs`
（复刻事故块 71297916 的生产检测链路，release，本地 amms + `AMMS_SIM_STATS=1`）。

```
AMMS_SIM_STATS=1 RUST_LOG=info cargo run --release --bin replay_xlayer_71297916_dust_perf
```

批次形状：`updated_pools=13`、`pairs=11`、`cycles=180`、`snapshot_pools=161`。

| 指标 | 实测（两次运行一致） |
|---|---|
| 模拟**总次数** S | **15,482** |
| 模拟累计 tick-walk 步数 | 346,939 |
| 模拟 **CPU 总耗时** | **80~85 ms** |
| multi-hop **墙钟** | **15 ms** ⇒ 有效并行度 ≈ 5.4 核 |

### 热点池 top-5（`summary_top`）

| 池 | 协议 | sims | 累计 | 均摊 | 最大步数 |
|---|---|---|---|---|---|
| `0xb0400114…5d764e` | **uniswap_v3** | 550 | **49.57 ms（58%）** | 90.1 µs | **4361**（空 word 4358） |
| `0x5d7e3ad0…df437c` | uniswap_v3 | **3120** | 6.07 ms | 1.95 µs | 115 |
| `0xf8096cd5…2b72b9` | uniswap_v4 | 853 | 3.65 ms | 4.28 µs | 36 |
| `0x386948b4…dc8e17` | uniswap_v3 | 800 | 3.34 ms | 4.17 µs | 54 |
| `0x77ef18ad…8b7dcc` | uniswap_v3 | 210 | 3.27 ms | 15.6 µs | 427 |
| `0xbd985f97…46c4a1` ← **文档嫌疑池** | uniswap_v4 | **54** | **1.20 ms（1.4%）** | 22.2 µs | 507 |

### 结论（推翻需求文档的前提）

1. **真正的 CPU 黑洞是 `0xb0400114…`（V3 dust 池）**：单池吃掉 **58%** 的模拟 CPU，
   550 次模拟 × 均摊 90 µs，单次最多走 **4361 个 tick word**（4358 个空 word，
   比事故池的 268 步大 16 倍）。
2. **文档锁定的 `0xbd985f97…` 只占 1.4%**。268 步的形状真实存在，但在批次尺度上
   无关紧要。照它优化等于优化 1.4%。
3. 第二档成本是**纯次数**驱动的：`0x5d7e3ad0…` 3,120 次 × 1.95 µs = 6 ms。
   全批 15,482 次模拟里，浅池（<100 步）约占 11.5k 次 / 23 ms（28%）。
4. 墙钟 15 ms << CPU 80 ms：**多跳模拟是并行的**，所以线上 `ms_multi_hop` 是并行后的
   墙钟；单看某一个池的耗时会被并行度稀释。要让墙钟下降，必须让**最热的那个池**变快，
   而不是平均值。

### 因此优化优先级重排

| # | 动作 | 针对 | 预期 |
|---|---|---|---|
| 1 | **长空 word + 恒定流动性段的精确前缀累积表**（§7.3/§9#3） | `0xb0400114…`：4361 步 → ~12 次二分 + 1 步 | 该池 49.6 ms → ~2 ms（**省全批 CPU 的 56%**） |
| 2 | 确认线上构建类型（debug ⇒ 全线 19x） | 全批 | 同上量级 |
| 3 | per-batch 单跳 memo（纯函数缓存） | `0x5d7e3ad0…` 这类高次数低耗时池 | 省全批 CPU 的 ~20% |
| 4 | 折叠（文档 R1） | — | 只省 3%，且非精准 |

**第 1 条是唯一能把 175 ms 级墙钟真正压下去、且保持逐位精准的改动。**

## 11. 复现命令

```bash
cargo test --release --test sim_stats_walk -- --nocapture --test-threads=1
cargo test --test sim_stats_walk -- --nocapture --test-threads=1   # debug 对照
```

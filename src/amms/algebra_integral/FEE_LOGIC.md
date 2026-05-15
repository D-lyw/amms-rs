# Algebra Integral — 动态 Fee 计算逻辑

## 概述

Algebra Integral 协议使用 **plugin 架构** 实现动态 swap fee。池子通过 `pluginConfig` 中的标志位控制 fee 行为。本模块在本地复刻了完整的动态 fee 计算逻辑，使 `simulate_swap()` 无需 RPC 调用即可获得正确的 fee 值。

## Fee 模式

### 三种模式

Algebra 池子有三种 fee 模式，由 `pluginConfig` 决定：

| 模式 | `DYNAMIC_FEE` (bit 7) | `BEFORE_SWAP` (bit 0) | 说明 |
|------|----------------------|----------------------|------|
| **Static** | 0 | 0 | fee 固定，不调用 plugin |
| **DynamicGlobal** | 1 | 0 | fee 由 `plugin.getCurrentFee()` 动态计算 |
| **DynamicHooked** | 1 | 1 | `beforeSwap` hook 可以返回 `overrideFee` 覆盖 fee |

判断逻辑（`mod.rs:refresh_fee_mode()`）：

```rust
self.fee_mode = if dynamic_enabled && before_swap_enabled && plugin_connected {
    AlgebraFeeMode::DynamicHooked
} else if dynamic_enabled && plugin_connected {
    AlgebraFeeMode::DynamicGlobal
} else {
    AlgebraFeeMode::Static
};
```

### 链上 fee 决定流程

```
swap()
  → _beforeSwap()                          // pluginConfig 的 BEFORE_SWAP 标志位控制
  │  如果 plugin 返回 overrideFee > 0:      // 本次 swap 使用 override 值
  │    cache.fee = overrideFee + pluginFee
  │  否则 if DYNAMIC_FEE:
  │    cache.fee = plugin.getCurrentFee()  // 动态计算
  │  否则:
  │    cache.fee = _lastFee                // 静态 base fee
  │
  → _calculateSwap(cache.fee)              // 用 cache.fee 做 swap 数学
```

## 本地计算架构

### 数据结构

| 字段 | 类型 | 用途 |
|------|------|------|
| `fee_config: Option<AlgebraFeeConfig>` | alpha1/2, beta1/2, gamma1/2, baseFee | AdaptiveFee 公式参数，从 plugin 读取 |
| `timepoints: Option<TimepointCache>` | 环形缓冲 | VolatilityOracle timepoints |
| `stale_fee_config: bool` | 标记 | FeeConfiguration 事件后标记过期 |
| `inner.fee: u32` | 继承自 UniswapV3Pool | **最终用于 `simulate_swap()` 的 fee** |

### 文件职责

```
algebra_integral/
├── mod.rs             池子结构体、init/sync/simulate_swap、seed 方法
├── adaptive_fee.rs    AdaptiveFee 纯数学：getFee / sigmoid / expXg4
├── timepoint.rs       TimepointCache：环形缓冲、volatility 计算、二分搜索
└── FEE_LOGIC.md       本文档
```

## 三种模式的完整链路

### Static

```
init(block):
  └─ sync_pool_state() → contract.fee() → lastFee（固定值，如 500）
  └─ is_dynamic_fee_enabled() = false → 不调用 seed_timepoints/seed_fee_config

sync(Swap):
  └─ Swap handler: 更新 price/liquidity/tick
  └─ reconcile_dynamic_fee() → inner.fee = last_fee
  └─ is_dynamic_fee_enabled() = false → 跳过 compute_fee()

simulate_swap():
  └─ 使用 inner.fee = static fee（正确）
```

### DynamicGlobal

```
init(block):
  └─ sync_pool_state() → contract.fee() → plugin.getCurrentFee()（如 201）
  └─ seed_fee_config() → 从 plugin 读取 alpha1/2, beta1/2, gamma1/2, baseFee
  └─ seed_timepoints() → Multicall3 读取 windowStartIndex→tp_idx 的 timepoints

sync(Swap):
  └─ Swap handler: 更新 state（不设 override）
  └─ reconcile_dynamic_fee() → inner.fee = last_fee（=500，错误的）
  └─ overrides 清空（0）
  └─ compute_fee(block_timestamp) → 201 ✅ 覆盖纠正

simulate_swap():
  └─ 使用 inner.fee = 201（动态 fee，正确）
```

### DynamicHooked

```
init(block):
  └─ 同 DynamicGlobal

sync(Swap):
  └─ Extended Swap handler:
  │     last_override_fee = event.overrideFee（如 500）
  │     inner.fee = 500 + 0 = 500
  └─ reconcile_dynamic_fee() → inner.fee = 500（用 last_override_fee）
  └─ overrides 清空（0）
  └─ compute_fee(block_timestamp) → 201 ✅
      ⚠ overrideFee=500 是已处理 swap 的值
      ⚡ 下一笔 swap 应使用 plugin.getCurrentFee() = 201

simulate_swap():
  └─ 使用 inner.fee = 201（动态 fee，正确）
```

### 关键设计

**`is_dynamic_fee_enabled()`** 是三层逻辑的分水岭：

| 功能 | Static | DynamicGlobal/Hooked |
|------|--------|---------------------|
| `seed_timepoints()` | ❌ 跳过 | ✅ 从 windowStartIndex 到 tp_idx 读取全部 timepoints |
| `seed_fee_config()` | ❌ 跳过 | ✅ 从 plugin 读取 AdaptiveFee 参数 |
| `compute_fee()` 覆盖 inner.fee | ❌ 跳过 | ✅ 覆盖 `reconcile_dynamic_fee()` 的值 |
| `stale_fee_config` 检查 | ❌ 跳过 | ✅ 标记后回退到 base fee |

## Timepoint 数据管理

### 播种（seed_timepoints）

```rust
fn seed_timepoints(block, provider):
  1. plugin.timepointIndex() → tp_idx           // 最新 timepoint 索引
  2. plugin.timepoints(tp_idx) → last_tp        // 获取 windowStartIndex
  3. 计算范围: start=last_tp.windowStartIndex, end=tp_idx
  4. 分块 (1500/chunk) Multicall3.aggregate3()  // 批量读取
  5. TimepointCache::seed(&timepoints, tp_idx)
```

### 事件驱动更新（write）

在 `sync()` 中处理 Swap 事件时：

```rust
// Swap 事件处理器（pre-swap tick 写入）
self.timepoints.write(block_timestamp, self.inner.tick);
// 然后才更新 self.inner.tick
self.inner.tick = event.tick.as_i32();
```

`write()` 对应 `VolatilityOracle.write()`：

1. 如果 `last.block_timestamp == current_timestamp`：跳过（同块不重复）
2. 计算 `avg_tick` 和 `window_start_index`（通过本地 timepoints 二分搜索）
3. 创建新 timepoint：`volatility_cumulative += volatility_on_range(delta, ...)`

### 清空策略

| 触发点 | 行为 |
|--------|------|
| `sync_all_pools()` | 清 `timepoints` + `fee_config` → 重新播种 |
| `FeeConfiguration` 事件 | 设 `stale=true`，清 `fee_config` → 下次 `seed_fee_config` 刷新 |
| `Plugin` 事件 | `AsyncUpdate` → `init()` → 重新播种 |
| 单个 `sync()` | 通过 `write()` 增量更新 |
| `update()` | 不清除 |

## 数学实现

### AdaptiveFee

```
fee = baseFee + sigmoid1(volatility/15) + sigmoid2(volatility/15)

sigmoid(x, gamma, alpha, beta) = α / (1 + e^((β-x)/γ))
    = α * e^((x-β)/γ) / (1 + e^((x-β)/γ))

exp 近似：泰勒展开 e^(x/g)  + 查表 e^xdg（整数部分）
实现在 adaptive_fee.rs
```

### Volatility

```
volatility = avg( (tick - avgTick)² ) over WINDOW (1 day)

通过 timepoints 的 volatilityCumulative 差值计算：
volatility = (lastVolCumulative - startVolCumulative) / WINDOW

区间 volatility 公式（_volatilityOnRange）：
  Σ ((k-p)²·t² + 2(k-p)(b-q)·t + (b-q)²) for t in [0, dt]
  使用 sumOfSequence 和 sumOfSquares 的闭式公式
  中间计算使用 i128（原 i64 在 dt=86400 时溢出 4 亿倍）

实现在 timepoint.rs
```

## FeeConfiguration 事件处理

plugin 合约的 `changeFeeConfiguration()` 会触发 `FeeConfiguration` 事件。
该事件来自 **plugin 合约**（不是 pool），通过 StateSpace 层的路由机制分发。

```rust
sync() 中处理:
  stale_fee_config = true
  fee_config = None                              // 清空
  inner.fee = self.last_fee.max(1)               // 回退到 base fee

// 之后：
reconcile_dynamic_fee()                          // 可能覆盖 inner.fee
compute_fee() → stale=true → 返回 None           // 跳过

// 下一个 seed_fee_config 调用：
seed_fee_config() → 重新从 plugin 读取 → stale=false
```

## 时间戳来源

| 来源 | 文件 | 可靠性 |
|------|------|--------|
| `eth_getLogs`（Ethereum/Arbitrum） | — | `blockTimestamp` 由主流 RPC 填充 |
| flashblocks（Base） | `state_space/flashblocks.rs:385` | 从 `base.timestamp` 解析，后备按 2s/block 估算 |
| Arbitrum feed | `state_space/arbitrum_feed.rs:152` | 标准 `eth_getLogs` |

`compute_fee(ts)` 中的 `ts` 来自 `log.block_timestamp`，仅在 `Some` 时执行。
如果为 `None`（极端 RPC 边缘情况），跳过计算，fee 保持 `reconcile_dynamic_fee()` 的值。

## 测试验证

| 测试 | 验证内容 |
|------|---------|
| `adaptive_fee::tests` (7 cases) | sigmoid、expXg4、getFee 纯数学 |
| `timepoint::tests` (6 cases) | write、getAverageVolatility、volatility_on_range、lte cmp |
| `test_compute_fee_matches_chain` | 5 个真实池子 `compute_fee()` == `plugin.getCurrentFee()` 0 误差 |
| `test_sync_drift` | 6000 区块事件重放后 tick/sqrt/liquidity/last_fee 无漂移 + `compute_fee()` 与链上一致 |
| `test_swap_compare` | 65 exact_in + 70 exact_out samples，全 0 drift |
| `test_root_cause_fee_variation_vs_init_data` | fee 漂移分析，implied_fee 与链上匹配 |

# Aerodrome AMM 协议完整分析报告

> 分析日期: 2025-02-27
> 分析目标: 为 amms-rs 项目集成 Aerodrome AMM 协议提供技术参考

## 目录

1. [协议概述](#一协议概述)
2. [V2AMM 详细分析](#二v2amm-详细分析)
3. [Slipstream (CL AMM) 详细分析](#三slipstream-cl-amm-详细分析)
4. [复用建议总结](#四复用建议总结)
5. [实现优先级](#五实现优先级)

---

## 一、协议概述

### 1.1 基本信息

| 属性 | 详情 |
|------|------|
| **协议名称** | Aerodrome Finance |
| **链** | Base (Coinbase Layer 2) |
| **上线时间** | 2023年8月 |
| **架构来源** | Velodrome V2 fork (Optimism) → Solidly 改进版 |
| **市场份额** | Base DEX 市场份额 ~63% |
| **TVL** | ~$5.8 亿 (2025年数据) |
| **经济模型** | ve(3,3) (vote-escrow + (3,3)) |

### 1.2 两大 AMM 类型

```
┌─────────────────────────────────────────────────────────────┐
│                    Aerodrome Finance                         │
├─────────────────────────────────────────────────────────────┤
│                                                              │
│  V2AMM (contracts 仓库)    Slipstream (slipstream 仓库)     │
│  ──────────────            ───────────────                   │
│                                                              │
│  ┌─────────────┐             ┌─────────────────────┐         │
│  │   Volatile  │             │  CL AMM (类似UniV3) │         │
│  │   (vAMM)    │             │                     │         │
│  └─────────────┘             └─────────────────────┘         │
│       │                                │                      │
│  ┌─────────────┐             集中流动性 + 自定义价格范围         │
│  │   Stable    │             Tick-based AMM                │
│  │   (sAMM)    │             动态费用配置                     │
│  └─────────────┘                                              │
│                                                              │
└─────────────────────────────────────────────────────────────┘
```

### 1.3 池子类型

| 类型 | 代码标识 | 公式 | 适用场景 | 费用范围 |
|------|---------|------|----------|----------|
| **Volatile** | `stable=false` | `x * y = k` | 普通代币对 | 0.05%-1% |
| **Stable** | `stable=true` | `x³y + y³x = k` | 稳定币对 | 0.01%-0.05% |

**重要发现**: Stable 和 Volatile 不是两种不同的 AMM 机制，而是 **同一 V2Pool 合约中的不同费用配置**！

### 1.4 GitHub 仓库

- **V2AMM**: https://github.com/aerodrome-finance/contracts
- **Slipstream**: https://github.com/aerodrome-finance/slipstream

---

## 二、V2AMM 详细分析

### 2.1 合约来源

- **基础仓库**: `velodrome-finance/contracts`
- **核心文件**: `contracts/Pool.sol`
- **池子类型**: 通过 `stable` bool 参数区分

### 2.2 Swap 计算逻辑对比

#### 2.2.1 Volatile 模式 (与 Uniswap V2 完全相同)

**Uniswap V2 公式**:
```
output = (amountIn * reserveOut) / (reserveIn + amountIn)
```

**Aerodrome Volatile 代码** (Pool.sol:488-492):
```solidity
function _getAmountOut(uint256 amountIn, address tokenIn, uint256 _reserve0, uint256 _reserve1)
    internal view returns (uint256)
{
    if (stable) {
        // Stable swap 逻辑
        ...
    } else {
        // Volatile swap - 与 Uniswap V2 完全相同!
        (uint256 reserveA, uint256 reserveB) = tokenIn == token0 ? (_reserve0, _reserve1) : (_reserve1, _reserve0);
        return (amountIn * reserveB) / (reserveA + amountIn);
    }
}
```

**结论**: ✅ Volatile 模式的 Swap 计算逻辑 **完全相同**，可以 **100% 直接复用** `uniswap_v2` 的代码！

#### 2.2.2 Stable 模式 (使用 Curve StableSwap 类似公式)

**Aerodrome Stable 公式**:
```
k(x,y) = x³y + y³x
```

**核心代码** (Pool.sol):
```solidity
// k(x,y) 计算函数
function _k(uint256 x, uint256 y) internal view returns (uint256) {
    if (stable) {
        uint256 _x = (x * 1e18) / decimals0;  // 标准化到 1e18
        uint256 _y = (y * 1e18) / decimals1;
        uint256 _a = (_x * _y) / 1e18;
        uint256 _b = ((_x * _x) / 1e18 + (_y * _y) / 1e18);
        return (_a * _b) / 1e18; // x³y + y³x
    } else {
        return x * y;
    }
}

// Newton-Raphson 迭代求解 y
function _get_y(uint256 x0, uint256 xy, uint256 y) internal view returns (uint256) {
    for (uint256 i = 0; i < 255; i++) {
        uint256 k = _f(x0, y);
        if (k < xy) {
            uint256 dy = ((xy - k) * 1e18) / _d(x0, y);
            if (dy == 0) {
                if (k == xy) return y;
                if (_f(x0, y + 1) > xy) return y + 1;
                dy = 1;
            }
            y = y + dy;
        } else {
            uint256 dy = ((k - xy) * 1e18) / _d(x0, y);
            if (dy == 0) {
                if (k == xy || _f(x0, y - 1) < xy) return y;
                dy = 1;
            }
            y = y - dy;
        }
    }
    revert("!y");
}
```

### 2.3 Fee 计算差异

| 特性 | Uniswap V2 | Aerodrome V2AMM |
|------|------------|-----------------|
| **Fee 来源** | 硬编码 (300 = 0.3%) | Factory 动态获取 |
| **获取方式** | `self.fee` | `IPoolFactory(factory).getFee(address(this), stable)` |
| **计算方式** | `amount * (100000 - fee) / 100000` | 相同 ✅ |

**Fee 扣除代码** (Pool.sol:481-482):
```solidity
amountIn -= (amountIn * IPoolFactory(factory).getFee(address(this), stable)) / 10000;
```

### 2.4 数据结构对比

| 状态变量 | Uniswap V2 | Aerodrome V2AMM | 兼容性 |
|----------|------------|-----------------|--------|
| `token0/token1` | ✅ | ✅ | 完全相同 |
| `reserve0/reserve1` | ✅ | ✅ | 完全相同 |
| `blockTimestampLast` | ✅ | ✅ | 完全相同 |
| `price0CumulativeLast` | ✅ | ✅ | 完全相同 |
| `price1CumulativeLast` | ✅ | ✅ | 完全相同 |
| `stable` | ❌ | ✅ bool | Aerodrome 新增 |
| `poolFees` | ❌ | ✅ address | Aerodrome 新增 (费用隔离) |
| `index0/index1` | ❌ | ✅ uint256 | Aerodrome 新增 (费用追踪) |
| `supplyIndex0/1` | ❌ | ✅ mapping | Aerodrome 新增 |
| `claimable0/1` | ❌ | ✅ mapping | Aerodrome 新增 |

### 2.5 事件对比

| 事件 | Uniswap V2 | Aerodrome V2AMM | 兼容性 |
|------|------------|-----------------|--------|
| `Sync(reserve0, reserve1)` | ✅ | ✅ | 完全相同 |
| `Swap(address, uint, uint)` | ✅ | ✅ | 完全相同 |
| `Mint(address, uint, uint)` | ✅ | ✅ | 完全相同 |
| `Burn(address, uint, uint)` | ✅ | ✅ | 完全相同 |
| `Fees(address, uint, uint)` | ❌ | ✅ | Aerodrome 新增 |

### 2.6 同步接口

**完全兼容的接口**:
- `getReserves()` → 返回 `(reserve0, reserve1, blockTimestampLast)`
- `token0()` / `token1()` → 返回代币地址
- `sync` 事件 → 用于监听储备变化

---

## 三、Slipstream (CL AMM) 详细分析

### 3.1 基本信息

| 属性 | 详情 |
|------|------|
| **类型** | 集中流动性 AMM (Concentrated Liquidity) |
| **基础** | Uniswap V3 fork |
| **核心改进** | 更大的 tick spacing、动态费用配置 |
| **市场份额** | ~85% 的 Aerodrome 交易量 |

### 3.2 核心数学对比

| 特性 | Uniswap V3 | Aerodrome Slipstream | 兼容性 |
|------|------------|---------------------|--------|
| **Tick 基础价格** | `1.0001^tick` | `1.0001^tick` ✅ | 完全相同 |
| **sqrtPriceX96** | `sqrt(price) * 2^96` | `sqrt(price) * 2^96` ✅ | 完全相同 |
| **Liquidity 计算** | `L = Δy / Δ(1/√P)` | `L = Δy / Δ(1/√P)` ✅ | 完全相同 |
| **Tick Spacing** | 10/60/200 | **2x 更大** ⚠️ | 不同 |

### 3.3 Tick Spacing 差异

| Fee Tier | Uniswap V3 | Aerodrome Slipstream |
|----------|------------|---------------------|
| 0.01% | 10 | ~20 (2x) |
| 0.05% | 不存在 | ~100 (预估) |
| 0.3% | 60 | ~120 (2x) |
| 1% | 200 | ~400 (2x) |

**影响**:
- 更大的 tick spacing 意味着更少的可配置价格点
- 对 swap 计算逻辑无影响
- 对 tick bitmap 查询有轻微影响

### 3.4 数据结构预期对比

| 状态变量 | Uniswap V3 | Aerodrome Slipstream | 兼容性 |
|----------|------------|---------------------|--------|
| `slot0.sqrtPriceX96` | ✅ | ✅ | 完全相同 |
| `slot0.tick` | ✅ | ✅ | 完全相同 |
| `slot0.observationIndex` | ✅ | ✅ | 完全相同 |
| `liquidity` | ✅ | ✅ | 完全相同 |
| `ticks` mapping | ✅ | ✅ | 完全相同 |
| `tickBitmap` | ✅ | ✅ | 完全相同 |
| `feeGrowthGlobal0X128` | ✅ | ✅ | 完全相同 |
| `feeGrowthGlobal1X128` | ✅ | ✅ | 完全相同 |

### 3.5 Swap 计算逻辑

**预期**: 与 Uniswap V3 完全相同，使用以下步骤：
1. 计算当前 tick 的 swap
2. 移动到下一个 tick
3. 如果跨越 tick，更新流动性
4. 重复直到完成

---

## 四、复用建议总结

### 4.1 V2AMM 复用方案

| 模块 | 复用策略 | 工作量 |
|------|----------|--------|
| **Volatile Pool** | ✅ **100% 直接复用** `uniswap_v2` | 极低 |
| **Stable Pool** | ❌ 需要新增，参考 `curve_ng` | 中等 |
| **Factory** | ⚠️ 修改 fee 获取逻辑 | 低 |
| **池子同步** | ✅ `getReserves()` 完全兼容 | 无 |

### 4.2 Slipstream 复用方案

| 模块 | 复用策略 | 工作量 |
|------|----------|--------|
| **Tick 计算** | ✅ 复用 `uniswap_v3_math::tick_math` | 无 |
| **Swap 计算** | ✅ 复用 `uniswap_v3_math::swap_math` | 无 |
| **Tick Bitmap** | ✅ 复用 `uniswap_v3_math::tick_bitmap` | 无 |
| **Liquidity 计算** | ✅ 复用 | 无 |
| **Factory** | ⚠️ 修改 tick spacing | 低 |

### 4.3 最终架构建议

```
src/amms/
├── aerodrome_v2/
│   ├── mod.rs
│   ├── volatile.rs      // 复用 uniswap_v2
│   ├── stable.rs         // 新增 (参考 curve_ng)
│   └── factory.rs        // 新增动态 fee
│
└── aerodrome_slipstream/
    ├── mod.rs
    ├── pool.rs           // 复用 uniswap_v3 结构
    ├── factory.rs        // 修改 tick spacing
    └── lib.rs             // 复用 uniswap_v3_math
```

---

## 五、实现优先级

| 优先级 | 任务 | 复用程度 | 预计工作量 |
|--------|------|----------|------------|
| **P0** | Volatile AMM | 100% 复用 `uniswap_v2` | 1-2 小时 |
| **P1** | Slipstream 基础框架 | 95% 复用 `uniswap_v3` | 2-4 小时 |
| **P2** | Slipstream 完整实现 | 90% 复用 `uniswap_v3` | 4-6 小时 |
| **P3** | Stable AMM | 0% (全新实现) | 8-12 小时 |

---

## 六、参考链接

- Velodrome Finance: https://velodrome.finance
- Velodrome GitHub: https://github.com/velodrome-finance/contracts
- Aerodrome App: https://aerodrome.finance
- BaseScan: https://basescan.org

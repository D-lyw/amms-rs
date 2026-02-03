# AMM 协议集成指南

本文档旨在指导开发者如何在 `amms` 模块中集成新的 AMM (Automated Market Maker) 协议。本系统采用了模块化设计，通过 `AutomatedMarketMaker` Trait 和枚举宏来统一管理不同的 DEX 协议。

## 1. 架构概览

在 `crates/amms/src/amms/` 目录下，每个协议通常作为一个独立的模块存在（例如 `uniswap_v3/`, `curve_ng/`）。

集成新协议的核心工作包括：
1.  **数据建模**：定义 Pool 的 Rust 结构体，映射链上存储状态。
2.  **核心逻辑**：实现 `AutomatedMarketMaker` Trait，包括状态同步、价格计算和模拟交易。
    *   **重要提示**: **Pool 数据状态的准确同步**和**本地模拟交易计算的绝对准确性**是集成工作的重中之重。任何微小的误差都可能导致套利失败。
3.  **工厂与发现**：实现 Factory 逻辑，用于批量获取 Pool 数据。
4.  **注册**：将新模块注册到系统的 `AMM` 枚举中。

## 2. 集成步骤 (Step-by-Step)

### Step 1: 创建模块结构
在 `crates/amms/src/amms/` 下创建新目录（如 `my_new_amm/`），并包含以下文件：
*   `mod.rs`: 核心 Pool 定义和 Trait 实现。
*   `factory.rs`: 负责 Pool 的发现和批量初始化。
*   `abi/`: (可选) 存放相关的 Solidity 接口或 JSON ABI。

**Action**: 在 `crates/amms/src/amms/mod.rs` 中声明新模块：
```rust
pub mod my_new_amm;
```

### Step 2: 定义 Pool 结构体
在 `mod.rs` 中定义 Pool 的结构体。它必须包含基本的识别信息和状态数据。

**Checklist**:
- [ ] `address`: Pool 的合约地址。
- [ ] `tokens`: Pool 包含的代币列表。
- [ ] `reserves/liquidity`: 核心流动性数据（如 `reserve0`, `reserve1`, `liquidity`, `sqrtPrice` 等）。
- [ ] `fee`: 交易费率。
- [ ] **[重要]** `spot_prices`: 用于缓存即时价格的 HashMap，避免重复计算。

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MyNewAmmPool {
    pub address: Address,
    pub tokens: Vec<Address>,
    pub reserves: Vec<U256>,
    pub fee: u32,
    #[serde(skip)]
    pub spot_prices: HashMap<(Address, Address), f64>, 
}
```

### Step 3: 实现 AutomatedMarketMaker Trait
这是最核心的部分。在 `mod.rs` 中实现 `AutomatedMarketMaker`。

**关键方法说明**:
1.  **`sync`**: 接收链上 Log，更新 Pool 状态。
    *   **TODO**: 监听 `Swap`, `Mint`, `Burn` 等事件。
    *   **TODO**: 更新完状态后，**必须**调用 `update_spot_prices()` 更新缓存。
2.  **`simulate_swap`**: 纯计算，给定输入算输出。
    *   **Hint**: 尽量复用链上的数学逻辑（可参考 Solidity 代码）。
    *   **Verification**: 完成本地逻辑后，**必须使用 Mainnet Fork 的方式进行充分验证**，确保本地计算结果（`amount_out`）与链上完全一致。
3.  **`calculate_price`**: 计算两个代币间的即时价格。
    *   **注意**: 这是一个计算方法，**不要**在这里读取缓存，以免死循环。
4.  **`spot_price`**: 覆盖默认实现，优先读取缓存。
    *   **Pattern**:
        ```rust
        fn spot_price(&self, base: Address) -> Result<f64, AMMError> {
            // 优先读缓存
            // ...
            // 缓存未命中则回退到 calculate_price
        }
        ```
5.  **`init`**: 初始化 Pool。
    *   通常调用 Factory 的批量获取接口来填充初始数据。
    *   **TODO**: 初始化结束前，**必须**调用 `update_spot_prices()`。

### Step 4: 实现 Price Caching (最佳实践)
为了保证套利引擎的性能，所有 AMM 必须实现价格缓存。

1.  **定义方法**: `pub(crate) fn update_spot_prices(&mut self)`。
2.  **逻辑**: 遍历所有 Token 对，调用 `calculate_price` 或 `simulate_swap` 计算价格并存入 `self.spot_prices`。
3.  **触发点**:
    *   Pool 初始化 (`init`, `init_batch`)。
    *   状态同步 (`sync`)。
    *   费率/配置变更 (如有)。

### Step 5: 实现 Factory
在 `factory.rs` 中实现批量获取 Pool 数据的逻辑。

**功能区分**:
1.  **`init_batch`**: 用于**全量初始化**。它需要获取池子的所有数据，包括**静态数据**（如 `token0`, `fee`）和**动态数据**（如 `reserves`, `liquidity`）。
    *   **RPC Warning**: 批量调用 RPC 时，数据量较大，**必须分批处理**（Chunking），以免超过 RPC 节点的限制（如 Gas Limit 或 Request Size）。
    *   **Best Practice**: 如果单个池子数据需要多次 RPC 调用才能获取完整，建议编写**批量查询合约**来优化（参考 `crates/amms/contracts` 目录下的合约）。
2.  **`sync_all_pools`**: 用于**快速重同步**。它只获取**动态数据**，用于在重启或重组后快速刷新状态。

**Checklist**:
- [ ] 实现 `init_batch`，处理好 Chunking 和 RPC 限制。
- [ ] 实现 `sync_all_pools`，仅更新易变状态。
- [ ] 如果协议支持 Factory 合约遍历，实现 `get_all_pools`。
- [ ] **[Critical]** 在 `init_batch` 中，填充完 Pool 数据后，显式调用 `pool.update_spot_prices()`。

### Step 6: 注册到系统
修改 `crates/amms/src/amms/amm.rs`。

1.  引入新模块的 Pool 结构体。
2.  在 `amm!` 宏的末尾添加你的 Pool 类型：
```rust
amm!(
    // ... 其他 Pools
    MyNewAmmPool
);
```
这将自动为你的 Pool 生成 `AMM` 枚举变体，并派发所有 Trait 方法。

## 3. TODO List & 验收清单

在提交代码前，请按照以下清单进行自查：

### 核心功能
- [ ] **结构体定义**: 包含所有必要的链上状态。
- [ ] **状态准确性**:
    - [ ] `init` 准确获取了初始状态。
    - [ ] `sync` 正确处理了所有状态变更事件。
- [ ] **模拟准确性 (Critical)**:
    - [ ] `simulate_swap` 通过了 **Mainnet Fork 测试**，结果与链上完全一致。
- [ ] **价格缓存**:
    - [ ] 实现了 `update_spot_prices`。
    - [ ] `init`, `sync`, `init_batch` 均正确触发了缓存更新。

### 工厂与 Batch
- [ ] **Factory 实现**:
    - [ ] `init_batch` 实现了全量数据获取，并正确**分批 (Chunking)** 避免 RPC 限制。
    - [ ] `sync_all_pools` 实现了轻量级动态数据更新。
    - [ ] 若需优化多重 RPC 调用，已实现并使用了自定义 Batch 合约（参考 `crates/amms/contracts`）。

### 集成与注册
- [ ] `mod.rs` 已公开模块。
- [ ] `amm.rs` 中的 `amm!` 宏已包含新 Pool。

### 测试
- [ ] **单元测试**: 针对数学计算的测试。
- [ ] **Fork 测试**: 针对实际链上数据的集成测试。

## 4. 常见问题 (FAQ)

**Q: 为什么需要自定义 Batch 合约?**
A: 某些 AMM (如 Curve NG) 的数据分散在多个合约方法中。如果在客户端通过多次 `eth_call` 获取，速度慢且容易触发 Rate Limit。编写一个 Solidity 合约将这些调用打包成一次 `staticcall` 能极大提升初始化速度。请参考 `crates/amms/contracts` 目录。

**Q: simulate_swap 和 calculate_price 有什么区别?**
A: `simulate_swap` 考虑了具体的输入金额和滑点，通常用于执行阶段的预估。`calculate_price` 计算的是无穷小额交换的边际价格（即 Spot Price），用于路径发现算法。

**Q: 如何调试 Sync 问题?**
A: 在 `sync` 方法中加入详细的 `tracing::info!` 日志，对比链上浏览器（如 Etherscan）的交易 Logs，检查 Event 解码和状态变更数值是否一致。

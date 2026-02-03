# amms-rs

Rust 库，用于与 EVM 链上的自动化做市商 (AMM) 进行状态同步和本地交易模拟。

> **致谢**: 本项目基于 [darkforestry/amms-rs](https://github.com/darkforestry/amms-rs) 进行深度的扩展与重构。

## 核心功能

*   **高性能模拟**: 纯 Rust 实现的 `simulate_swap` 逻辑，无需 EVM 模拟执行，极大提高 MEV/套利 策略的回测与实时计算性能。
*   **状态同步 (State Space)**: 基于 `eth_getLogs` 和 `eth_call` (Batch Request) 的高效状态维护机制，支持增量更新。
*   **全自动发现**: 内置 Factory 模块，支持从链上自动发现并索引新创建的池子。
*   **多链支持**: 架构设计支持 Ethereum Mainnet, Optimism, Arbitrum, Base, Polygon, Zksync Era 等各主流 EVM 兼容链。

## 支持的协议

本项目目前支持以下主流 AMM 协议及其变体：

### 核心 AMM
*   **Uniswap**:
    *   V2 (标准 CPMM)
    *   V3 (集中流动性 CLMM)
    *   V4 (Singleton, Hooks 架构)
*   **Balancer**:
    *   V2 (Weighted, Stable, ComposableStable)
    *   V3 (新的 Vault 架构)
*   **Curve**:
    *   Legacy (StableSwap, CryptoSwap/V2)
    *   NG (New Generation: StableSwap NG, TwoCrypto NG, TriCrypto NG)
*   **Ekubo**:
    *   V2 (Starknet 风格的做市商，Base 链支持)

### 其他协议
*   **PancakeSwap**: V2, V3, Infinity (Stable)
*   **SushiSwap**: V2
*   **Fluid DEX**: Liquidity Layer 架构
*   **ERC4626**: Tokenized Vaults 标准支持

## 架构概览

### 1. AMM 抽象
所有池子通过 `AutomatedMarketMaker` trait 统一抽象：
```rust
pub trait AutomatedMarketMaker: Send + Sync {
    fn address(&self) -> Address;
    fn tokens(&self) -> Vec<Address>;
    fn simulate_swap(&self, base_token: Address, quote_token: Address, amount_in: U256) -> Result<U256, AMMError>;
    // ...
}
```

### 2. State Space (状态空间)
`StateSpace` 是核心的状态容器，负责：
*   监听并处理新区块事件。
*   批量同步池子状态（Reserves, Ticks, Fees 等）。
*   提供统一的查询接口供策略引擎使用。
*   高效的批量请求 (Batch Requests) 均通过临时合约 (Ephemeral Contract) 或 Multicall 实现。

### 3. Pool Discovery (自动发现)
每个协议都实现了 `AutomatedMarketMakerFactory` trait，用于扫描 Factory 合约的 `PoolCreated` 事件自动加载新池子。


---

*Forked and extended from [darkforestry/amms-rs](https://github.com/darkforestry/amms-rs).*

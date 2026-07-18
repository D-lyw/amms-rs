# amms-rs

A high-performance Rust library for state synchronization, local swap simulation, and realtime monitoring of automated market makers (AMMs) across EVM-compatible chains. Optimized for MEV, arbitrage, and DeFi aggregation use cases.

Implements fully-replicated swap calculation logic for each supported protocol. Pool state is maintained in realtime via on-chain event-driven updates, with most AMM types producing swap simulation results that match on-chain execution exactly (zero deviation).

> **Attribution**: Forked and extensively extended from [darkforestry/amms-rs](https://github.com/darkforestry/amms-rs).

---

## Key Features

- **Pure-Rust Swap Simulation** — `simulate_swap` logic implemented entirely in Rust without EVM execution, enabling sub-millisecond arbitrage path discovery and backtesting.
- **Realtime Event-Driven Sync** — Chain-optimized log subscriptions: Base `pendingLogs` (flashblocks-aware), Arbitrum Nitro Sequencer Feed, XLayer Flashblocks, and generic newHeads + logsBloom prefilter + `getLogs` for all other chains.
- **Batch State Initialization** — Ephemeral Solidity batch contracts fetch pool static/runtime data efficiently (slot0, ticks, liquidity, fees, rates, reserves) in a single RPC call per variant.
- **Periodic Drift Detection** — Silent background probe compares local state against chain for V2, V3, V4, Slipstream, and Curve NG pools, enqueuing targeted resyncs when drift is detected.
- **Maintenance Coverage** — Oldest-first periodic full resync sweeps to catch non-event-driven parameter changes (e.g., governance fee updates, oracle drift, rebasing token accrual).
- **Pending Sync Queue** — Priority-aware queue with canonical-head gating, exponential backoff retries, and in-flight deduplication.
- **Snapshot Persistence** — Serialize/deserialize the entire state space via `serde_json` for fast cold-start recovery.
- **Multi-Chain** — Ethereum Mainnet, Arbitrum, Base, Optimism, Polygon, BSC, XLayer, and any EVM-compatible L2.

---

## Supported Protocols

| Protocol | Variants | Description |
|---|---|---|
| **Uniswap** | V2, V3, V4 | CPMM, CLMM, Singleton+Hooks |
| **Balancer** | V2 (Weighted/Stable/ComposableStable), V3 | Multi-token pools with rate providers |
| **Curve** | Legacy (StableSwap, CryptoSwap), NG (StableSwap NG, TwoCrypto NG, TriCrypto NG) | Stableswap and CryptoSwap invariants |
| **Ekubo** | V2 (Starknet-style singleton CLMM) | CLMM with custom fee model |
| **PancakeSwap** | V2, V3, Infinity (V4-compatible) | BSC-native fork ecosystem |
| **SushiSwap** | V2 | Uniswap V2 fork |
| **Aerodrome** | V2, Slipstream (CL with dynamic fees) | Base-native with ve(3,3) tokenomics |
| **Algebra Integral** | CLMM with adaptive/dynamic fees | Plugin-based fee manager |
| **Fluid DEX** | Smart Collateral/Debt + Liquidity Layer | Real/virtual reserves, utilization limits, center price drift |
| **ERC4626** | Tokenized Vault standard | Share/asset conversion with yield accrual |
| **Rocket Pool** | rETH/ETH converter | Deposit pool + redemption model via Multicall3 |
| **SKY Protocol** | DAI/USDS/USDC converters | Fixed-rate (DaiUsds 1:1) and fee-based (LitePSM) |
| **Pendle** | PT/Underlying via SY intermediate | Time-decaying yield tokenization with implied rate |
| **Caliber propAMM** | Ladder-based market maker | Piecewise linear pricing via batchQuote (Makina Protocol) |

---

## Architecture

### 1. AMM Abstraction

All pools implement the `AutomatedMarketMaker` trait, which provides a unified interface:

```rust
pub trait AutomatedMarketMaker: Send + Sync + 'static {
    fn address(&self) -> Address;
    fn tokens(&self) -> Vec<Address>;

    // Swap simulation (read-only)
    fn simulate_swap(&self, base_token: Address, quote_token: Address, amount_in: U256) -> Result<U256, AMMError>;
    // Stateful swap simulation
    fn simulate_swap_mut(&mut self, base_token: Address, quote_token: Address, amount_in: U256) -> Result<U256, AMMError>;
    // Exact-output swap simulation
    fn simulate_swap_exact_out(&self, base_token: Address, quote_token: Address, amount_out: U256) -> Result<U256, AMMError>;

    // Price queries
    fn calculate_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError>;
    fn spot_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError>;

    // Liquidity check
    fn has_sufficient_liquidity(&self) -> bool;

    // Event-driven sync
    fn sync_events(&self) -> Vec<B256>;
    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError>;

    // State initialization & periodic refresh
    async fn init<N, P>(self, block_number: BlockId, provider: P) -> Result<Self, AMMError>;
    async fn update<N, P>(&mut self, provider: P) -> Result<(), AMMError>;
}
```

All pool variants are wrapped in a single `AMM` enum via a declarative macro for type-safe dispatch.

### 2. State Space

`StateSpace` is the core state container — an `Arc<RwLock<HashMap<Address, Arc<AMM>>>>` — managed by `StateSpaceManager`:

| Component | Role |
|---|---|
| **Realtime subscription** | Chain-specific log sources (Base pendingLogs, Arbitrum feed, XLayer flashblocks, or generic newHeads+getLogs) streaming `Sync`/`Swap` events into the state lock. |
| **Pending sync worker** | Drains the priority queue of resync/async-update tasks with canonical-head gating, exponential backoff, and in-flight dedup. |
| **Drift probe** | Round-robin background task comparing local state snapshots against on-chain readings; enqueues corrections on mismatch. |
| **Maintenance coverage** | Oldest-first periodic full resync to catch non-event-driven drift (governance changes, rate accrual). |
| **Non-event sync services** | Periodic background tasks refreshing parameters that change without emitting events: Balancer rates/fees, Curve rates/price_scale, Fluid DEX limits/centerPrice, Slipstream dynamic fee config, Rocket Pool redemption state, Pendle `sy_exchange_rate`, Caliber propAMM ladders. |
| **Snapshot** | JSON serialization/deserialization of the entire state space for persistence across restarts. |
| **Hooks** | `StateHook<T>` callbacks invoked on every state change with affected pool addresses. |

### 3. Builder API

The `StateSpaceBuilder` provides a fluent interface for configuring and launching the state space:

```rust
use amms::state_space::{StateSpaceBuilder, RealtimeSyncSource};

let manager = StateSpaceBuilder::new(provider)
    .with_amms(static_amms)
    .with_filters(vec![blacklist.into()])
    .with_non_event_sync_interval(Duration::from_secs(300))
    .with_maintenance_interval(Duration::from_secs(3600))
    .with_snapshot_path(PathBuf::from("state.json"))
    .with_realtime_ws_endpoints(vec!["wss://...".into()])
    .sync()
    .await?;

let mut stream = manager.subscribe().await?;
while let Some(Ok(affected_pools)) = stream.next().await {
    // React to state changes
}
```

A convenience `sync!` macro is also available for simple setups.

---

## Chain-Specific Optimizations

| Chain | Realtime Source | Notes |
|---|---|---|
| **Ethereum Mainnet** | newHeads + logsBloom + getLogs | 50-block backfill windows |
| **Base** | `pendingLogs` subscription (flashblocks-aware) | 100-block windows; requires WebSocket endpoint supporting `eth_subscribe` with `pendingLogs` |
| **Arbitrum** | Nitro Sequencer Feed | 200-block windows; WebSocket endpoint auto-detected |
| **XLayer** | Flashblocks raw WebSocket stream | 100-block windows; JSON transport (no Brotli) |
| **Others** | newHeads + logsBloom + getLogs | 50-block windows |

Automatic chain detection via `chain_id` selects the optimal realtime source; manual override is available via `RealtimeSyncSource`.

---

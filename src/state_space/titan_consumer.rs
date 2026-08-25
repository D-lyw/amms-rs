//! Titan pAMM 流消费者（M4）。
//!
//! 将 `titan_stream::subscribe_overrides_stream` 的 overrides 快照应用到
//! StateSpace 中的 Fermi pools，并触发下游变化检测；同时周期性用链上
//! `eth_getStorageAt` 校准 registry lane（断流/漏消息兜底）。
//!
//! 语义（详见 `docs/fermi_prop_internal.md` §6.4）：
//! - lane 报价：以 Titan 流为准（链上 latest 过时），slot 单调守卫在上游，
//!   `apply_titan_lane` 的 update_timestamp 守卫兜底；
//! - vault ERC20 余额：不来自 Titan 流（快照 balance 为原生 ETH），
//!   由链上事件账本 + 本任务 reconcile 校准；
//! - 变化检测：快照应用后 `hooks.notify(affected)`（与 realtime 路径一致）。

use std::sync::Arc;
use std::time::Duration;

use alloy::eips::BlockId;
use alloy::network::Network;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use futures::StreamExt;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::amms::amm::AMM;
use crate::amms::fermi_prop::titan::apply_titan_snapshot;
use crate::amms::fermi_prop::{fermi_registry_lane_slot, FermiLane};

use super::hooks::HookRegistry;
use super::titan_stream::{
    subscribe_overrides_stream, TitanOverridesSnapshot, TitanPammStreamConfig,
    TitanQuoteStreamConfig,
};
use super::StateSpace;

/// 该 AMM 变体是否需要 Titan 实时流（链上看不到的高频报价）。
///
/// 目前：Fermi（Ethereum 主网 PropAMM）。未来接入 Kipseli/bopAMM 等
/// Titan PropAMM venue 时在此扩展。
pub(crate) fn pool_requires_titan_stream(amm: &AMM) -> bool {
    matches!(amm, AMM::FermiPropPool(_))
}

/// 收集 StateSpace 中全部 Fermi pool 的 virtual_address。
async fn fermi_pool_addresses(state: &Arc<RwLock<StateSpace>>) -> Vec<Address> {
    let guard = state.read().await;
    guard
        .state
        .iter()
        .filter_map(|(addr, amm)| match amm.as_ref() {
            AMM::FermiPropPool(_) => Some(*addr),
            _ => None,
        })
        .collect()
}

/// 将单个 overrides 快照应用到全部 Fermi pools，返回实际变化的地址。
async fn apply_snapshot_to_state(
    state: &Arc<RwLock<StateSpace>>,
    snapshot: &Arc<TitanOverridesSnapshot>,
) -> Vec<Address> {
    let addresses = fermi_pool_addresses(state).await;
    if addresses.is_empty() {
        return Vec::new();
    }

    let mut affected = Vec::new();
    let mut lanes = 0usize;
    {
        let mut guard = state.write().await;
        for addr in addresses {
            let Some(AMM::FermiPropPool(pool)) = guard.get_mut_cow(&addr) else {
                continue;
            };
            let outcome = apply_titan_snapshot(pool, snapshot);
            lanes += outcome.lanes_applied;
            affected.extend(outcome.affected_pools);
        }
    }

    if !affected.is_empty() {
        debug!(
            slot = ?snapshot.slot,
            block = ?snapshot.block_number,
            lanes,
            affected = affected.len(),
            "titan overrides applied to fermi pools"
        );
    }
    affected
}

/// 链上校准：读 registry lane 槽位，链上比本地新则刷新（断流/漏消息兜底）。
async fn reconcile_lanes<N, P>(state: &Arc<RwLock<StateSpace>>, provider: &P)
where
    N: Network,
    P: Provider<N>,
{
    let pools: Vec<(Address, Address, Address, Address, Address)> = {
        let guard = state.read().await;
        guard
            .state
            .iter()
            .filter_map(|(addr, amm)| match amm.as_ref() {
                AMM::FermiPropPool(p) => Some((
                    *addr,
                    p.engine_address,
                    p.registry_address,
                    p.token_a,
                    p.token_b,
                )),
                _ => None,
            })
            .collect()
    };
    if pools.is_empty() {
        return;
    }

    for (addr, engine, registry, token_a, token_b) in pools {
        let slot = U256::from_be_bytes(fermi_registry_lane_slot(engine, token_a, token_b).0);
        let word = match provider
            .get_storage_at(registry, slot)
            .block_id(BlockId::latest())
            .await
        {
            Ok(word) => word,
            Err(e) => {
                warn!(
                    target: "state_space::titan_consumer",
                    pool = %addr,
                    error = %e,
                    "titan reconcile: registry lane storage read failed"
                );
                continue;
            }
        };
        let Some(lane) = FermiLane::from_slot_word(word) else {
            continue;
        };

        let mut guard = state.write().await;
        let Some(AMM::FermiPropPool(pool)) = guard.get_mut_cow(&addr) else {
            continue;
        };
        if lane.update_timestamp > pool.lane.update_timestamp {
            // 链上比本地新：流可能断线/漏消息，以链上为准刷新。
            if pool.apply_titan_lane(lane) {
                info!(
                    target: "state_space::titan_consumer",
                    pool = %addr,
                    local_ts = pool.lane.update_timestamp,
                    chain_ts = lane.update_timestamp,
                    "titan reconcile: onchain lane newer, refreshed"
                );
            }
        } else if lane.update_timestamp < pool.lane.update_timestamp {
            // 流比链上新：正常（报价走私有流不上链），仅保留调试日志。
            debug!(
                target: "state_space::titan_consumer",
                pool = %addr,
                local_ts = pool.lane.update_timestamp,
                chain_ts = lane.update_timestamp,
                "titan reconcile: stream ahead of onchain (expected)"
            );
        }
    }
}

/// Titan pAMM 流消费者主循环（挂载于 `ensure_background_tasks`）。
///
/// `tokio::select!` 双路：
/// - WS overrides 快照 → `apply_snapshot_to_state` → `hooks.notify(affected)`；
/// - 周期 reconcile（链上校准），间隔由 `TitanPammStreamConfig::reconcile_interval` 决定。
pub async fn run_titan_pamm_stream_task<N, P>(
    config: TitanPammStreamConfig,
    state: Arc<RwLock<StateSpace>>,
    hooks: HookRegistry<Vec<Address>>,
    provider: P,
) where
    N: Network,
    P: Provider<N> + Clone + 'static,
{
    let stream_config = TitanQuoteStreamConfig {
        ws_url: config.ws_url.clone(),
        rpc_url: config.rpc_url.clone(),
        idle_timeout: config.idle_timeout,
        reconnect_delay: config.reconnect_delay,
    };
    let mut stream = Box::pin(subscribe_overrides_stream(stream_config));
    let mut reconcile = tokio::time::interval(config.reconcile_interval);
    reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 首个 tick 立即触发一次校准（冷启动对齐）。
    reconcile.tick().await;

    loop {
        tokio::select! {
            item = stream.next() => {
                match item {
                    Some(Ok(snapshot)) => {
                        let affected = apply_snapshot_to_state(&state, &snapshot).await;
                        if !affected.is_empty() {
                            hooks.notify(&affected).await;
                        }
                    }
                    Some(Err(e)) => {
                        warn!(target: "state_space::titan_consumer", error = ?e, "titan stream item error");
                    }
                    None => {
                        warn!(target: "state_space::titan_consumer", "titan stream ended, waiting before restart");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
            _ = reconcile.tick() => {
                reconcile_lanes::<N, P>(&state, &provider).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, U256};
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;

    use crate::amms::fermi_prop::factory::FermiPropFactory;
    use crate::amms::fermi_prop::types::IFermiEngine::TokenPair;
    use crate::amms::fermi_prop::FermiPropPool;

    fn weth() -> Address {
        address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2")
    }
    fn usdc() -> Address {
        address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48")
    }

    fn pool() -> FermiPropPool {
        let mut p = FermiPropFactory::new_default(1, 1).skeleton(
            TokenPair {
                token0: usdc(),
                token1: weth(),
                active: true,
            },
            1,
        );
        p.token_a = weth();
        p.token_b = usdc();
        p
    }

    fn state_with_pool(p: FermiPropPool) -> Arc<RwLock<StateSpace>> {
        let mut state = HashMap::new();
        state.insert(p.virtual_address, Arc::new(AMM::FermiPropPool(p)));
        Arc::new(RwLock::new(StateSpace {
            state,
            realtime_head: Arc::new(AtomicU64::new(0)),
            canonical_head: Arc::new(AtomicU64::new(0)),
            chain_id: 1,
        }))
    }

    fn real_snapshot(price_e8: u64, ts: u32) -> Arc<TitanOverridesSnapshot> {
        let slot_key = crate::amms::fermi_prop::fermi_registry_lane_slot(
            pool().engine_address,
            weth(),
            usdc(),
        );
        let word = (U256::from(ts) << U256::from(224))
            | (U256::from(0x01u64) << U256::from(216))
            | U256::from(price_e8);
        let payload = json!({
            "slot": 15058570,
            "blockNumber": 25821077,
            "0xb1076fe3ab5e28005c7c323bac5ac06a680d452e": {
                "stateOverride": {
                    "0xda7afeed01fe625cf15d187a19f94b45f00b8c5f": {
                        "stateDiff": {
                            slot_key.to_string(): format!("{word:#x}")
                        }
                    }
                }
            }
        });
        Arc::new(TitanOverridesSnapshot::parse(&payload).unwrap())
    }

    #[test]
    fn requires_titan_stream_only_for_fermi() {
        use crate::amms::uniswap_v2::UniswapV2Pool;
        let fermi = AMM::FermiPropPool(pool());
        let other = AMM::UniswapV2Pool(UniswapV2Pool::default());
        assert!(pool_requires_titan_stream(&fermi));
        assert!(!pool_requires_titan_stream(&other));
    }

    #[tokio::test]
    async fn applies_snapshot_and_reports_affected() {
        let state = state_with_pool(pool());
        let affected =
            apply_snapshot_to_state(&state, &real_snapshot(242374096470, 0x6a8ae2b7)).await;
        assert_eq!(affected.len(), 1);
        let guard = state.read().await;
        let pool_addr = affected[0];
        let AMM::FermiPropPool(p) = guard.get(&pool_addr).unwrap() else {
            panic!("not fermi");
        };
        assert_eq!(p.lane.fair_price_e8, 242374096470);
        assert_eq!(p.lane.update_timestamp, 0x6a8ae2b7);
    }

    #[tokio::test]
    async fn same_snapshot_is_idempotent_no_affected() {
        let state = state_with_pool(pool());
        let snapshot = real_snapshot(242374096470, 0x6a8ae2b7);
        let first = apply_snapshot_to_state(&state, &snapshot).await;
        assert_eq!(first.len(), 1);
        let second = apply_snapshot_to_state(&state, &snapshot).await;
        assert!(second.is_empty());
    }

    #[tokio::test]
    async fn no_fermi_pools_no_affected() {
        let state = Arc::new(RwLock::new(StateSpace {
            state: HashMap::new(),
            realtime_head: Arc::new(AtomicU64::new(0)),
            canonical_head: Arc::new(AtomicU64::new(0)),
            chain_id: 1,
        }));
        let affected =
            apply_snapshot_to_state(&state, &real_snapshot(242374096470, 0x6a8ae2b7)).await;
        assert!(affected.is_empty());
    }
}

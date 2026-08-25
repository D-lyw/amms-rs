//! Fermi Titan 快照应用器(M4)。
//!
//! 将 `state_space::titan_stream` 的 overrides 快照应用到 `FermiPropPool`：
//!
//! - **lane**：用 M3.1 槽位公式（`fermi_registry_lane_slot`）计算本 pair 的
//!   registry 存储槽位，在快照 `stateDiff` 中查找并解码 `FermiLane`，
//!   经 `apply_titan_lane` 版本守卫（update_timestamp 不回卷、同值不触发）更新；
//! - **余额**：Titan 快照账户的 `balance` 是**原生 ETH 余额**（stateOverride 语义），
//!   不是 ERC20 金库余额；ERC20 余额需从 token 合约 `stateDiff`（balance 槽位）解码，
//!   Titan 实测快照不含。故 vault ERC20 余额**不来自 Titan 流**，完全依赖
//!   链上事件账本（init balanceOf + Transfer/Swapped 对账）+ reconcile
//!   `eth_getStorageAt` 校准（见 M4.4）。
//! - **输出**：实际变化的 pool `virtual_address` 列表，供 state_space 触发下游
//!   变化检测（复用 HookRegistry affected pools 通知）。
//!
//! 版本守卫双层：上游 `accept_snapshot` 的 slot 单调 + 此处 lane update_timestamp
//! 不回卷；同批更新共享 timestamp、价格可能微调——timestamp 相等即接受。

use std::collections::HashMap;

use alloy::primitives::Address;

use crate::state_space::titan_stream::{TitanAccountOverride, TitanOverridesSnapshot};

use super::{fermi_registry_lane_slot, FermiLane, FermiPropPool};

/// 单次快照应用到单池的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FermiTitanApplyOutcome {
    /// 实际发生变化的 pool 地址（virtual_address；无变化为空）。
    pub affected_pools: Vec<Address>,
    /// 应用成功的 lane 更新数（0 或 1，单池单 lane）。
    pub lanes_applied: usize,
    /// 快照 beacon slot（透传，便于日志）。
    pub slot: Option<u64>,
}

/// 收集快照中本部署（swapper/wrapper venue）的账户 override，按账户合并。
///
/// Fermi 在流中实测出现两个 key（swapper 与 wrapper），逐条轮换；两者携带的
/// registry `stateDiff` 内容一致（或部分重叠），按账户地址合并、槽位级后到覆盖。
fn collect_venue_overrides(
    pool: &FermiPropPool,
    snapshot: &TitanOverridesSnapshot,
) -> HashMap<Address, TitanAccountOverride> {
    let mut merged: HashMap<Address, TitanAccountOverride> = HashMap::new();
    for (venue, pamm) in &snapshot.per_pamm {
        if *venue != pool.swapper_address && *venue != pool.wrapper_address {
            continue;
        }
        for (account, override_) in &pamm.accounts {
            let entry = merged.entry(*account).or_default();
            if let Some(balance) = override_.balance {
                entry.balance = Some(balance);
            }
            if let Some(nonce) = override_.nonce {
                entry.nonce = Some(nonce);
            }
            for (slot, value) in &override_.state_diff {
                entry.state_diff.insert(*slot, *value);
            }
        }
    }
    merged
}

/// 将 Titan overrides 快照应用到单个 Fermi pool。
///
/// - 快照不含本部署（swapper/wrapper）条目 → 无操作；
/// - registry `stateDiff` 不含本 pair 槽位 → 该 pair 无新报价，跳过；
/// - lane 经 `apply_titan_lane` 守卫后实际变化 → 计入 `affected_pools`。
pub fn apply_titan_snapshot(
    pool: &mut FermiPropPool,
    snapshot: &TitanOverridesSnapshot,
) -> FermiTitanApplyOutcome {
    let mut outcome = FermiTitanApplyOutcome {
        slot: snapshot.slot,
        ..Default::default()
    };

    let merged = collect_venue_overrides(pool, snapshot);
    if merged.is_empty() {
        return outcome;
    }

    // 1. lane：本 pair registry 槽位 → 解码 → 版本守卫应用。
    let slot_key = fermi_registry_lane_slot(pool.engine_address, pool.token_a, pool.token_b);
    if let Some(registry) = merged.get(&pool.registry_address) {
        if let Some(word) = registry.state_diff.get(&slot_key) {
            if let Some(lane) = FermiLane::from_slot_word(*word) {
                if pool.apply_titan_lane(lane) {
                    outcome.lanes_applied += 1;
                    outcome.affected_pools.push(pool.virtual_address);
                }
            }
        }
    }

    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, b256, U256};
    use serde_json::json;
    use std::str::FromStr;

    use crate::state_space::titan_stream::TitanOverridesSnapshot;

    fn weth() -> Address {
        address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2")
    }
    fn usdc() -> Address {
        address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48")
    }

    /// 2026-08-24 实测 WETH/USDC lane 槽位与打包值（Titan 快照）。
    fn real_slot_value(price_e8: u64, ts: u32) -> U256 {
        (U256::from(ts) << U256::from(224))
            | (U256::from(0x01u64) << U256::from(216))
            | U256::from(price_e8)
    }

    fn pool_fixture() -> FermiPropPool {
        let mut pool = crate::amms::fermi_prop::factory::FermiPropFactory::new_default(1, 1)
            .skeleton(
                crate::amms::fermi_prop::types::IFermiEngine::TokenPair {
                    token0: usdc(),
                    token1: weth(),
                    active: true,
                },
                1,
            );
        // 方向：WETH 为 base、USDC 为 quote（与链上报价方向一致）。
        pool.token_a = weth();
        pool.token_b = usdc();
        pool.lane_index = crate::amms::fermi_prop::fermi_lane_index(pool.token_a, pool.token_b);
        pool
    }

    fn snapshot_with_lane(price_e8: u64, ts: u32) -> TitanOverridesSnapshot {
        let slot_key = crate::amms::fermi_prop::fermi_registry_lane_slot(
            pool_fixture().engine_address,
            weth(),
            usdc(),
        );
        let payload = json!({
            "slot": 15058570,
            "blockNumber": 25821077,
            "0xb1076fe3ab5e28005c7c323bac5ac06a680d452e": {
                "stateOverride": {
                    "0xda7afeed01fe625cf15d187a19f94b45f00b8c5f": {
                        "stateDiff": {
                            slot_key.to_string(): format!("{:#x}", real_slot_value(price_e8, ts))
                        }
                    }
                }
            }
        });
        TitanOverridesSnapshot::parse(&payload).unwrap()
    }

    #[test]
    fn applies_lane_from_real_snapshot() {
        let mut pool = pool_fixture();
        let price_e8 = 242374096470u64;
        let ts = 0x6a8ae2b7;
        let outcome = apply_titan_snapshot(&mut pool, &snapshot_with_lane(price_e8, ts));
        assert_eq!(outcome.lanes_applied, 1);
        assert_eq!(outcome.affected_pools, vec![pool.virtual_address]);
        assert_eq!(pool.lane.fair_price_e8, price_e8);
        assert_eq!(pool.lane.update_timestamp, ts);
        assert_eq!(pool.lane.flag, 1);
    }

    #[test]
    fn same_value_is_idempotent() {
        let mut pool = pool_fixture();
        let price_e8 = 242374096470u64;
        let ts = 0x6a8ae2b7;
        let snapshot = snapshot_with_lane(price_e8, ts);
        let first = apply_titan_snapshot(&mut pool, &snapshot);
        assert_eq!(first.lanes_applied, 1);
        let second = apply_titan_snapshot(&mut pool, &snapshot);
        assert_eq!(second.lanes_applied, 0);
        assert!(second.affected_pools.is_empty());
    }

    #[test]
    fn newer_timestamp_updates_same_slot() {
        let mut pool = pool_fixture();
        apply_titan_snapshot(&mut pool, &snapshot_with_lane(242374096470, 0x6a8ae2b7));
        let outcome =
            apply_titan_snapshot(&mut pool, &snapshot_with_lane(242375000000, 0x6a8ae2b8));
        assert_eq!(outcome.lanes_applied, 1);
        assert_eq!(pool.lane.fair_price_e8, 242375000000);
    }

    #[test]
    fn older_timestamp_rejected() {
        let mut pool = pool_fixture();
        apply_titan_snapshot(&mut pool, &snapshot_with_lane(242375000000, 0x6a8ae2b8));
        let outcome =
            apply_titan_snapshot(&mut pool, &snapshot_with_lane(242374096470, 0x6a8ae2b7));
        assert_eq!(outcome.lanes_applied, 0);
        assert!(outcome.affected_pools.is_empty());
        assert_eq!(pool.lane.fair_price_e8, 242375000000);
    }

    #[test]
    fn wrapper_venue_key_also_drives_updates() {
        // wrapper key（与 swapper 交替出现）携带相同 stateDiff 时同样生效且幂等。
        let slot_key = crate::amms::fermi_prop::fermi_registry_lane_slot(
            pool_fixture().engine_address,
            weth(),
            usdc(),
        );
        let payload = json!({
            "slot": 15058570,
            "blockNumber": 25821077,
            "0x5979458912f80b96d30d4220af8e2e4925a33320": {
                "stateOverride": {
                    "0xda7afeed01fe625cf15d187a19f94b45f00b8c5f": {
                        "stateDiff": {
                            slot_key.to_string(): format!("{:#x}", real_slot_value(242374096470, 0x6a8ae2b7))
                        }
                    }
                }
            }
        });
        let snapshot = TitanOverridesSnapshot::parse(&payload).unwrap();
        let mut pool = pool_fixture();
        let outcome = apply_titan_snapshot(&mut pool, &snapshot);
        assert_eq!(outcome.lanes_applied, 1);
        assert_eq!(pool.lane.fair_price_e8, 242374096470);
    }

    #[test]
    fn missing_pair_slot_is_skipped() {
        // 快照只含其它 pair 的槽位 → 本 pair 无更新、不视为 affected。
        let other_slot = b256!("1111111111111111111111111111111111111111111111111111111111111111");
        let payload = json!({
            "slot": 15058570,
            "0xb1076fe3ab5e28005c7c323bac5ac06a680d452e": {
                "stateOverride": {
                    "0xda7afeed01fe625cf15d187a19f94b45f00b8c5f": {
                        "stateDiff": {
                            other_slot.to_string(): "0x6a8ae2b70100000000000000000000000000000000000000000000386e9f3656"
                        }
                    }
                }
            }
        });
        let snapshot = TitanOverridesSnapshot::parse(&payload).unwrap();
        let mut pool = pool_fixture();
        let outcome = apply_titan_snapshot(&mut pool, &snapshot);
        assert_eq!(outcome.lanes_applied, 0);
        assert!(outcome.affected_pools.is_empty());
        assert_eq!(pool.lane.fair_price_e8, 0);
    }

    #[test]
    fn unknown_venue_is_ignored() {
        // 其它 venue（如 bopAMM）的 stateDiff 不影响 Fermi pool。
        let payload = json!({
            "slot": 15058570,
            "0xb0999914b3de1be58ef2416af09bd2e7f8aad03c": {
                "stateOverride": {
                    "0xda7afeed01fe625cf15d187a19f94b45f00b8c5f": {
                        "stateDiff": {
                            "0x1111111111111111111111111111111111111111111111111111111111111111": "0x1"
                        }
                    }
                }
            }
        });
        let snapshot = TitanOverridesSnapshot::parse(&payload).unwrap();
        let mut pool = pool_fixture();
        let outcome = apply_titan_snapshot(&mut pool, &snapshot);
        assert_eq!(outcome.lanes_applied, 0);
        assert!(outcome.affected_pools.is_empty());
    }
}

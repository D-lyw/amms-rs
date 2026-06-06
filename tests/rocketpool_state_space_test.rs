//! Rocket Pool full state-space sync integration test.
//!
//! Exercises the production `StateSpaceBuilder::new(provider)
//! .with_amms(amms)
//! .sync()` pipeline to ensure RocketPoolConverter initialises
//! correctly through the standard AMM initialisation path.

mod common;

use alloy::{
    primitives::U256,
    providers::ProviderBuilder,
};
use amms::amms::{
    amm::AMM,
    rocketpool::{addresses, RocketPoolConverter, NATIVE_ETH_PLACEHOLDER},
};
use amms::state_space::{StateSpaceBuilder, StateSpaceManager};
use eyre::Result;

#[tokio::test]
async fn test_rocketpool_via_state_space_builder() -> Result<()> {
    let rpc_url = crate::common::rpc::provider_url_required()?;
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    let converter = RocketPoolConverter::new(
        addresses::ROCKET_DEPOSIT_POOL,
        addresses::RETH,
        addresses::ROCKET_NETWORK_BALANCES,
    );

    let manager: StateSpaceManager<_, _> = StateSpaceBuilder::new(provider)
        .with_amms(vec![AMM::RocketPoolConverter(converter)])
        .with_non_event_sync_interval(std::time::Duration::from_secs(370))
        .with_curve_sync_interval(std::time::Duration::from_secs(270))
        .with_drift_probe_interval(std::time::Duration::from_secs(210))
        .with_maintenance_interval(std::time::Duration::from_secs(620))
        .sync()
        .await?;

    // Read back the converter from state
    let state = manager.state.read().await;
    let entry = state
        .state
        .get(&addresses::ROCKET_DEPOSIT_POOL)
        .expect("RocketPoolConverter should be in state after sync");

    match entry.as_ref() {
        AMM::RocketPoolConverter(converter) => {
            assert_eq!(converter.token_0, addresses::RETH);
            assert_eq!(converter.token_1, NATIVE_ETH_PLACEHOLDER);
            assert!(
                converter.total_eth_balance > U256::ZERO,
                "total_eth_balance should be > 0"
            );
            assert!(
                converter.reth_supply > U256::ZERO,
                "reth_supply should be > 0"
            );
            assert!(
                converter.total_collateral > U256::ZERO
                    || converter.maximum_deposit_amount > U256::ZERO,
                "at least one liquidity source should be non-zero"
            );
            assert!(converter.exchange_rate > U256::ZERO);
            assert!(converter.token_0_price > 0.0);
            assert!(converter.token_1_price > 0.0);
            assert!(converter.last_synced_block > 0);
        }
        other => panic!(
            "Expected RocketPoolConverter, got {:?}",
            std::mem::discriminant(other)
        ),
    }

    Ok(())
}

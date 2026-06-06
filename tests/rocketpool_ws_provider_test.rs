//! Rocket Pool WebSocket provider integration test.
//!
//! Isolates the `connect_ws(...)` provider path used by downstream systems and
//! verifies that RocketPoolConverter can initialize and fetch its required
//! state over WebSocket RPC.

mod common;

use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use amms::amms::{
    amm::AutomatedMarketMaker,
    rocketpool::{addresses, RocketPoolConverter, NATIVE_ETH_PLACEHOLDER},
};
use eyre::{eyre, Result};

fn websocket_url_required() -> Result<String> {
    dotenv::dotenv().ok();

    if let Ok(url) = std::env::var("ETHEREUM_WS_PROVIDER") {
        return Ok(url);
    }
    if let Ok(url) = std::env::var("ETHEREUM_WS_URL") {
        return Ok(url);
    }

    let fallback = crate::common::rpc::provider_url_required()?;
    if fallback.starts_with("wss://") || fallback.starts_with("ws://") {
        return Ok(fallback);
    }
    if let Some(stripped) = fallback.strip_prefix("https://") {
        return Ok(format!("wss://{stripped}"));
    }
    if let Some(stripped) = fallback.strip_prefix("http://") {
        return Ok(format!("ws://{stripped}"));
    }

    Err(eyre!(
        "Could not derive WebSocket URL from ETHEREUM_WS_PROVIDER / ETHEREUM_WS_URL / ETHEREUM_PROVIDER / ETHEREUM_RPC_URL"
    ))
}

#[tokio::test]
async fn test_rocketpool_init_over_websocket() -> Result<()> {
    let ws_url = websocket_url_required()?;
    let provider = ProviderBuilder::new()
        .connect_ws(WsConnect::new(ws_url.clone()))
        .await?;
    let provider = std::sync::Arc::new(provider);

    let block_number = provider.get_block_number().await?;
    let block_id = alloy::eips::BlockId::number(block_number);

    let converter = RocketPoolConverter::new(
        addresses::ROCKET_DEPOSIT_POOL,
        addresses::RETH,
        addresses::ROCKET_NETWORK_BALANCES,
    )
    .init(block_id, provider)
    .await?;

    assert_eq!(converter.token_0, addresses::RETH);
    assert_eq!(converter.token_1, NATIVE_ETH_PLACEHOLDER);
    assert_eq!(converter.address, addresses::ROCKET_DEPOSIT_POOL);
    assert_eq!(
        converter.network_balances_address,
        addresses::ROCKET_NETWORK_BALANCES
    );

    assert!(converter.reth_supply > alloy::primitives::U256::ZERO);
    assert!(converter.total_eth_balance > alloy::primitives::U256::ZERO);
    assert!(
        converter.total_collateral > alloy::primitives::U256::ZERO
            || converter.maximum_deposit_amount > alloy::primitives::U256::ZERO
    );
    assert!(converter.exchange_rate > alloy::primitives::U256::ZERO);

    eprintln!(
        "rocketpool ws init ok: block={}, total_eth_balance={}, reth_supply={}, excess_balance={}, max_deposit={}, deposit_fee_rate={}",
        block_number,
        converter.total_eth_balance,
        converter.reth_supply,
        converter.excess_balance,
        converter.maximum_deposit_amount,
        converter.deposit_fee_rate
    );

    Ok(())
}

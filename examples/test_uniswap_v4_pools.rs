use std::str::FromStr;
use std::sync::Arc;

use alloy::{
    primitives::{
        address,
        aliases::{I24, U24},
        Address,
    },
    providers::ProviderBuilder,
    rpc::client::ClientBuilder,
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::{
    amms::{
        amm::AMM,
        uniswap_v4::{IPoolManager::PoolKey, UniswapV4Pool},
    },
    state_space::StateSpaceBuilder,
};

fn create_v4_pool(
    token0: Address,
    token1: Address,
    fee: u32,
    tick_spacing: i32,
    hooks: Address,
    manager_address: Address,
) -> AMM {
    let pool_key = PoolKey {
        currency0: token0,
        currency1: token1,
        fee: U24::from(fee as u64),
        tickSpacing: I24::from_str(&tick_spacing.to_string()).unwrap_or(I24::ZERO),
        hooks,
    };
    UniswapV4Pool::new(manager_address, pool_key).into()
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_target(true)
        .init();

    let rpc_endpoint = std::env::var("BASE_PROVIDER")?;

    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(500))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(rpc_endpoint.parse()?);

    let provider = Arc::new(ProviderBuilder::new().connect_client(client));

    let manager_address = address!("498581ff718922c3f8e6a244956af099b2652b2b");
    let hooks_zero = address!("0000000000000000000000000000000000000000");

    let v4_pools: Vec<AMM> = vec![
        create_v4_pool(
            address!("833589fcd6edb6e08f4c7c32d4f71b54bda02913"),
            address!("cbb7c0000ab88b473b1f5afd9ef808440eed33bf"),
            500,
            10,
            hooks_zero,
            manager_address,
        ),
        create_v4_pool(
            address!("0000000000000000000000000000000000000000"),
            address!("833589fcd6edb6e08f4c7c32d4f71b54bda02913"),
            3000,
            60,
            hooks_zero,
            manager_address,
        ),
        create_v4_pool(
            address!("0000000000000000000000000000000000000000"),
            address!("edfa23602d0ec14714057867a78d01e94176bea0"),
            100,
            1,
            hooks_zero,
            manager_address,
        ),
        create_v4_pool(
            address!("0000000000000000000000000000000000000000"),
            address!("833589fcd6edb6e08f4c7c32d4f71b54bda02913"),
            500,
            10,
            hooks_zero,
            manager_address,
        ),
        create_v4_pool(
            address!("0000000000000000000000000000000000000000"),
            address!("833589fcd6edb6e08f4c7c32d4f71b54bda02913"),
            20000,
            400,
            hooks_zero,
            manager_address,
        ),
        create_v4_pool(
            address!("833589fcd6edb6e08f4c7c32d4f71b54bda02913"),
            address!("cbb7c0000ab88b473b1f5afd9ef808440eed33bf"),
            3000,
            60,
            hooks_zero,
            manager_address,
        ),
        create_v4_pool(
            address!("0000000000000000000000000000000000000000"),
            address!("cbb7c0000ab88b473b1f5afd9ef808440eed33bf"),
            3000,
            60,
            hooks_zero,
            manager_address,
        ),
        create_v4_pool(
            address!("833589fcd6edb6e08f4c7c32d4f71b54bda02913"),
            address!("cbb7c0000ab88b473b1f5afd9ef808440eed33bf"),
            10000,
            200,
            hooks_zero,
            manager_address,
        ),
        create_v4_pool(
            address!("0000000000000000000000000000000000000000"),
            address!("cbb7c0000ab88b473b1f5afd9ef808440eed33bf"),
            90,
            2,
            hooks_zero,
            manager_address,
        ),
        create_v4_pool(
            address!("0000000000000000000000000000000000000000"),
            address!("60a3e35cc302bfa44cb288bc5a4f316fdb1adb42"),
            3000,
            60,
            hooks_zero,
            manager_address,
        ),
        create_v4_pool(
            address!("0000000000000000000000000000000000000000"),
            address!("833589fcd6edb6e08f4c7c32d4f71b54bda02913"),
            10000,
            200,
            hooks_zero,
            manager_address,
        ),
    ];

    tracing::info!("Starting sync for {} UniswapV4 pools", v4_pools.len());
    tracing::info!("Using PoolManager address: {:?}", manager_address);

    let state_space_manager = StateSpaceBuilder::new(provider.clone())
        .with_amms(v4_pools.clone())
        .sync()
        .await?;

    let state_space = state_space_manager.state.read().await;

    let total_requested = v4_pools.len();
    let total_synced = state_space.state.len();
    let failed_count = total_requested - total_synced;

    tracing::info!("========== SYNC RESULT ==========");
    tracing::info!("Total requested: {}", total_requested);
    tracing::info!("Successfully synced: {}", total_synced);
    tracing::info!("Failed/Skipped: {}", failed_count);

    for (address, amm) in &state_space.state {
        if let AMM::UniswapV4Pool(pool) = amm {
            tracing::info!(
                "Synced V4 pool: pool_id={:?}, sqrt_price={}, tick={}, liquidity={}",
                pool.pool_id,
                pool.sqrt_price,
                pool.tick,
                pool.liquidity
            );
        }
    }

    Ok(())
}

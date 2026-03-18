use std::sync::Arc;

use alloy::{
    primitives::address,
    providers::ProviderBuilder,
    rpc::client::ClientBuilder,
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::{
    amms::{
        amm::AMM,
        pancake_v3::PancakeV3Pool,
    },
    state_space::StateSpaceBuilder,
};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(true)
        .init();

    let rpc_endpoint = std::env::var("BASE_PROVIDER")?;

    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(500))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(rpc_endpoint.parse()?);

    let provider = Arc::new(ProviderBuilder::new().connect_client(client));

    let pancake_v3_pools: Vec<AMM> = vec![
        PancakeV3Pool::new(address!("C211e1f853A898Bd1302385CCdE55f33a8C4B3f3")).into(),
        PancakeV3Pool::new(address!("26e263efdc91f0d3279E2Ec2Bd58A7Ca5C2fCE62")).into(),
        PancakeV3Pool::new(address!("E9d76696f8A35e2E2520e3125875C3af23f1E69c")).into(),
        PancakeV3Pool::new(address!("72AB388E2E2F6FaceF59E3C3FA2C4E29011c2D38")).into(),
        PancakeV3Pool::new(address!("257FCbAE4Ac6B26A02E4FC5e1a11e4174B5ce395")).into(),
        PancakeV3Pool::new(address!("B775272E537cc670C65DC852908aD47015244EaF")).into(),
        PancakeV3Pool::new(address!("Bd59a718E60bd868123C6E949c9fd97185EFbDB7")).into(),
        PancakeV3Pool::new(address!("b94b22332ABf5f89877A14Cc88f2aBC48c34B3Df")).into(),
        PancakeV3Pool::new(address!("1Ca42C7219F0cB1B67927e26502320cB98F725bd")).into(),
        PancakeV3Pool::new(address!("5b3613ef9a535b48e82e2800aCb77053DFeC93b1")).into(),
    ];

    tracing::info!(
        "Starting sync for {} PancakeV3 pools on Base using StateSpaceBuilder",
        pancake_v3_pools.len()
    );

    for (i, amm) in pancake_v3_pools.iter().enumerate() {
        if let AMM::PancakeV3Pool(pool) = amm {
            tracing::info!(
                "[{}] Pool address: {}",
                i + 1,
                pool.address
            );
        }
    }

    let manager = StateSpaceBuilder::new(provider.clone())
        .with_amms(pancake_v3_pools)
        .sync()
        .await?;

    let state = manager.state.read().await;
    tracing::info!("========== SYNC RESULT ==========");
    tracing::info!("Successfully synced: {} AMMs", state.state.len());

    for (addr, amm) in state.state.iter() {
        if let AMM::PancakeV3Pool(pool) = amm {
            tracing::info!(
                "[OK] {} | token_a={} | token_b={} | tick_spacing={} | fee={} | liquidity={} | tick={} | sqrt_price={}",
                addr,
                pool.token_a.address,
                pool.token_b.address,
                pool.tick_spacing,
                pool.fee,
                pool.liquidity,
                pool.tick,
                pool.sqrt_price
            );
        }
    }

    Ok(())
}

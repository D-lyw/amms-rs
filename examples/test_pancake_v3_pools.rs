use std::sync::Arc;

use alloy::{
    eips::BlockId,
    primitives::address,
    providers::{Provider, ProviderBuilder},
    rpc::client::ClientBuilder,
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::amms::{
    amm::{AMM, AutomatedMarketMaker},
    pancake_v3::{PancakeV3Factory, PancakeV3Pool},
};

const BASE_CHAIN_ID: u64 = 8453;

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
        "Starting sync for {} PancakeV3 pools on Base",
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

    let block = provider.get_block_number().await?;
    tracing::info!("Current block: {}", block);

    match PancakeV3Factory::init_batch(
        pancake_v3_pools.clone(),
        BlockId::from(block),
        provider.clone(),
    )
    .await
    {
        Ok(synced_amms) => {
            tracing::info!("========== SYNC RESULT ==========");
            tracing::info!("Total requested: {}", pancake_v3_pools.len());
            tracing::info!("Successfully synced: {}", synced_amms.len());
            tracing::info!("Failed/Skipped: {}", pancake_v3_pools.len() - synced_amms.len());

            for amm in &synced_amms {
                if let AMM::PancakeV3Pool(pool) = amm {
                    tracing::info!(
                        "[OK] {} | token_a={} | token_b={} | tick_spacing={} | fee={} | liquidity={} | tick={} | sqrt_price={}",
                        pool.address,
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
        }
        Err(e) => {
            tracing::error!("Batch initialization failed: {}", e);
            tracing::error!("Error type: {:?}", std::any::type_name_of_val(&e));

            if let Some(source) = std::error::Error::source(&e) {
                tracing::error!("Caused by: {}", source);
            }

            tracing::info!("Trying individual pool initialization to identify the problematic pool...");

            for (i, amm) in pancake_v3_pools.iter().enumerate() {
                if let AMM::PancakeV3Pool(pool) = amm {
                    tracing::info!("--- Testing pool {}/{}: {} ---", i + 1, pancake_v3_pools.len(), pool.address);

                    match pool.clone().init::<_, _>(BlockId::from(block), provider.clone()).await {
                        Ok(synced_pool) => {
                            tracing::info!(
                                "[OK] {} | token_a={} | token_b={} | tick_spacing={} | fee={} | liquidity={} | tick={}",
                                synced_pool.address,
                                synced_pool.token_a.address,
                                synced_pool.token_b.address,
                                synced_pool.tick_spacing,
                                synced_pool.fee,
                                synced_pool.liquidity,
                                synced_pool.tick
                            );
                        }
                        Err(e) => {
                            tracing::error!(
                                "[FAILED] {} | Error: {}",
                                pool.address,
                                e
                            );
                        }
                    }

                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                }
            }
        }
    }

    Ok(())
}

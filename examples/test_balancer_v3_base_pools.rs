use std::sync::Arc;

use alloy::{
    eips::BlockId,
    primitives::{address, Address},
    providers::{Provider, ProviderBuilder},
    rpc::client::ClientBuilder,
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::amms::{
    amm::{AutomatedMarketMaker, AMM},
    balancer_v3::{BalancerV3Factory, BalancerV3Pool, BalancerV3PoolType},
};

const BASE_VAULT_ADDRESS: Address = address!("bA1333333333a1BA1108E8412f11850A5C319bA9");

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

    let balancer_v3_pools: Vec<AMM> = vec![
        BalancerV3Pool::new(
            address!("b97459fe72708603d822f4edf24bff8ecc07d8f6"),
            BASE_VAULT_ADDRESS,
            BalancerV3PoolType::Weighted,
        )
        .into(),
        BalancerV3Pool::new(
            address!("608de85fff36132e1f6212b4550801f246609bbf"),
            BASE_VAULT_ADDRESS,
            BalancerV3PoolType::Weighted,
        )
        .into(),
        BalancerV3Pool::new(
            address!("f09e25b0f5974ec9caf26df4c2f57f4152e46069"),
            BASE_VAULT_ADDRESS,
            BalancerV3PoolType::Stable,
        )
        .into(),
        BalancerV3Pool::new(
            address!("aae5d575b730c6ce28af137490f3cfc96797d07f"),
            BASE_VAULT_ADDRESS,
            BalancerV3PoolType::Weighted,
        )
        .into(),
    ];

    tracing::info!(
        "Starting sync for {} BalancerV3 pools on Base",
        balancer_v3_pools.len()
    );

    for (i, amm) in balancer_v3_pools.iter().enumerate() {
        if let AMM::BalancerV3Pool(pool) = amm {
            tracing::info!(
                "[{}] Pool address: {} | type: {:?}",
                i + 1,
                pool.address,
                pool.pool_type
            );
        }
    }

    let block = provider.get_block_number().await?;
    tracing::info!("Current block: {}", block);

    match BalancerV3Factory::init_batch(
        balancer_v3_pools.clone(),
        BlockId::from(block),
        provider.clone(),
    )
    .await
    {
        Ok(synced_amms) => {
            tracing::info!("========== SYNC RESULT ==========");
            tracing::info!("Total requested: {}", balancer_v3_pools.len());
            tracing::info!("Successfully synced: {}", synced_amms.len());
            tracing::info!(
                "Failed/Skipped: {}",
                balancer_v3_pools.len() - synced_amms.len()
            );

            for amm in &synced_amms {
                if let AMM::BalancerV3Pool(pool) = amm {
                    tracing::info!(
                        "[OK] {} | type: {:?} | token_count: {} | swap_fee: {:?}",
                        pool.address,
                        pool.pool_type,
                        pool.tokens.len(),
                        pool.swap_fee
                    );
                    for (token_addr, state) in &pool.tokens {
                        tracing::info!(
                            "    Token: {} | decimals: {} | balance: {}",
                            token_addr,
                            state.decimals,
                            state.balance
                        );
                    }
                }
            }
        }
        Err(e) => {
            tracing::error!("Batch initialization failed: {}", e);
            tracing::error!("Error type: {:?}", std::any::type_name_of_val(&e));

            if let Some(source) = std::error::Error::source(&e) {
                tracing::error!("Caused by: {}", source);
            }

            tracing::info!(
                "Trying individual pool initialization to identify the problematic pool..."
            );

            for (i, amm) in balancer_v3_pools.iter().enumerate() {
                if let AMM::BalancerV3Pool(pool) = amm {
                    tracing::info!(
                        "--- Testing pool {}/{}: {} ---",
                        i + 1,
                        balancer_v3_pools.len(),
                        pool.address
                    );

                    match pool
                        .clone()
                        .init::<_, _>(BlockId::from(block), provider.clone())
                        .await
                    {
                        Ok(synced_pool) => {
                            tracing::info!(
                                "[OK] {} | type: {:?} | token_count: {} | swap_fee: {:?}",
                                synced_pool.address,
                                synced_pool.pool_type,
                                synced_pool.tokens.len(),
                                synced_pool.swap_fee
                            );
                            for (token_addr, state) in &synced_pool.tokens {
                                tracing::info!(
                                    "    Token: {} | decimals: {} | balance: {}",
                                    token_addr,
                                    state.decimals,
                                    state.balance
                                );
                            }
                        }
                        Err(e) => {
                            tracing::error!("[FAILED] {} | Error: {}", pool.address, e);
                        }
                    }

                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                }
            }
        }
    }

    Ok(())
}

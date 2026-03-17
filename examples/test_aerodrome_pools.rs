use std::sync::Arc;

use alloy::{
    primitives::address,
    providers::ProviderBuilder,
    rpc::client::ClientBuilder,
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::{
    amms::{
        aerodrome_slipstream::AerodromeSlipstreamPool,
        aerodrome_v2::AerodromeV2Pool,
        amm::AMM,
    },
    state_space::StateSpaceBuilder,
};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .init();

    let rpc_endpoint = std::env::var("BASE_PROVIDER")?;

    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(500))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(rpc_endpoint.parse()?);

    let provider = Arc::new(ProviderBuilder::new().connect_client(client));

    let slipstream_pools: Vec<AMM> = vec![
        AerodromeSlipstreamPool::new(address!("14dccdd311ab827c42cca448ba87b1ac1039e2a4")).into(),
        AerodromeSlipstreamPool::new(address!("22aee3699b6a0fed71490c103bd4e5f3309891d5")).into(),
        AerodromeSlipstreamPool::new(address!("3e66e55e97ce60096f74b7c475e8249f2d31a9fb")).into(),
        AerodromeSlipstreamPool::new(address!("47ca96ea59c13f72745928887f84c9f52c3d7348")).into(),
        AerodromeSlipstreamPool::new(address!("4a79b0168296c0ef7b8f314973b82ad406a29f1b")).into(),
        AerodromeSlipstreamPool::new(address!("4e962bb3889bf030368f56810a9c96b83cb3e778")).into(),
        AerodromeSlipstreamPool::new(address!("4f5905e36ac07ee1f01ffb939aa7f212a58d5cdf")).into(),
        AerodromeSlipstreamPool::new(address!("5d4e504eb4c526995e0cc7a6e327fda75d8b52b5")).into(),
        AerodromeSlipstreamPool::new(address!("70acdf2ad0bf2402c957154f944c19ef4e1cbae1")).into(),
        AerodromeSlipstreamPool::new(address!("861a2922be165a5bd41b1e482b49216b465e1b5f")).into(),
        AerodromeSlipstreamPool::new(address!("98c7a2338336d2d354663246f64676009c7bda97")).into(),
        AerodromeSlipstreamPool::new(address!("a44d3bb767d953711ea4bce8c0f01f4d7d299af6")).into(),
        AerodromeSlipstreamPool::new(address!("b07d7eece8866e549601af5c7622d8cdbedc914e")).into(),
        AerodromeSlipstreamPool::new(address!("b2cc224c1c9fee385f8ad6a55b4d94e92359dc59")).into(),
        AerodromeSlipstreamPool::new(address!("bd3cd0d9d429b41f0a2e1c026552bd598294d5e0")).into(),
        AerodromeSlipstreamPool::new(address!("c5e51044eb7318950b1afb044fccfb25782c48c1")).into(),
        AerodromeSlipstreamPool::new(address!("dbc6998296caa1652a810dc8d3baf4a8294330f1")).into(),
        AerodromeSlipstreamPool::new(address!("dc7ead706795eda3feda08ad519d9452badf2c0d")).into(),
        AerodromeSlipstreamPool::new(address!("e846373c1a92b167b4e9cd5d8e4d6b1db9e90ec7")).into(),
    ];

    let v2_pools: Vec<AMM> = vec![
        AerodromeV2Pool::new(address!("9c38b55f9a9aba91bbcedeb12bf4428f47a6a0b8")).into(),
        AerodromeV2Pool::new(address!("44ecc644449fc3a9858d2007caa8cfaa4c561f91")).into(),
        AerodromeV2Pool::new(address!("b4885bc63399bf5518b994c1d0c153334ee579d0")).into(),
        AerodromeV2Pool::new(address!("cdac0d6c6c59727a65f871236188350531885c43")).into(),
        AerodromeV2Pool::new(address!("3548029694fbb241d45fb24ba0cd9c9d4e745f16")).into(),
        AerodromeV2Pool::new(address!("a6385c73961dd9c58db2ef0c4eb98ce4b60651e8")).into(),
        AerodromeV2Pool::new(address!("91f0f34916ca4e2cce120116774b0e4fa0cdcaa8")).into(),
        AerodromeV2Pool::new(address!("2578365b3dfa7ffe60108e181efb79feddec2319")).into(),
    ];

    let mut amms: Vec<AMM> = Vec::with_capacity(slipstream_pools.len() + v2_pools.len());
    amms.extend(slipstream_pools);
    amms.extend(v2_pools);

    tracing::info!(
        "Starting sync for {} AerodromeSlipstream pools and {} AerodromeV2 pools",
        19,
        8
    );

    let state_space_manager = StateSpaceBuilder::new(provider.clone())
        .with_amms(amms)
        .sync()
        .await?;

    let state_space = state_space_manager.state.read().await;

    let total_requested = 19 + 8;
    let total_synced = state_space.state.len();
    let failed_count = total_requested - total_synced;

    tracing::info!("========== SYNC RESULT ==========");
    tracing::info!("Total requested: {}", total_requested);
    tracing::info!("Successfully synced: {}", total_synced);
    tracing::info!("Failed/Skipped: {}", failed_count);

    let mut slipstream_count = 0;
    let mut v2_count = 0;

    for (address, amm) in &state_space.state {
        match amm {
            AMM::AerodromeSlipstreamPool(pool) => {
                slipstream_count += 1;
                tracing::info!(
                    "[Slipstream] {} | token0={} | token1={} | tick_spacing={} | fee={} | liquidity={}",
                    address,
                    pool.token_a.address,
                    pool.token_b.address,
                    pool.tick_spacing,
                    pool.fee,
                    pool.liquidity
                );
            }
            AMM::AerodromeV2Pool(pool) => {
                v2_count += 1;
                tracing::info!(
                    "[V2] {} | token0={} | token1={} | reserve0={} | reserve1={} | stable={} | fee={}",
                    address,
                    pool.token_a.address,
                    pool.token_b.address,
                    pool.reserve_0,
                    pool.reserve_1,
                    pool.stable,
                    pool.fee
                );
            }
            _ => {}
        }
    }

    tracing::info!("========== SUMMARY ==========");
    tracing::info!("AerodromeSlipstream synced: {}/19", slipstream_count);
    tracing::info!("AerodromeV2 synced: {}/8", v2_count);

    Ok(())
}

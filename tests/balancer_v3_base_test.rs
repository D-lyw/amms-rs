use alloy::{
    eips::BlockId,
    primitives::{Address, U256},
    providers::{Provider, ProviderBuilder},
    rpc::client::ClientBuilder,
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::amms::{
    amm::AutomatedMarketMaker,
    balancer_v3::{BalancerV3Pool, BalancerV3PoolType},
};
use std::str::FromStr;

#[tokio::test]
async fn test_base_balancer_v3_init() -> eyre::Result<()> {
    dotenv::dotenv().ok();

    let rpc_endpoint = match std::env::var("BASE_PROVIDER") {
        Ok(url) => url,
        Err(_) => {
            println!("Skipping test: BASE_PROVIDER not set");
            return Ok(());
        }
    };

    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(250))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(rpc_endpoint.parse()?);

    let provider = ProviderBuilder::new().connect_client(client);

    let block_number = provider.get_block_number().await?;
    let block_id = BlockId::number(block_number);

    let vault_address = Address::from_str("0xbA1333333333a1BA1108E8412f11850A5C319bA9")?;

    let pool_address =
        Address::from_str("0x4126D6C3c675F27a92Ee031E5670B7EAC868887f")?;

    let code = provider.get_code_at(pool_address).await?;
    if code.is_empty() {
        println!("Skipping test: Pool contract not found at {pool_address}");
        return Ok(());
    }

    let mut pool = BalancerV3Pool::new(
        pool_address,
        vault_address,
        BalancerV3PoolType::Weighted,
    );

    pool = pool.init(block_id, provider.clone()).await?;

    println!("Pool address: {:?}", pool.address);
    println!("Pool type: {:?}", pool.pool_type);
    println!("Tokens: {:?}", pool.token_list);
    println!("Token count: {}", pool.tokens.len());

    for (token_addr, state) in &pool.tokens {
        println!(
            "Token: {:?}, decimals: {}, balance: {}",
            token_addr, state.decimals, state.balance
        );
    }

    assert!(!pool.tokens.is_empty(), "Pool should have tokens");
    assert!(
        pool.tokens.values().any(|t| t.balance > U256::ZERO),
        "At least one token should have non-zero balance"
    );

    Ok(())
}

#[tokio::test]
async fn test_base_balancer_v3_stable_pool() -> eyre::Result<()> {
    dotenv::dotenv().ok();

    let rpc_endpoint = match std::env::var("BASE_PROVIDER") {
        Ok(url) => url,
        Err(_) => {
            println!("Skipping test: BASE_PROVIDER not set");
            return Ok(());
        }
    };

    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(250))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(rpc_endpoint.parse()?);

    let provider = ProviderBuilder::new().connect_client(client);

    let block_number = provider.get_block_number().await?;
    let block_id = BlockId::number(block_number);

    let vault_address = Address::from_str("0xbA1333333333a1BA1108E8412f11850A5C319bA9")?;

    let pool_address =
        Address::from_str("0x9B4efaA492923435E8Cbf3A7c5230590866010a7")?;

    let code = provider.get_code_at(pool_address).await?;
    if code.is_empty() {
        println!("Skipping test: Pool contract not found at {pool_address}");
        return Ok(());
    }

    let mut pool = BalancerV3Pool::new(
        pool_address,
        vault_address,
        BalancerV3PoolType::Stable,
    );

    pool = pool.init(block_id, provider.clone()).await?;

    println!("Pool address: {:?}", pool.address);
    println!("Pool type: {:?}", pool.pool_type);
    println!("Tokens: {:?}", pool.token_list);
    println!("Token count: {}", pool.tokens.len());

    for (token_addr, state) in &pool.tokens {
        println!(
            "Token: {:?}, decimals: {}, balance: {}",
            token_addr, state.decimals, state.balance
        );
    }

    assert!(!pool.tokens.is_empty(), "Pool should have tokens");

    Ok(())
}

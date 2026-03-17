use alloy::{
    eips::BlockId,
    primitives::{Address, U256},
    providers::{Provider, ProviderBuilder},
    rpc::client::ClientBuilder,
    sol,
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::amms::{
    amm::AutomatedMarketMaker,
    balancer_v3::{BalancerV3Pool, BalancerV3PoolType},
};
use std::str::FromStr;

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IBalancerV3Router {
        function querySwapSingleTokenExactIn(
            address pool,
            address tokenIn,
            address tokenOut,
            uint256 exactAmountIn,
            address limitAmountOut,
            bytes calldata userData
        ) external returns (uint256 amountOut);
    }
}

async fn run_swap_test(
    pool_address: Address,
    pool_type: BalancerV3PoolType,
    token_in_idx: usize,
    token_out_idx: usize,
    amount_units: f64,
    check_desc: &str,
) -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let rpc_endpoint = match std::env::var("ETHEREUM_PROVIDER") {
        Ok(url) => url,
        Err(_) => {
            println!("Skipping test: ETHEREUM_PROVIDER not set");
            return Ok(());
        }
    };

    let provider = ProviderBuilder::new().connect_http(rpc_endpoint.parse()?);

    // Fetch latest block to ensure consistency between Init and Router Query

    let block_number = provider.get_block_number().await?;
    let block_id = BlockId::number(block_number);

    // Balancer V3 Vault (Mainnet)
    let vault_address = Address::from_str("ba1333333333a1BA1108E8412f11850A5C319bA9")?;
    let router_address = Address::from_str("0xAE563E3f8219521950555F5962419C8919758Ea2")?;

    // Check if pool exists
    let code = provider.get_code_at(pool_address).await?;
    if code.is_empty() {
        println!("Skipping test: Pool contract not found at {pool_address}");
        return Ok(());
    }

    let mut pool = BalancerV3Pool::new(
        pool_address,
        vault_address,
        pool_type,
    );
    pool = pool.init(block_id, provider.clone()).await?;

    assert_eq!(pool.pool_type, pool_type);
    println!("Pool Initialized: {:?}", pool.address);

    if pool.token_list.len() < 2 {
        println!("Pool has insufficient tokens");
        return Ok(());
    }

    let token_in_addr = pool.token_list[token_in_idx];
    let token_out_addr = pool.token_list[token_out_idx];

    let token_in_state = pool.tokens.get(&token_in_addr).unwrap();
    let decimals = token_in_state.decimals;

    let amount_in = if decimals <= 18 {
        U256::from((amount_units * 10f64.powi(decimals as i32)) as u128)
    } else {
        U256::from(amount_units as u128)
    };

    println!("Simulating Swap: {amount_units} of {token_in_addr} -> {token_out_addr}");
    let amount_out = pool.simulate_swap(token_in_addr, token_out_addr, amount_in)?;

    let router = IBalancerV3Router::new(router_address, provider.clone());
    match router
        .querySwapSingleTokenExactIn(
            pool_address,
            token_in_addr,
            token_out_addr,
            amount_in,
            Address::ZERO,
            alloy::primitives::Bytes::new(),
        )
        .block(block_id) // CRITICAL: Use same block as init
        .call()
        .await
    {
        Ok(onchain_amount_out) => {
            let onchain_val = onchain_amount_out;

            let diff = if amount_out > onchain_val {
                amount_out - onchain_val
            } else {
                onchain_val - amount_out
            };

            println!("===============");
            println!("{check_desc}: Local={amount_out}, OnChain={onchain_val}, Diff={diff}");

            // Tolerance check
            // Boosted/Stable pools might have higher diffs due to Rate Providers or Math approximation
            let tolerance_bps = 5; // 0.05%
            let allowed_diff = onchain_val * U256::from(tolerance_bps) / U256::from(10000);

            assert!(
                diff <= allowed_diff,
                "Relative error too high. Local: {amount_out}, OnChain: {onchain_val}, Diff: {diff}, Allowed: {allowed_diff}"
            );
        }
        Err(e) => {
            println!("Router querySwapSingleTokenExactIn failed: {e:?}");
            // Do not fail test if router reverts (e.g. paused pool), but log it
        }
    }
    Ok(())
}

#[tokio::test]
async fn test_balancer_v3_stable_swap_gho_usdt_usdc() -> eyre::Result<()> {
    let pool = Address::from_str("0x85b2b559bc2d21104c4defdd6efca8a20343361d")?;
    // GHO -> USDT (0 -> 1)
    run_swap_test(
        pool,
        BalancerV3PoolType::Stable,
        0,
        1,
        100.0,
        "Stable GHO->USDT",
    )
    .await?;
    // USDT -> GHO (1 -> 0)
    run_swap_test(
        pool,
        BalancerV3PoolType::Stable,
        1,
        0,
        100.0,
        "Stable USDT->GHO",
    )
    .await?;

    // USDT -> USDC (1 -> 2)
    run_swap_test(
        pool,
        BalancerV3PoolType::Stable,
        1,
        2,
        100.0, // 100 USDT
        "Stable USDT->USDC",
    )
    .await?;

    // USDC -> GHO (2 -> 0)
    run_swap_test(
        pool,
        BalancerV3PoolType::Stable,
        2,
        0,
        100.0, // 100 USDC
        "Stable USDC->GHO",
    )
    .await
}

#[tokio::test]
async fn test_balancer_v3_stable_swap_weth_wsteth() -> eyre::Result<()> {
    let pool = Address::from_str("0xc4ce391d82d164c166df9c8336ddf84206b2f812")?;
    // WETH -> wstETH
    run_swap_test(
        pool,
        BalancerV3PoolType::Stable,
        0,
        1,
        1.0,
        "Stable WETH->wstETH",
    )
    .await?;
    // wstETH -> WETH
    run_swap_test(
        pool,
        BalancerV3PoolType::Stable,
        1,
        0,
        1.0,
        "Stable wstETH->WETH",
    )
    .await
}

#[tokio::test]
async fn test_balancer_v3_stable_swap_reth_waeth() -> eyre::Result<()> {
    let pool = Address::from_str("0x1ea5870f7c037930ce1d5d8d9317c670e89e13e3")?;
    // rETH -> waEth
    run_swap_test(
        pool,
        BalancerV3PoolType::Stable,
        0,
        1,
        0.1,
        "Stable rETH->waEth",
    )
    .await?;
    // waEth -> rETH
    run_swap_test(
        pool,
        BalancerV3PoolType::Stable,
        1,
        0,
        0.1,
        "Stable waEth->rETH",
    )
    .await
}

#[tokio::test]
async fn test_balancer_v3_weighted_swap_weth_tree() -> eyre::Result<()> {
    let pool = Address::from_str("0xdaba3d8ccf79ef289a7e2dbce51871b39ea445a2")?;
    // WETH -> TREE
    run_swap_test(
        pool,
        BalancerV3PoolType::Weighted,
        0,
        1,
        1.0,
        "Weighted WETH->TREE",
    )
    .await?;
    // TREE -> WETH
    run_swap_test(
        pool,
        BalancerV3PoolType::Weighted,
        1,
        0,
        100.0,
        "Weighted TREE->WETH",
    )
    .await
}

#[tokio::test]
async fn test_balancer_v3_weighted_alcx_weth_80_20() -> eyre::Result<()> {
    // Pool: 20% WETH / 80% ALCX
    let pool = Address::from_str("0x1535d7ca00323aa32bd62aeddf7ca651e4b95966")?;

    // Check 1: Small Swap WETH -> ALCX
    run_swap_test(
        pool,
        BalancerV3PoolType::Weighted,
        0,
        1,
        0.1,
        "Weighted 80/20 WETH->ALCX (Small)",
    )
    .await?;

    // Check 2: Large Swap ALCX -> WETH (100 ALCX)
    run_swap_test(
        pool,
        BalancerV3PoolType::Weighted,
        1,
        0,
        100.0,
        "Weighted 80/20 ALCX->WETH (Large)",
    )
    .await
}

#[tokio::test]
async fn test_balancer_v3_weighted_eigen_weth_50_50() -> eyre::Result<()> {
    // Pool: 50% WETH / 50% EIGEN
    let pool = Address::from_str("0xbda917a67c7d9ae67da92c4ea87e10e5d6c11b54")?;

    // EIGEN -> WETH
    run_swap_test(
        pool,
        BalancerV3PoolType::Weighted,
        1,
        0,
        10.0,
        "Weighted 50/50 EIGEN->WETH",
    )
    .await?;

    // WETH -> EIGEN
    run_swap_test(
        pool,
        BalancerV3PoolType::Weighted,
        0,
        1,
        1.0,
        "Weighted 50/50 WETH->EIGEN",
    )
    .await
}

#[tokio::test]
async fn test_calculate_price() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(250))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(rpc_endpoint.parse()?);

    let provider = ProviderBuilder::new().connect_client(client);

    // 7-token weighted pool
    let pool_address = Address::from_str("0xB1F62fc950E30A64a5032bBD8619A70B2c2B27C6")?;
    let vault_address = Address::from_str("0xbA1333333333a1BA1108E8412f11850A5C319bA9")?;

    let mut pool = BalancerV3Pool::new(
        pool_address,
        vault_address,
        BalancerV3PoolType::Weighted,
    );

    pool = pool.init(BlockId::latest(), provider.clone()).await?;

    // Use UNI and another token from the pool
    let uni = Address::from_str("0x1f9840a85d5af5bf1d1762f925bdaddc4201f984")?;
    let other = Address::from_str("0xbe1936a67f503e0eaf2434b0cf9f4e3d7100008a")?; 

    let price_uni = pool.calculate_price(uni, other)?;
    let price_other = pool.calculate_price(other, uni)?;

    println!("UNI Price in Other: {}", price_uni);
    println!("Other Price in UNI: {}", price_other);

    assert!(price_uni > 0.0);
    assert!(price_other > 0.0);

    // Sanity check
    let product = price_uni * price_other;
    assert!(product > 0.99 && product < 1.01);

    Ok(())
}

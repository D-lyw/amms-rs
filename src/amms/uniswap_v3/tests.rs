use super::*;

use alloy::{
    sol,
    primitives::{address, aliases::U24, U160, U256},
    providers::ProviderBuilder,
    rpc::client::ClientBuilder,
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};

sol! {
    /// Interface of the Quoter
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IQuoter {
        function quoteExactInputSingle(address tokenIn, address tokenOut,uint24 fee, uint256 amountIn, uint160 sqrtPriceLimitX96) external returns (uint256 amountOut);
        function quoteExactOutputSingle(address tokenIn, address tokenOut,uint24 fee, uint256 amountOut, uint160 sqrtPriceLimitX96) external returns (uint256 amountIn);
    }
}

sol! {
    /// Interface of the Uniswap V3 Factory
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IUniswapV3Factory {
        function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address pool);
    }
}

const UNISWAP_V3_FACTORY: Address = address!("1F98431c8aD98523631AE4a59f267346ea31F984");
const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
const WBTC: Address = address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");
const LINK: Address = address!("514910771AF9Ca656af840dff83E8264EcF986CA");
const UNI: Address = address!("1f9840a85d5aF5bf1D1762F925BDADdC4201F984");

async fn build_provider() -> eyre::Result<impl alloy::providers::Provider<alloy::network::Ethereum> + Clone> {
    let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;
    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(250))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(rpc_endpoint.parse()?);
    Ok(ProviderBuilder::new().connect_client(client))
}

async fn load_pool_from_factory<P: alloy::providers::Provider<alloy::network::Ethereum> + Clone>(
    provider: P,
    token_a: Address,
    token_b: Address,
    fee: u32,
    block: BlockId,
) -> eyre::Result<Option<UniswapV3Pool>> {
    let factory = IUniswapV3Factory::new(UNISWAP_V3_FACTORY, provider.clone());
    let (token0, token1) = if token_a < token_b {
        (token_a, token_b)
    } else {
        (token_b, token_a)
    };
    let pool_addr = factory
        .getPool(token0, token1, U24::from(fee))
        .block(block)
        .call()
        .await?;

    if pool_addr == Address::ZERO {
        return Ok(None);
    }

    let pool = UniswapV3Pool::new(pool_addr)
        .init(block, provider)
        .await?;
    Ok(Some(pool))
}

#[tokio::test]
async fn test_simulate_swap_usdc_weth() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let provider = build_provider().await?;

    let pool = UniswapV3Pool::new(address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"))
        .init(BlockId::latest(), provider.clone())
        .await?;

    let quoter = IQuoter::new(
        address!("b27308f9f90d607463bb33ea1bebb41c27ce5ab6"),
        provider.clone(),
    );

    // Test swap from USDC to WETH
    let amount_in = U256::from(100000000); // 100 USDC
    let amount_out = pool.simulate_swap(pool.token_a.address, Address::default(), amount_in)?;

    dbg!(pool.token_a.address);
    dbg!(pool.token_b.address);
    dbg!(amount_in);
    dbg!(amount_out);
    dbg!(pool.fee);

    let expected_amount_out = quoter
        .quoteExactInputSingle(
            pool.token_a.address,
            pool.token_b.address,
            U24::from(pool.fee),
            amount_in,
            U160::ZERO,
        )
        .block(BlockId::latest())
        .call()
        .await?;

    assert_eq!(amount_out, expected_amount_out);

    let amount_in_1 = U256::from(10000000000_u64); // 10_000 USDC
    let amount_out_1 =
        pool.simulate_swap(pool.token_a.address, Address::default(), amount_in_1)?;

    let expected_amount_out_1 = quoter
        .quoteExactInputSingle(
            pool.token_a.address,
            pool.token_b.address,
            U24::from(pool.fee),
            amount_in_1,
            U160::ZERO,
        )
        .block(BlockId::latest())
        .call()
        .await?;

    assert_eq!(amount_out_1, expected_amount_out_1);

    let amount_in_2 = U256::from(10000000000000_u128); // 10_000_000 USDC
    let amount_out_2 =
        pool.simulate_swap(pool.token_a.address, Address::default(), amount_in_2)?;

    let expected_amount_out_2 = quoter
        .quoteExactInputSingle(
            pool.token_a.address,
            pool.token_b.address,
            U24::from(pool.fee),
            amount_in_2,
            U160::ZERO,
        )
        .block(BlockId::latest())
        .call()
        .await?;

    assert_eq!(amount_out_2, expected_amount_out_2);

    let amount_in_3 = U256::from(100000000000000_u128); // 100_000_000 USDC
    let amount_out_3 =
        pool.simulate_swap(pool.token_a.address, Address::default(), amount_in_3)?;

    let expected_amount_out_3 = quoter
        .quoteExactInputSingle(
            pool.token_a.address,
            pool.token_b.address,
            U24::from(pool.fee),
            amount_in_3,
            U160::ZERO,
        )
        .block(BlockId::latest())
        .call()
        .await?;

    assert_eq!(amount_out_3, expected_amount_out_3);

    // Test swap from WETH to USDC

    let amount_in = U256::from(1000000000000000000_u128); // 1 ETH
    let amount_out = pool.simulate_swap(pool.token_b.address, Address::default(), amount_in)?;
    let expected_amount_out = quoter
        .quoteExactInputSingle(
            pool.token_b.address,
            pool.token_a.address,
            U24::from(pool.fee),
            amount_in,
            U160::ZERO,
        )
        .block(BlockId::latest())
        .call()
        .await?;
    assert_eq!(amount_out, expected_amount_out);

    let amount_in_1 = U256::from(10000000000000000000_u128); // 10 ETH
    let amount_out_1 =
        pool.simulate_swap(pool.token_b.address, Address::default(), amount_in_1)?;
    let expected_amount_out_1 = quoter
        .quoteExactInputSingle(
            pool.token_b.address,
            pool.token_a.address,
            U24::from(pool.fee),
            amount_in_1,
            U160::ZERO,
        )
        .block(BlockId::latest())
        .call()
        .await?;
    assert_eq!(amount_out_1, expected_amount_out_1);

    let amount_in_2 = U256::from(100000000000000000000_u128); // 100 ETH
    let amount_out_2 =
        pool.simulate_swap(pool.token_b.address, Address::default(), amount_in_2)?;
    let expected_amount_out_2 = quoter
        .quoteExactInputSingle(
            pool.token_b.address,
            pool.token_a.address,
            U24::from(pool.fee),
            amount_in_2,
            U160::ZERO,
        )
        .block(BlockId::latest())
        .call()
        .await?;
    assert_eq!(amount_out_2, expected_amount_out_2);

    let amount_in_3 = U256::from(100000000000000000000_u128); // 100_000 ETH
    let amount_out_3 =
        pool.simulate_swap(pool.token_b.address, Address::default(), amount_in_3)?;
    let expected_amount_out_3 = quoter
        .quoteExactInputSingle(
            pool.token_b.address,
            pool.token_a.address,
            U24::from(pool.fee),
            amount_in_3,
            U160::ZERO,
        )
        .block(BlockId::latest())
        .call()
        .await?;

    assert_eq!(amount_out_3, expected_amount_out_3);

    Ok(())
}

#[tokio::test]
async fn test_simulate_swap_exact_out_usdc_weth() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let provider = build_provider().await?;

    let current_block = BlockId::from(provider.get_block_number().await?);

    let pool = UniswapV3Pool::new(address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"))
        .init(current_block, provider.clone())
        .await?;

    let quoter = IQuoter::new(
        address!("b27308f9f90d607463bb33ea1bebb41c27ce5ab6"),
        provider.clone(),
    );

    // Exact out: USDC -> WETH (want WETH out)
    let exact_outs_weth = [
        U256::from(100_000_000_000_000_u128),   // 0.0001 WETH
        U256::from(1_000_000_000_000_000_u128), // 0.001 WETH
        U256::from(1000_000_000_000_000_000_000_u128), // 1000 WETH
    ];
    for amount_out in exact_outs_weth {
        let amount_in = pool.simulate_swap_exact_out(
            pool.token_a.address,
            Address::default(),
            amount_out,
        )?;

        let expected_amount_in = quoter
            .quoteExactOutputSingle(
                pool.token_a.address,
                pool.token_b.address,
                U24::from(pool.fee),
                amount_out,
                U160::ZERO,
            )
            .block(current_block)
            .call()
            .await?;

        println!("amount_in: {:?}, expected_amount_in: {:?}", amount_in, expected_amount_in);
        assert_eq!(amount_in, expected_amount_in);
    }

    // Exact out: WETH -> USDC (want USDC out)
    let exact_outs_usdc = [
        U256::from(1_000_000u64),      // 1 USDC
        U256::from(100_000_000u64),    // 100 USDC
        U256::from(10_000_000_000u64), // 10,000 USDC
    ];
    for amount_out in exact_outs_usdc {
        let amount_in = pool.simulate_swap_exact_out(
            pool.token_b.address,
            Address::default(),
            amount_out,
        )?;

        let expected_amount_in = quoter
            .quoteExactOutputSingle(
                pool.token_b.address,
                pool.token_a.address,
                U24::from(pool.fee),
                amount_out,
                U160::ZERO,
            )
            .block(current_block)
            .call()
            .await?;

        assert_eq!(amount_in, expected_amount_in);
    }

    Ok(())
}

#[tokio::test]
async fn test_simulate_swap_exact_out_link_weth() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(250))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(rpc_endpoint.parse()?);

    let provider = ProviderBuilder::new().connect_client(client);
    let current_block = BlockId::from(provider.get_block_number().await?);

    let pool = UniswapV3Pool::new(address!("5d4F3C6fA16908609BAC31Ff148Bd002AA6b8c83"))
        .init(current_block, provider.clone())
        .await?;

    let quoter = IQuoter::new(
        address!("b27308f9f90d607463bb33ea1bebb41c27ce5ab6"),
        provider.clone(),
    );

    // Exact out: LINK -> WETH (want WETH out)
    let exact_outs_weth = [
        U256::from(100_000_000_000_000_u128),   // 0.0001 WETH
        U256::from(1_000_000_000_000_000_u128), // 0.001 WETH
        U256::from(10_000_000_000_000_000_000_u128), // 10 WETH
    ];
    for amount_out in exact_outs_weth {
        let amount_in = pool.simulate_swap_exact_out(
            pool.token_a.address,
            Address::default(),
            amount_out,
        )?;

        let expected_amount_in = quoter
            .quoteExactOutputSingle(
                pool.token_a.address,
                pool.token_b.address,
                U24::from(pool.fee),
                amount_out,
                U160::ZERO,
            )
            .block(current_block)
            .call()
            .await?;

        assert_eq!(amount_in, expected_amount_in);
    }

    // Exact out: WETH -> LINK (want LINK out)
    let exact_outs_link = [
        U256::from(1_000_000_000_000_000_000_u128),  // 1 LINK
        U256::from(10_000_000_000_000_000_000_u128), // 10 LINK
        U256::from(100_000_000_000_000_000_000_u128), // 100 LINK
    ];
    for amount_out in exact_outs_link {
        let amount_in = pool.simulate_swap_exact_out(
            pool.token_b.address,
            Address::default(),
            amount_out,
        )?;

        let expected_amount_in = quoter
            .quoteExactOutputSingle(
                pool.token_b.address,
                pool.token_a.address,
                U24::from(pool.fee),
                amount_out,
                U160::ZERO,
            )
            .block(current_block)
            .call()
            .await?;

        assert_eq!(amount_in, expected_amount_in);
    }

    Ok(())
}

#[tokio::test]
async fn test_simulate_swap_link_weth() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let provider = build_provider().await?;

    let current_block = BlockId::from(provider.get_block_number().await?);

    let pool = UniswapV3Pool::new(address!("5d4F3C6fA16908609BAC31Ff148Bd002AA6b8c83"))
        .init(current_block, provider.clone())
        .await?;

    let quoter = IQuoter::new(
        address!("b27308f9f90d607463bb33ea1bebb41c27ce5ab6"),
        provider.clone(),
    );

    // Test swap LINK to WETH
    let amount_in = U256::from(1000000000000000000_u128); // 1 LINK
    let amount_out = pool.simulate_swap(pool.token_a.address, Address::default(), amount_in)?;
    let expected_amount_out = quoter
        .quoteExactInputSingle(
            pool.token_a.address,
            pool.token_b.address,
            U24::from(pool.fee),
            amount_in,
            U160::ZERO,
        )
        .block(current_block)
        .call()
        .await?;

    assert_eq!(amount_out, expected_amount_out);

    let amount_in_1 = U256::from(10000000000000000000_u128); // 10 LINK
    let amount_out_1 =
        pool.simulate_swap(pool.token_a.address, Address::default(), amount_in_1)?;
    let expected_amount_out_1 = quoter
        .quoteExactInputSingle(
            pool.token_a.address,
            pool.token_b.address,
            U24::from(pool.fee),
            amount_in_1,
            U160::ZERO,
        )
        .block(current_block)
        .call()
        .await?;

    assert_eq!(amount_out_1, expected_amount_out_1);

    let amount_in_2 = U256::from(100000000000000000000_u128); // 100 LINK
    let amount_out_2 =
        pool.simulate_swap(pool.token_a.address, Address::default(), amount_in_2)?;
    let expected_amount_out_2 = quoter
        .quoteExactInputSingle(
            pool.token_a.address,
            pool.token_b.address,
            U24::from(pool.fee),
            amount_in_2,
            U160::ZERO,
        )
        .block(current_block)
        .call()
        .await?;

    assert_eq!(amount_out_2, expected_amount_out_2);

    let amount_in_3 = U256::from(1000000000000000000000_u128); // 1000 LINK
    let amount_out_3 =
        pool.simulate_swap(pool.token_a.address, Address::default(), amount_in_3)?;
    let expected_amount_out_3 = quoter
        .quoteExactInputSingle(
            pool.token_a.address,
            pool.token_b.address,
            U24::from(pool.fee),
            amount_in_3,
            U160::ZERO,
        )
        .block(current_block)
        .call()
        .await?;

    assert_eq!(amount_out_3, expected_amount_out_3);

    // Test swap WETH to LINK
    let amount_in = U256::from(1000000000000000000_u128); // 1 WETH
    let amount_out = pool.simulate_swap(pool.token_b.address, Address::default(), amount_in)?;
    let expected_amount_out = quoter
        .quoteExactInputSingle(
            pool.token_b.address,
            pool.token_a.address,
            U24::from(pool.fee),
            amount_in,
            U160::ZERO,
        )
        .block(current_block)
        .call()
        .await?;

    assert_eq!(amount_out, expected_amount_out);

    let amount_in_1 = U256::from(10000000000000000000_u128); // 10 WETH
    let amount_out_1 =
        pool.simulate_swap(pool.token_b.address, Address::default(), amount_in_1)?;
    let expected_amount_out_1 = quoter
        .quoteExactInputSingle(
            pool.token_b.address,
            pool.token_a.address,
            U24::from(pool.fee),
            amount_in_1,
            U160::ZERO,
        )
        .block(current_block)
        .call()
        .await?;

    assert_eq!(amount_out_1, expected_amount_out_1);

    let amount_in_2 = U256::from(100000000000000000000_u128); // 100 WETH
    let amount_out_2 =
        pool.simulate_swap(pool.token_b.address, Address::default(), amount_in_2)?;
    let expected_amount_out_2 = quoter
        .quoteExactInputSingle(
            pool.token_b.address,
            pool.token_a.address,
            U24::from(pool.fee),
            amount_in_2,
            U160::ZERO,
        )
        .block(current_block)
        .call()
        .await?;

    assert_eq!(amount_out_2, expected_amount_out_2);

    let amount_in_3 = U256::from(1000000000000000000000_u128); // 1000 WETH
    let amount_out_3 =
        pool.simulate_swap(pool.token_b.address, Address::default(), amount_in_3)?;
    let expected_amount_out_3 = quoter
        .quoteExactInputSingle(
            pool.token_b.address,
            pool.token_a.address,
            U24::from(pool.fee),
            amount_in_3,
            U160::ZERO,
        )
        .block(current_block)
        .call()
        .await?;

    assert_eq!(amount_out_3, expected_amount_out_3);

    Ok(())
}

#[tokio::test]
async fn test_simulate_swap_exact_out_additional_pools() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let provider = build_provider().await?;
    let current_block = BlockId::from(provider.get_block_number().await?);

    let quoter = IQuoter::new(
        address!("b27308f9f90d607463bb33ea1bebb41c27ce5ab6"),
        provider.clone(),
    );

    // (token_in, token_out, fee, exact_outs)
    let test_cases: Vec<(Address, Address, u32, Vec<U256>)> = vec![
        // Stable pools (fee 100)
        (USDC, USDT, 100, vec![U256::from(1u64), U256::from(1_000_000u64), U256::from(10_000_000u64)]), // 1 wei, 1, 10 USDT
        (USDT, USDC, 100, vec![U256::from(1u64), U256::from(1_000_000u64), U256::from(10_000_000u64)]), // reverse
        (USDC, DAI, 100, vec![U256::from(1u64), U256::from(1_000_000_000_000_000_000u128), U256::from(10_000_000_000_000_000_000u128)]), // 1 wei, 1, 10 DAI
        (DAI, USDC, 100, vec![U256::from(1u64), U256::from(1_000_000u64), U256::from(10_000_000u64)]), // reverse (USDC 6)
        // Common volatile pools (fee 3000)
        (WETH, DAI, 3000, vec![U256::from(1u64), U256::from(1_000_000_000_000_000u128), U256::from(10_000_000_000_000_000u128)]), // 1 wei, 0.001, 0.01 DAI
        (DAI, WETH, 3000, vec![U256::from(1u64), U256::from(1_000_000_000_000_000u128), U256::from(10_000_000_000_000_000u128)]), // reverse (WETH 18)
        (WETH, USDT, 3000, vec![U256::from(1u64), U256::from(1_000_000u64), U256::from(100_000_000u64)]), // 1 wei, 1, 100 USDT
        (USDT, WETH, 3000, vec![U256::from(1u64), U256::from(1_000_000_000_000_000u128)]), // reverse small
        (WETH, WBTC, 3000, vec![U256::from(1u64), U256::from(10_000u64), U256::from(100_000u64)]), // 1 sat, 0.0001, 0.001 WBTC (8 decimals)
        (WBTC, WETH, 3000, vec![U256::from(1u64), U256::from(10_000u64)]), // reverse small
    ];

    for (token_in, token_out, fee, exact_outs) in test_cases {
        let Some(pool) = load_pool_from_factory(
            provider.clone(),
            token_in,
            token_out,
            fee,
            current_block,
        )
        .await?
        else {
            continue;
        };

        for amount_out in exact_outs {
            let amount_in = pool.simulate_swap_exact_out(token_in, Address::default(), amount_out)?;
            let expected_amount_in = quoter
                .quoteExactOutputSingle(
                    token_in,
                    token_out,
                    U24::from(fee),
                    amount_out,
                    U160::ZERO,
                )
                .block(current_block)
                .call()
                .await?;
            assert_eq!(amount_in, expected_amount_in);
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_simulate_swap_exact_out_insufficient_liquidity() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let provider = build_provider().await?;
    let current_block = BlockId::from(provider.get_block_number().await?);

    let pool = UniswapV3Pool::new(address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"))
        .init(current_block, provider.clone())
        .await?;

    // Request an absurdly large exact-out to force exhaustion.
    let huge_out = U256::from(10u8).pow(U256::from(36u8));
    let res = pool.simulate_swap_exact_out(pool.token_a.address, Address::default(), huge_out);
    assert!(res.is_err());

    Ok(())
}

#[tokio::test]
async fn test_simulate_swap_exact_out_fee_10000() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let provider = build_provider().await?;
    let current_block = BlockId::from(provider.get_block_number().await?);

    let quoter = IQuoter::new(
        address!("b27308f9f90d607463bb33ea1bebb41c27ce5ab6"),
        provider.clone(),
    );

    let candidates: Vec<(Address, Address)> = vec![
        (WETH, USDC),
        (WETH, DAI),
        (WETH, USDT),
        (WBTC, WETH),
        (LINK, WETH),
        (UNI, WETH),
    ];

    let mut found_any = false;
    for (token_in, token_out) in candidates {
        let Some(pool) =
            load_pool_from_factory(provider.clone(), token_in, token_out, 10_000, current_block)
                .await?
        else {
            continue;
        };

        found_any = true;

        // Use small/medium exact-out amounts to reduce risk of exhausting liquidity.
        let out_decimals = pool.decimals(token_out);
        let unit = U256::from(10u8).pow(U256::from(out_decimals));
        let exact_outs = vec![unit, unit * U256::from(10u8)];

        for amount_out in exact_outs {
            let amount_in = pool.simulate_swap_exact_out(token_in, Address::default(), amount_out)?;
            let expected_amount_in = quoter
                .quoteExactOutputSingle(
                    token_in,
                    token_out,
                    U24::from(10_000u32),
                    amount_out,
                    U160::ZERO,
                )
                .block(current_block)
                .call()
                .await?;
            assert_eq!(amount_in, expected_amount_in);
        }
    }

    if !found_any {
        return Err(eyre::eyre!("no fee=10000 pools found in candidates"));
    }

    Ok(())
}

#[tokio::test]
async fn test_simulate_swap_exact_out_low_liquidity_pool() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let provider = build_provider().await?;
    let current_block = BlockId::from(provider.get_block_number().await?);

    let quoter = IQuoter::new(
        address!("b27308f9f90d607463bb33ea1bebb41c27ce5ab6"),
        provider.clone(),
    );

    let candidates: Vec<(Address, Address, u32)> = vec![
        (LINK, WETH, 3000),
        (UNI, WETH, 3000),
        (WBTC, WETH, 3000),
        (WETH, USDT, 3000),
        (WETH, DAI, 3000),
        (USDC, DAI, 500), // fee 0.05
        (USDC, USDT, 100), // fee 0.01
    ];

    let mut selected: Option<(UniswapV3Pool, Address, Address, u32)> = None;
    for (token_in, token_out, fee) in candidates {
        let Some(pool) =
            load_pool_from_factory(provider.clone(), token_in, token_out, fee, current_block)
                .await?
        else {
            continue;
        };

        // Pick the lowest-liquidity pool found among candidates.
        if selected
            .as_ref()
            .map(|(p, _, _, _)| pool.liquidity < p.liquidity)
            .unwrap_or(true)
        {
            selected = Some((pool, token_in, token_out, fee));
        }
    }

    let Some((pool, token_in, token_out, fee)) = selected else {
        return Err(eyre::eyre!("no candidate pools found for low-liquidity test"));
    };

    let out_decimals = pool.decimals(token_out);
    let unit = U256::from(10u8).pow(U256::from(out_decimals));
    let exact_outs = vec![unit, unit * U256::from(100u8), unit * U256::from(1_000u16)];

    for amount_out in exact_outs {
        let amount_in = pool.simulate_swap_exact_out(token_in, Address::default(), amount_out)?;
        let expected_amount_in = quoter
            .quoteExactOutputSingle(
                token_in,
                token_out,
                U24::from(fee),
                amount_out,
                U160::ZERO,
            )
            .block(current_block)
            .call()
            .await?;
        assert_eq!(amount_in, expected_amount_in);
    }

    Ok(())
}

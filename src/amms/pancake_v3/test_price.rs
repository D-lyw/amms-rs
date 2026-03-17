#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::amms::{
        amm::AutomatedMarketMaker,
        pancake_v3::{
            IQuoterV2, IQuoterV2::IQuoterV2Instance, IPancakeV3FactoryExt::IPancakeV3FactoryExtInstance,
            PancakeV3Pool,
        },
    };
    use alloy::{
        eips::BlockId,
        primitives::{address, aliases::U24, Address, U160, U256},
        providers::{Provider, ProviderBuilder},
    };
    use eyre::Result;

    const PANCAKE_V3_FACTORY_ETH: Address = address!("0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865");
    const PANCAKE_V3_QUOTER_ETH: Address = address!("B048Bbc1Ee6b733FFfCFb9e9CeF7375518e25997");

    const USDC_ETH: Address = address!("A0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    const USDT_ETH: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    const WETH_ETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

    async fn find_pool<P: Provider + Clone>(
        factory: &IPancakeV3FactoryExtInstance<P>,
        token_a: Address,
        token_b: Address,
        fees: &[u32],
    ) -> eyre::Result<(Address, u32)> {
        for &fee in fees {
            let addr = factory.getPool(token_a, token_b, U24::from(fee)).call().await?;
            if !addr.is_zero() {
                return Ok((addr, fee));
            }
        }
        Ok((Address::ZERO, 0))
    }

    #[tokio::test]
    async fn test_simulate_swap_usdc_usdt() -> Result<()> {
        dotenv::dotenv().ok();
        let provider_url = std::env::var("ETHEREUM_PROVIDER")?;
        let provider = Arc::new(ProviderBuilder::new().connect_http(provider_url.parse()?));

        let factory = IPancakeV3FactoryExtInstance::new(PANCAKE_V3_FACTORY_ETH, provider.clone());

        let (pool_addr, fee) = find_pool(&factory, USDC_ETH, USDT_ETH, &[100, 500, 2500, 10000]).await?;
        if pool_addr.is_zero() {
            println!("Pool not found, skipping test");
            return Ok(());
        }

        let block = BlockId::from(provider.get_block_number().await?);
        let pool = PancakeV3Pool::new(pool_addr).init::<_, _>(block, provider.clone()).await?;

        let quoter = IQuoterV2Instance::new(PANCAKE_V3_QUOTER_ETH, provider.clone());

        println!("Pool address: {:?}", pool_addr);
        println!("Token A: {:?}", pool.token_a.address);
        println!("Token B: {:?}", pool.token_b.address);
        println!("Fee: {}", fee);

        let test_amounts = [
            U256::from(100_000u64),
            U256::from(1_000_000u64),
            U256::from(10_000_000u64),
            U256::from(100_000_000u64),
            U256::from(1_000_000_000u64),
        ];

        for amount_in in test_amounts {
            let simulated = pool.simulate_swap(USDC_ETH, USDT_ETH, amount_in)?;

            let params = IQuoterV2::QuoteExactInputSingleParams {
                tokenIn: USDC_ETH,
                tokenOut: USDT_ETH,
                amountIn: amount_in,
                fee: U24::from(fee),
                sqrtPriceLimitX96: U160::ZERO,
            };
            let quoted = quoter.quoteExactInputSingle(params).block(block).call().await?;

            println!("Amount in: {}, Simulated: {}, Quoted: {}", amount_in, simulated, quoted.amountOut);

            assert_eq!(
                simulated, quoted.amountOut,
                "Mismatch for amount_in {}: simulated={}, quoted={}",
                amount_in, simulated, quoted.amountOut
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_simulate_swap_weth_usdt() -> Result<()> {
        dotenv::dotenv().ok();
        let provider_url = std::env::var("ETHEREUM_PROVIDER")?;
        let provider = Arc::new(ProviderBuilder::new().connect_http(provider_url.parse()?));

        let factory = IPancakeV3FactoryExtInstance::new(PANCAKE_V3_FACTORY_ETH, provider.clone());

        let (pool_addr, fee) = find_pool(&factory, WETH_ETH, USDT_ETH, &[500, 2500, 10000]).await?;
        if pool_addr.is_zero() {
            println!("Pool not found, skipping test");
            return Ok(());
        }

        let block = BlockId::from(provider.get_block_number().await?);
        let pool = PancakeV3Pool::new(pool_addr).init::<_, _>(block, provider.clone()).await?;

        let quoter = IQuoterV2Instance::new(PANCAKE_V3_QUOTER_ETH, provider.clone());

        println!("Pool address: {:?}", pool_addr);
        println!("Token A: {:?}", pool.token_a.address);
        println!("Token B: {:?}", pool.token_b.address);
        println!("Fee: {}", fee);

        let test_amounts = [
            U256::from(100_000_000_000_000u128),
            U256::from(1_000_000_000_000_000u128),
            U256::from(10_000_000_000_000_000u128),
        ];

        for amount_in in test_amounts {
            let simulated = pool.simulate_swap(WETH_ETH, USDT_ETH, amount_in)?;

            let params = IQuoterV2::QuoteExactInputSingleParams {
                tokenIn: WETH_ETH,
                tokenOut: USDT_ETH,
                amountIn: amount_in,
                fee: U24::from(fee),
                sqrtPriceLimitX96: U160::ZERO,
            };
            let quoted = quoter.quoteExactInputSingle(params).block(block).call().await?;

            println!("Amount in: {}, Simulated: {}, Quoted: {}", amount_in, simulated, quoted.amountOut);

            assert_eq!(
                simulated, quoted.amountOut,
                "Mismatch for amount_in {}: simulated={}, quoted={}",
                amount_in, simulated, quoted.amountOut
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_simulate_swap_usdt_weth() -> Result<()> {
        dotenv::dotenv().ok();
        let provider_url = std::env::var("ETHEREUM_PROVIDER")?;
        let provider = Arc::new(ProviderBuilder::new().connect_http(provider_url.parse()?));

        let factory = IPancakeV3FactoryExtInstance::new(PANCAKE_V3_FACTORY_ETH, provider.clone());

        let (pool_addr, fee) = find_pool(&factory, WETH_ETH, USDT_ETH, &[500, 2500, 10000]).await?;
        if pool_addr.is_zero() {
            println!("Pool not found, skipping test");
            return Ok(());
        }

        let block = BlockId::from(provider.get_block_number().await?);
        let pool = PancakeV3Pool::new(pool_addr).init::<_, _>(block, provider.clone()).await?;

        let quoter = IQuoterV2Instance::new(PANCAKE_V3_QUOTER_ETH, provider.clone());

        println!("Pool address: {:?}", pool_addr);
        println!("Token A: {:?}", pool.token_a.address);
        println!("Token B: {:?}", pool.token_b.address);
        println!("Fee: {}", fee);

        let test_amounts = [
            U256::from(100_000_000u128),
            U256::from(1_000_000_000u128),
            U256::from(10_000_000_000u128),
            U256::from(100_000_000_000u128),
        ];

        for amount_in in test_amounts {
            let simulated = pool.simulate_swap(USDT_ETH, WETH_ETH, amount_in)?;

            let params = IQuoterV2::QuoteExactInputSingleParams {
                tokenIn: USDT_ETH,
                tokenOut: WETH_ETH,
                amountIn: amount_in,
                fee: U24::from(fee),
                sqrtPriceLimitX96: U160::ZERO,
            };
            let quoted = quoter.quoteExactInputSingle(params).block(block).call().await?;

            println!("Amount in: {}, Simulated: {}, Quoted: {}", amount_in, simulated, quoted.amountOut);

            assert_eq!(
                simulated, quoted.amountOut,
                "Mismatch for amount_in {}: simulated={}, quoted={}",
                amount_in, simulated, quoted.amountOut
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_simulate_swap_bidirectional() -> Result<()> {
        dotenv::dotenv().ok();
        let provider_url = std::env::var("ETHEREUM_PROVIDER")?;
        let provider = Arc::new(ProviderBuilder::new().connect_http(provider_url.parse()?));

        let factory = IPancakeV3FactoryExtInstance::new(PANCAKE_V3_FACTORY_ETH, provider.clone());

        let (pool_addr, fee) = find_pool(&factory, WETH_ETH, USDT_ETH, &[500, 2500, 10000]).await?;
        if pool_addr.is_zero() {
            println!("Pool not found, skipping test");
            return Ok(());
        }

        let block = BlockId::from(provider.get_block_number().await?);
        let pool = PancakeV3Pool::new(pool_addr).init::<_, _>(block, provider.clone()).await?;

        let quoter = IQuoterV2Instance::new(PANCAKE_V3_QUOTER_ETH, provider.clone());

        println!("Pool address: {:?}", pool_addr);
        println!("Fee: {}", fee);

        let weth_to_usdt_amounts = [
            U256::from(100_000_000_000_000_000u128),
            U256::from(500_000_000_000_000_000u128),
            U256::from(1_000_000_000_000_000_000u128),
        ];

        println!("\n=== WETH -> USDT ===");
        for amount_in in weth_to_usdt_amounts {
            let simulated = pool.simulate_swap(WETH_ETH, USDT_ETH, amount_in)?;

            let params = IQuoterV2::QuoteExactInputSingleParams {
                tokenIn: WETH_ETH,
                tokenOut: USDT_ETH,
                amountIn: amount_in,
                fee: U24::from(fee),
                sqrtPriceLimitX96: U160::ZERO,
            };
            let quoted = quoter.quoteExactInputSingle(params).block(block).call().await?;

            println!("WETH->USDT: in={}, out_sim={}, out_quote={}", amount_in, simulated, quoted.amountOut);
            assert_eq!(simulated, quoted.amountOut);
        }

        let usdt_to_weth_amounts = [
            U256::from(100_000_000u128),
            U256::from(1_000_000_000u128),
            U256::from(10_000_000_000u128),
        ];

        println!("\n=== USDT -> WETH ===");
        for amount_in in usdt_to_weth_amounts {
            let simulated = pool.simulate_swap(USDT_ETH, WETH_ETH, amount_in)?;

            let params = IQuoterV2::QuoteExactInputSingleParams {
                tokenIn: USDT_ETH,
                tokenOut: WETH_ETH,
                amountIn: amount_in,
                fee: U24::from(fee),
                sqrtPriceLimitX96: U160::ZERO,
            };
            let quoted = quoter.quoteExactInputSingle(params).block(block).call().await?;

            println!("USDT->WETH: in={}, out_sim={}, out_quote={}", amount_in, simulated, quoted.amountOut);
            assert_eq!(simulated, quoted.amountOut);
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_calculate_price() -> Result<()> {
        dotenv::dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;
        let provider = Arc::new(ProviderBuilder::new().connect_http(rpc_endpoint.parse()?));

        let factory = IPancakeV3FactoryExtInstance::new(PANCAKE_V3_FACTORY_ETH, provider.clone());

        let (pool_addr, _) = find_pool(&factory, WETH_ETH, USDT_ETH, &[500, 2500, 10000]).await?;
        if pool_addr.is_zero() {
            println!("Pool not found, skipping test");
            return Ok(());
        }

        let block = BlockId::from(19000000u64);
        let pool = PancakeV3Pool::new(pool_addr).init::<_, _>(block, provider.clone()).await?;

        let price_weth_usdt = pool.calculate_price(WETH_ETH, USDT_ETH)?;
        println!("WETH price in USDT: {}", price_weth_usdt);

        let price_usdt_weth = pool.calculate_price(USDT_ETH, WETH_ETH)?;
        println!("USDT price in WETH: {}", price_usdt_weth);

        assert!(
            price_weth_usdt > 1500.0 && price_weth_usdt < 5000.0,
            "WETH price out of expected range"
        );
        assert!(price_usdt_weth > 0.0, "USDT price should be positive");

        Ok(())
    }
}

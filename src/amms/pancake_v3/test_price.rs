#[cfg(test)]
mod tests {
    use crate::amms::amm::AutomatedMarketMaker;
    use crate::amms::pancake_v3::PancakeV3Pool;
    use alloy::{
        eips::BlockId,
        primitives::{address, Address},
        providers::ProviderBuilder,
        sol,
    };
    use eyre::Result;
    use tracing_subscriber;

    sol! {
        #[sol(rpc)]
        interface IPancakeV3Factory {
            function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address pool);
        }
    }

    use alloy::primitives::aliases::U24;

    #[tokio::test]
    async fn test_calculate_price() -> Result<()> {
        let _ = tracing_subscriber::fmt::try_init();
        dotenv::dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let provider = ProviderBuilder::new().connect_http(rpc_endpoint.parse()?);

        // PancakeSwap V3 Factory on Ethereum
        let factory_address = address!("0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865");
        let factory = IPancakeV3Factory::new(factory_address, provider.clone());

        let usdt = address!("dac17f958d2ee523a2206206994597c13d831ec7");
        let weth = address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let fee = U24::from(500); // 0.05%

        // Use a recent enough block where PancakeSwap V3 exists on Ethereum (deployed around block 16M+)
        // Block 18M is safe.
        let block_number = BlockId::from(19000000);

        let pool_address = factory
            .getPool(usdt, weth, fee)
            .block(block_number)
            .call()
            .await?;
        println!("USDT/WETH Pool Address: {:?}", pool_address);

        assert!(pool_address != Address::ZERO, "Pool not found");

        let mut pool = PancakeV3Pool::new(pool_address);
        pool = pool.init(block_number, provider.clone()).await?;

        // Calculate WETH price in USDT
        let price_weth_usdt = pool.calculate_price(weth, usdt)?;
        println!("WETH price in USDT: {}", price_weth_usdt);

        // Calculate USDT price in WETH
        let price_usdt_weth = pool.calculate_price(usdt, weth)?;
        println!("USDT price in WETH: {}", price_usdt_weth);

        assert!(
            price_weth_usdt > 1500.0 && price_weth_usdt < 4000.0,
            "WETH price out of expected range at block 19M"
        );
        assert!(price_usdt_weth > 0.0);

        Ok(())
    }
}

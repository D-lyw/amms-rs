#[cfg(test)]
mod tests {
    use crate::amms::amm::AutomatedMarketMaker;
    use crate::amms::sushi_v2::SushiV2Pool;
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
        interface ISushiV2Factory {
            function getPair(address tokenA, address tokenB) external view returns (address pair);
        }
    }

    sol! {
        #[sol(rpc)]
        interface IUniswapV2Pair {
            function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
            function token0() external view returns (address);
            function token1() external view returns (address);
        }
    }

    use crate::amms::Token;

    #[tokio::test]
    async fn test_calculate_price() -> Result<()> {
        let _ = tracing_subscriber::fmt::try_init();
        dotenv::dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let provider = ProviderBuilder::new().connect_http(rpc_endpoint.parse()?);

        // SushiSwap V2 Factory on Ethereum
        let factory_address = address!("C0AEe478e3658e2610c5F7A4A2E1777cE9e4f2Ac");
        let factory = ISushiV2Factory::new(factory_address, provider.clone());

        let usdt = address!("dac17f958d2ee523a2206206994597c13d831ec7");
        let weth = address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");

        // Block 18M
        let block_number = BlockId::from(19000000);

        let pool_address = factory
            .getPair(usdt, weth)
            .block(block_number)
            .call()
            .await?;
        println!("Sushi USDT/WETH Pool Address: {:?}", pool_address);

        assert!(pool_address != Address::ZERO, "Pool not found");

        let pair_contract = IUniswapV2Pair::new(pool_address, provider.clone());
        let reserves = pair_contract
            .getReserves()
            .block(block_number)
            .call()
            .await?;
        let token0 = pair_contract.token0().block(block_number).call().await?;
        let token1 = pair_contract.token1().block(block_number).call().await?;

        let mut pool = SushiV2Pool::new(pool_address);
        pool.reserve_0 = reserves.reserve0.to::<u128>();
        pool.reserve_1 = reserves.reserve1.to::<u128>();

        // We need token decimals.
        // USDT (token0 usually?) check addresses.
        // We can manually set them since we know them.
        let token_a_decimals = if token0 == usdt { 6 } else { 18 };
        let token_b_decimals = if token1 == usdt { 6 } else { 18 };

        pool.token_a = Token::new_with_decimals(token0, token_a_decimals);
        pool.token_b = Token::new_with_decimals(token1, token_b_decimals);

        // Calculate WETH price in USDT
        let price_weth_usdt = pool.calculate_price(weth, usdt)?;
        println!("WETH price in USDT: {}", price_weth_usdt);

        // Calculate USDT price in WETH
        let price_usdt_weth = pool.calculate_price(usdt, weth)?;
        println!("USDT price in WETH: {}", price_usdt_weth);

        assert!(price_weth_usdt > 1500.0 && price_weth_usdt < 4000.0);
        assert!(price_usdt_weth > 0.0);

        Ok(())
    }
}

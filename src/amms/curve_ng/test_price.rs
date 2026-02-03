#[cfg(test)]
mod tests {
    use crate::amms::amm::AutomatedMarketMaker;
    use crate::amms::curve_ng::{CurveNGPool, CurveNGPoolType};
    use alloy::{
        eips::BlockId,
        primitives::address,
        providers::ProviderBuilder,
        rpc::client::ClientBuilder,
        transports::layers::{RetryBackoffLayer, ThrottleLayer},
    };
    use eyre::Result;
    use tracing_subscriber;

    #[tokio::test]
    #[ignore] // TODO: RPC or Pool consistently returning ZeroData for 0xeb16... (TriCrypto-NG)
    async fn test_calculate_price_ng_tricrypto() -> Result<()> {
        let _ = tracing_subscriber::fmt::try_init();
        dotenv::dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let provider = ProviderBuilder::new().connect_http(rpc_endpoint.parse()?);

        // TriCrypto-NG pool: crvUSD/wBTC/wETH
        // Address: 0xeb16ae0052ed37f479f7fe63849198df17669213
        // Coins: crvUSD, wBTC, wETH
        let pool_address = address!("eb16ae0052ed37f479f7fe63849198df17669213");

        let block_number = BlockId::latest();

        let mut pool = CurveNGPool::new(pool_address, CurveNGPoolType::TriCrypto);
        pool = pool.init(block_number, provider.clone()).await?;

        let crv_usd = address!("f939E0A03FB07F59A73314E73794Be0E57ac1B4E");
        let wbtc = address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

        // Price of wBTC in crvUSD (should be huge)
        let price_wbtc_crvusd = pool.calculate_price(wbtc, crv_usd)?;
        println!("wBTC price in crvUSD: {}", price_wbtc_crvusd);

        // Price of wETH in crvUSD
        let price_weth_crvusd = pool.calculate_price(weth, crv_usd)?;
        println!("wETH price in crvUSD: {}", price_weth_crvusd);

        assert!(price_wbtc_crvusd > 40000.0);
        assert!(price_weth_crvusd > 2000.0);

        Ok(())
    }

    #[tokio::test]
    async fn test_calculate_price_ng_stableswap() -> Result<()> {
        let _ = tracing_subscriber::fmt::try_init();
        dotenv::dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        // StableSwap-NG pool: crvUSD/USDC
        // Pool: 0x4DEcE678ceceb27446b35C672dC7d61F30bAD69E
        let pool_address = address!("4DEcE678ceceb27446b35C672dC7d61F30bAD69E");
        let block_number = BlockId::from(19000000);

        let mut pool = CurveNGPool::new(pool_address, CurveNGPoolType::StableSwap);
        pool = pool.init(block_number, provider.clone()).await?;

        let crv_usd = address!("f939E0A03FB07F59A73314E73794Be0E57ac1B4E");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

        let price_crvusd_usdc = pool.calculate_price(crv_usd, usdc)?;
        println!("crvUSD price in USDC: {}", price_crvusd_usdc);

        let price_usdc_crvusd = pool.calculate_price(usdc, crv_usd)?;
        println!("USDC price in crvUSD: {}", price_usdc_crvusd);

        assert!(price_crvusd_usdc > 0.0);
        assert!(price_usdc_crvusd > 0.0);

        Ok(())
    }
}

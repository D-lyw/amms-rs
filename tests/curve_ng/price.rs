use alloy::{eips::BlockId, primitives::address, providers::ProviderBuilder};
use amms::amms::{
    amm::AutomatedMarketMaker,
    curve_ng::{CurveNGPool, CurveNGPoolType},
};
use eyre::Result;

use crate::common::rpc::provider_url_required;

#[tokio::test]
#[ignore] // TODO: RPC or Pool consistently returning ZeroData for 0xeb16... (TriCrypto-NG)
async fn test_calculate_price_ng_tricrypto() -> Result<()> {
    let provider = ProviderBuilder::new().connect_http(provider_url_required()?.parse()?);

    let pool_address = address!("eb16ae0052ed37f479f7fe63849198df17669213");
    let mut pool = CurveNGPool::new(pool_address, CurveNGPoolType::TriCrypto);
    pool = pool.init(BlockId::latest(), provider.clone()).await?;

    let crv_usd = address!("f939E0A03FB07F59A73314E73794Be0E57ac1B4E");
    let wbtc = address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");
    let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

    let price_wbtc_crvusd = pool.calculate_price(wbtc, crv_usd)?;
    let price_weth_crvusd = pool.calculate_price(weth, crv_usd)?;

    assert!(price_wbtc_crvusd > 40000.0);
    assert!(price_weth_crvusd > 2000.0);

    Ok(())
}

#[tokio::test]
async fn test_calculate_price_ng_stableswap() -> Result<()> {
    let provider = ProviderBuilder::new().connect_http(provider_url_required()?.parse()?);

    let pool_address = address!("4DEcE678ceceb27446b35C672dC7d61F30bAD69E");
    let mut pool = CurveNGPool::new(pool_address, CurveNGPoolType::StableSwap);
    pool = pool.init(BlockId::from(19000000), provider.clone()).await?;

    let crv_usd = address!("f939E0A03FB07F59A73314E73794Be0E57ac1B4E");
    let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

    let price_crvusd_usdc = pool.calculate_price(crv_usd, usdc)?;
    let price_usdc_crvusd = pool.calculate_price(usdc, crv_usd)?;

    assert!(price_crvusd_usdc > 0.0);
    assert!(price_usdc_crvusd > 0.0);

    Ok(())
}

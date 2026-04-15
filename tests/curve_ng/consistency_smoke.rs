use alloy::{
    eips::BlockId,
    primitives::{address, U256},
    providers::{Provider, ProviderBuilder},
};
use amms::amms::{
    amm::AutomatedMarketMaker,
    curve_ng::{CurveNGPool, CurveNGPoolType},
};
use eyre::Result;

use crate::common::{quotes::assert_diff_within_ppm, rpc::provider_url_required};

use super::support::{ICurveCryptoPoolNG, ICurveStablePoolNG};

#[tokio::test]
async fn test_consistency_stableswap_ng() -> Result<()> {
    let provider = ProviderBuilder::new().connect_http(provider_url_required()?.parse()?);
    let block = provider.get_block_number().await?;
    let block_id = BlockId::number(block);
    let pool_addr = address!("4dece678ceceb27446b35c672dc7d61f30bad69e");

    let mut pool = CurveNGPool::new(pool_addr, CurveNGPoolType::StableSwap);
    pool = pool.init(block_id, provider.clone()).await?;

    assert!(
        pool.n_coins >= 2,
        "StableSwap-NG pool should have at least 2 coins"
    );
    assert!(pool.amp.is_some(), "StableSwap-NG pool should have amp");

    let i = 0;
    let j = 1;
    let amount_in = U256::from(100) * U256::from(10).pow(U256::from(pool.decimals[i] as u32));

    let local_out = pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;
    let contract = ICurveStablePoolNG::new(pool_addr, provider.clone());
    let chain_out = contract
        .get_dy(i as i128, j as i128, amount_in)
        .block(block_id)
        .call()
        .await?;

    assert_diff_within_ppm(local_out, chain_out, 100);
    Ok(())
}

#[tokio::test]
async fn test_consistency_tricrypto_ng() -> Result<()> {
    let provider = ProviderBuilder::new().connect_http(provider_url_required()?.parse()?);
    let block = provider.get_block_number().await?;
    let block_id = BlockId::number(block);
    let pool_addr = address!("7F86Bf177Dd4F3494b841a37e810A34dD56c829B");

    let mut pool = CurveNGPool::new(pool_addr, CurveNGPoolType::TriCrypto);
    pool = pool.init(block_id, provider.clone()).await?;

    assert!(
        pool.n_coins >= 3,
        "TriCrypto-NG pool should have at least 3 coins, got {}",
        pool.n_coins
    );
    assert!(
        pool.gamma.is_some(),
        "TriCrypto-NG pool should have gamma parameter"
    );

    let i = 0;
    let j = 1;
    let amount_in = U256::from(100) * U256::from(10).pow(U256::from(pool.decimals[i] as u32));

    let local_out = pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;
    let contract = ICurveCryptoPoolNG::new(pool_addr, provider.clone());
    let chain_out = contract
        .get_dy(U256::from(i), U256::from(j), amount_in)
        .block(block_id)
        .call()
        .await?;

    assert_diff_within_ppm(local_out, chain_out, 500);
    Ok(())
}

#[tokio::test]
async fn test_consistency_twocrypto_ng() -> Result<()> {
    let provider = ProviderBuilder::new().connect_http(provider_url_required()?.parse()?);
    let block = provider.get_block_number().await?;
    let block_id = BlockId::number(block);
    let pool_addr = address!("77146B0a1d08B6844376dF6d9da99bA7F1b19e71");

    let mut pool = CurveNGPool::new(pool_addr, CurveNGPoolType::TwoCrypto);
    pool = pool.init(block_id, provider.clone()).await?;

    assert!(
        pool.n_coins >= 2,
        "TwoCrypto-NG pool should have at least 2 coins, got {}",
        pool.n_coins
    );
    assert!(
        pool.gamma.is_some(),
        "TwoCrypto-NG pool should have gamma parameter"
    );

    let i = 0;
    let j = 1;
    let amount_in = U256::from(100) * U256::from(10).pow(U256::from(pool.decimals[i] as u32));

    let local_out = pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;
    let contract = ICurveCryptoPoolNG::new(pool_addr, provider.clone());
    let chain_out = contract
        .get_dy(U256::from(i), U256::from(j), amount_in)
        .block(block_id)
        .call()
        .await?;

    assert_diff_within_ppm(local_out, chain_out, 500);
    Ok(())
}

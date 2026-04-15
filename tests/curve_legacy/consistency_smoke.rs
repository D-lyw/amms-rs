use alloy::{
    eips::BlockId,
    primitives::{address, U256},
    providers::{Provider, ProviderBuilder},
};
use amms::amms::{
    amm::AutomatedMarketMaker,
    curve_legacy::types::{CurveLegacyPool, CurveLegacyPoolType},
};
use eyre::Result;

use crate::common::{quotes::assert_diff_within_ppm, rpc::provider_url_required};

use super::support::{ICurveCryptoPool, ICurveStablePool};

#[tokio::test]
async fn test_consistency_3pool_legacy_stable() -> Result<()> {
    let provider = ProviderBuilder::new().connect_http(provider_url_required()?.parse()?);
    let block = provider.get_block_number().await?;
    let block_id = BlockId::number(block);
    let pool_addr = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");

    let mut pool = CurveLegacyPool::new(pool_addr, CurveLegacyPoolType::StableSwap);
    pool = pool.init(block_id, provider.clone()).await?;

    let i = 0;
    let j = 1;
    let amount_in = U256::from(10000) * U256::from(10).pow(U256::from(18));

    let local_out = pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;

    let contract = ICurveStablePool::new(pool_addr, provider.clone());
    let chain_out = contract
        .get_dy(i as i128, j as i128, amount_in)
        .block(block_id)
        .call()
        .await?;

    assert_diff_within_ppm(local_out, chain_out, 1000);
    Ok(())
}

#[tokio::test]
async fn test_consistency_steth_legacy_stable() -> Result<()> {
    let provider = ProviderBuilder::new().connect_http(provider_url_required()?.parse()?);
    let block = provider.get_block_number().await?;
    let block_id = BlockId::number(block);
    let pool_addr = address!("DC24316b9AE028F1497c275EB9192a3Ea0f67022");

    let mut pool = CurveLegacyPool::new(pool_addr, CurveLegacyPoolType::StableSwap);
    pool = pool.init(block_id, provider.clone()).await?;

    let i = 1;
    let j = 0;
    let amount_in = U256::from(10) * U256::from(10).pow(U256::from(18));

    let local_out = pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;

    let contract = ICurveStablePool::new(pool_addr, provider.clone());
    let chain_out = contract
        .get_dy(i as i128, j as i128, amount_in)
        .block(block_id)
        .call()
        .await?;

    assert_diff_within_ppm(local_out, chain_out, 100);
    Ok(())
}

#[tokio::test]
async fn test_consistency_tricrypto2_legacy_crypto() -> Result<()> {
    let provider = ProviderBuilder::new().connect_http(provider_url_required()?.parse()?);
    let block = provider.get_block_number().await?;
    let block_id = BlockId::number(block);
    let pool_addr = address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46");

    let mut pool = CurveLegacyPool::new(pool_addr, CurveLegacyPoolType::CryptoSwap);
    pool = pool.init(block_id, provider.clone()).await?;

    if pool.out_fee.is_none() || pool.mid_fee.is_none() || pool.fee_gamma.is_none() {
        println!(
            "Skipping Tricrypto2 test: missing fee params (out_fee={:?}, mid_fee={:?}, fee_gamma={:?})",
            pool.out_fee, pool.mid_fee, pool.fee_gamma
        );
        return Ok(());
    }

    let i = 1;
    let j = 0;
    let amount_in = U256::from(1) * U256::from(10).pow(U256::from(8));

    let local_out = pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;

    let contract = ICurveCryptoPool::new(pool_addr, provider.clone());
    let chain_out = contract
        .get_dy(U256::from(i), U256::from(j), amount_in)
        .block(block_id)
        .call()
        .await?;

    assert_diff_within_ppm(local_out, chain_out, 2000);
    Ok(())
}

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

use crate::common::rpc::provider_url_required;

#[tokio::test]
async fn test_curve_legacy_3pool_exact_out() -> Result<()> {
    let provider = ProviderBuilder::new().connect_http(provider_url_required()?.parse()?);
    let block = provider.get_block_number().await?;
    let block_id = BlockId::number(block);

    let pool_addr = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");

    let pool = CurveLegacyPool::new(pool_addr, CurveLegacyPoolType::StableSwap)
        .init(block_id, provider.clone())
        .await?;

    let dai_idx = 0;
    let usdc_idx = 1;
    let target_outs = [
        U256::from(100) * U256::from(10).pow(U256::from(6)),
        U256::from(1000) * U256::from(10).pow(U256::from(6)),
        U256::from(10000) * U256::from(10).pow(U256::from(6)),
    ];

    for target_out in target_outs {
        let amount_in =
            pool.simulate_swap_exact_out(pool.coins[dai_idx], pool.coins[usdc_idx], target_out)?;
        let actual_out =
            pool.simulate_swap(pool.coins[dai_idx], pool.coins[usdc_idx], amount_in)?;

        assert!(
            actual_out >= target_out,
            "Exact-out verification failed: actual_out {} < target_out {}",
            actual_out,
            target_out
        );

        let diff = actual_out - target_out;
        let diff_pct = if target_out > U256::ZERO {
            (diff * U256::from(10000) / target_out).to::<u64>() as f64 / 100.0
        } else {
            0.0
        };
        assert!(diff_pct < 0.1, "Exact-out diff {}% too high", diff_pct);
    }

    Ok(())
}

#[tokio::test]
async fn test_curve_legacy_tricrypto2_exact_out() -> Result<()> {
    let provider = ProviderBuilder::new().connect_http(provider_url_required()?.parse()?);
    let block = provider.get_block_number().await?;
    let block_id = BlockId::number(block);

    let pool_addr = address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46");

    let pool = CurveLegacyPool::new(pool_addr, CurveLegacyPoolType::CryptoSwap)
        .init(block_id, provider.clone())
        .await?;

    let test_cases = vec![
        (
            0,
            1,
            vec![
                U256::from(100000),
                U256::from(1000000),
                U256::from(10000000),
            ],
        ),
        (
            1,
            0,
            vec![
                U256::from(1000) * U256::from(10).pow(U256::from(6)),
                U256::from(10000) * U256::from(10).pow(U256::from(6)),
            ],
        ),
        (
            2,
            0,
            vec![U256::from(1000) * U256::from(10).pow(U256::from(6))],
        ),
    ];

    for (i, j, targets) in test_cases {
        for target_out in targets {
            let amount_in =
                pool.simulate_swap_exact_out(pool.coins[i], pool.coins[j], target_out)?;
            let actual_out = pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;

            assert!(
                actual_out >= target_out,
                "Exact-out verification failed for {}->{}: actual_out {} < target_out {}",
                i,
                j,
                actual_out,
                target_out
            );

            let diff = actual_out - target_out;
            let diff_pct = if target_out > U256::ZERO {
                (diff * U256::from(10000) / target_out).to::<u64>() as f64 / 100.0
            } else {
                0.0
            };
            assert!(diff_pct < 0.1, "Exact-out diff {}% too high", diff_pct);
        }
    }

    Ok(())
}

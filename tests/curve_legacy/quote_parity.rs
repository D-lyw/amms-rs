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

use super::support::{ICurveCryptoPool, ICurveStablePool};

#[tokio::test]
async fn test_curve_legacy_3pool_simulation() -> Result<()> {
    let provider = ProviderBuilder::new().connect_http(provider_url_required()?.parse()?);
    let block = provider.get_block_number().await?;
    let block_id = BlockId::number(block);

    let pool_addr = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");

    let mut pool = CurveLegacyPool::new(pool_addr, CurveLegacyPoolType::StableSwap);
    pool = pool.init(block_id, provider.clone()).await?;

    let contract = ICurveStablePool::new(pool_addr, provider.clone());

    let dai_idx = 0;
    let usdc_idx = 1;
    let amounts = [
        U256::from(1) * U256::from(10).pow(U256::from(18)),
        U256::from(1000) * U256::from(10).pow(U256::from(18)),
        U256::from(100000) * U256::from(10).pow(U256::from(18)),
    ];

    for amount_in in amounts {
        let amount_out_sim =
            pool.simulate_swap(pool.coins[dai_idx], pool.coins[usdc_idx], amount_in)?;
        let amount_out_chain = contract
            .get_dy(dai_idx as i128, usdc_idx as i128, amount_in)
            .block(block_id)
            .call()
            .await?;

        let diff = if amount_out_sim > amount_out_chain {
            amount_out_sim - amount_out_chain
        } else {
            amount_out_chain - amount_out_sim
        };

        if amount_out_chain > U256::ZERO {
            let error_pct =
                (diff * U256::from(10_000) / amount_out_chain).to::<u64>() as f64 / 100.0;
            assert!(error_pct < 0.01, "Sim error {}% too high", error_pct);
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_curve_legacy_tricrypto2_simulation() -> Result<()> {
    let provider = ProviderBuilder::new().connect_http(provider_url_required()?.parse()?);
    let block = provider.get_block_number().await?;
    let block_id = BlockId::number(block);

    let pool_addr = address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46");

    let mut pool = CurveLegacyPool::new(pool_addr, CurveLegacyPoolType::CryptoSwap);
    pool = pool.init(block_id, provider.clone()).await?;

    let contract = ICurveCryptoPool::new(pool_addr, provider.clone());

    let test_cases = vec![
        (
            0,
            1,
            vec![
                U256::from(100) * U256::from(10).pow(U256::from(6)),
                U256::from(10000) * U256::from(10).pow(U256::from(6)),
            ],
        ),
        (1, 0, vec![U256::from(1000), U256::from(10000000)]),
        (
            2,
            0,
            vec![U256::from(1) * U256::from(10).pow(U256::from(18))],
        ),
    ];

    for (i, j, amounts) in test_cases {
        for amount_in in amounts {
            let amount_out_sim = pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;
            let amount_out_chain = contract
                .get_dy(U256::from(i), U256::from(j), amount_in)
                .block(block_id)
                .call()
                .await?;

            let diff = if amount_out_sim > amount_out_chain {
                amount_out_sim - amount_out_chain
            } else {
                amount_out_chain - amount_out_sim
            };

            if amount_out_chain > U256::ZERO {
                if diff <= U256::from(5) {
                    continue;
                }
                let error_pct =
                    (diff * U256::from(1_000_000) / amount_out_chain).to::<u64>() as f64 / 10000.0;
                assert!(
                    error_pct < 0.01,
                    "Sim error {}% too high for {}->{}. Sim: {}, Chain: {}",
                    error_pct,
                    i,
                    j,
                    amount_out_sim,
                    amount_out_chain
                );
            }
        }
    }

    Ok(())
}

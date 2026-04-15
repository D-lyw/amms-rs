use alloy::{
    eips::BlockId,
    primitives::{address, U256},
    providers::ProviderBuilder,
};
use amms::amms::{
    amm::AutomatedMarketMaker,
    curve_legacy::types::{CurveLegacyPool, CurveLegacyPoolType},
};
use eyre::Result;

use crate::common::rpc::provider_url_required;

use super::support::{legacy_pool_matrix, ICurveCryptoPool, ICurveStablePool};

#[tokio::test]
async fn test_curve_legacy_ethx_weth_stored_rates() -> Result<()> {
    let provider = ProviderBuilder::new().connect_http(provider_url_required()?.parse()?);

    let pool_addr = address!("59ab5a5b5d617e478a2479b0cad80da7e2831492");

    let mut pool = CurveLegacyPool::new(pool_addr, CurveLegacyPoolType::StableSwap);
    pool = pool.init(BlockId::latest(), provider.clone()).await?;

    let contract = ICurveStablePool::new(pool_addr, provider.clone());

    let test_cases = vec![
        (0, 1, U256::from(1) * U256::from(10).pow(U256::from(18))),
        (0, 1, U256::from(10) * U256::from(10).pow(U256::from(18))),
        (0, 1, U256::from(100) * U256::from(10).pow(U256::from(18))),
        (1, 0, U256::from(1) * U256::from(10).pow(U256::from(18))),
        (1, 0, U256::from(10) * U256::from(10).pow(U256::from(18))),
    ];

    for (i, j, amount_in) in test_cases {
        let amount_out_sim = pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;
        let amount_out_chain = contract
            .get_dy(i as i128, j as i128, amount_in)
            .call()
            .await?;

        let diff = if amount_out_sim > amount_out_chain {
            amount_out_sim - amount_out_chain
        } else {
            amount_out_chain - amount_out_sim
        };

        let error_pct = if amount_out_chain > U256::ZERO {
            (diff * U256::from(1_000_000) / amount_out_chain).to::<u64>() as f64 / 10000.0
        } else {
            0.0
        };

        assert!(
            error_pct < 0.01,
            "ETHx/WETH sim error {}% too high. Rates: {:?}",
            error_pct,
            pool.rates
        );
    }

    Ok(())
}

#[tokio::test]
async fn test_curve_legacy_all_pools_comprehensive() -> Result<()> {
    let provider = ProviderBuilder::new().connect_http(provider_url_required()?.parse()?);

    let pools = legacy_pool_matrix();

    let mut failed_pools = Vec::new();

    for (name, pool_addr, pool_type) in pools {
        let mut pool = CurveLegacyPool::new(pool_addr, pool_type);
        match pool.init(BlockId::latest(), provider.clone()).await {
            Ok(p) => pool = p,
            Err(e) => {
                failed_pools.push((name, format!("init_failed: {}", e)));
                continue;
            }
        }

        let n_coins = pool.coins.len();
        if n_coins < 2 {
            continue;
        }

        let mut pool_errors = Vec::new();

        for i in 0..n_coins.min(3) {
            for j in 0..n_coins.min(3) {
                if i == j {
                    continue;
                }

                let decimals = if i < pool.decimals.len() {
                    pool.decimals[i]
                } else {
                    18
                };
                let test_amount = U256::from(10).pow(U256::from(decimals as u64));

                let amount_out_sim =
                    match pool.simulate_swap(pool.coins[i], pool.coins[j], test_amount) {
                        Ok(v) => v,
                        Err(e) => {
                            pool_errors.push(format!("{}->{} sim failed: {}", i, j, e));
                            continue;
                        }
                    };

                let amount_out_chain = if pool_type == CurveLegacyPoolType::StableSwap {
                    let contract = ICurveStablePool::new(pool_addr, provider.clone());
                    contract
                        .get_dy(i as i128, j as i128, test_amount)
                        .call()
                        .await
                } else {
                    let contract = ICurveCryptoPool::new(pool_addr, provider.clone());
                    contract
                        .get_dy(U256::from(i), U256::from(j), test_amount)
                        .call()
                        .await
                };

                let amount_out_chain = match amount_out_chain {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                let diff = if amount_out_sim > amount_out_chain {
                    amount_out_sim - amount_out_chain
                } else {
                    amount_out_chain - amount_out_sim
                };

                let error_pct = if amount_out_chain > U256::ZERO {
                    (diff * U256::from(1_000_000) / amount_out_chain).to::<u64>() as f64 / 10000.0
                } else {
                    0.0
                };

                if error_pct >= 0.5 {
                    pool_errors.push(format!("{}->{} error={:.4}%", i, j, error_pct));
                }
            }
        }

        if !pool_errors.is_empty() {
            failed_pools.push((name, pool_errors.join("; ")));
        }
    }

    assert!(
        failed_pools.is_empty(),
        "Some pools failed: {:?}",
        failed_pools
    );
    Ok(())
}

use alloy::{
    eips::BlockId,
    primitives::{Address, U256},
    providers::{Provider, ProviderBuilder},
};
use amms::amms::{
    amm::AutomatedMarketMaker,
    curve_ng::{CurveNGPool, CurveNGPoolType},
};
use eyre::Result;
use std::str::FromStr;

use crate::common::{amounts::sample_amounts, rpc::provider_url};

use super::support::{
    ICurveCryptoPoolMeta, ICurveCryptoPoolNG, YIELDBASIS_SPECIAL_TWOCRYPTO_POOLS,
};

#[tokio::test]
async fn test_ng_yieldbasis_special_twocrypto_init_and_swap_regression() -> Result<()> {
    let rpc_url = match provider_url() {
        Some(url) => url,
        None => {
            println!("Skipping test: ETHEREUM_PROVIDER/ETHEREUM_RPC_URL not set");
            return Ok(());
        }
    };

    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let block = provider.get_block_number().await?;
    let block_id = BlockId::number(block);
    let block_ts = provider
        .get_block(block_id)
        .await?
        .map(|b| b.header.timestamp)
        .unwrap_or_default();

    let mut failures = Vec::new();

    for pool_addr_str in YIELDBASIS_SPECIAL_TWOCRYPTO_POOLS {
        let pool_address = Address::from_str(pool_addr_str)?;
        println!("Targeted YieldBasis TwoCrypto check @ {}", pool_address);

        let meta = ICurveCryptoPoolMeta::new(pool_address, provider.clone());
        let is_ramping = match meta.future_A_gamma_time().block(block_id).call().await {
            Ok(future_time) => future_time > block_ts,
            Err(_) => false,
        };

        if is_ramping {
            failures.push(format!(
                "{} is ramping at block {} - strict 0-diff parity check is disabled",
                pool_address, block
            ));
            continue;
        }

        let pool = CurveNGPool::new(pool_address, CurveNGPoolType::TwoCrypto);
        let pool = match pool.init(block_id, provider.clone()).await {
            Ok(p) => p,
            Err(e) => {
                failures.push(format!("{} init failed: {}", pool_address, e));
                continue;
            }
        };

        if pool.coins.len() < 2 {
            failures.push(format!(
                "{} initialized with too few coins: {} (expected >= 2)",
                pool_address,
                pool.coins.len(),
            ));
            continue;
        }

        let contract = ICurveCryptoPoolNG::new(pool_address, provider.clone());
        for (i, j) in [(0usize, 1usize), (1usize, 0usize)] {
            for amount_in in sample_amounts(pool.balances[i], pool.decimals[i]) {
                let local_out = match pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in) {
                    Ok(v) => v,
                    Err(e) => {
                        failures.push(format!(
                            "{} simulate_swap failed {}->{} amount_in={} err={}",
                            pool_address, i, j, amount_in, e
                        ));
                        continue;
                    }
                };

                let chain_out = match contract
                    .get_dy(U256::from(i), U256::from(j), amount_in)
                    .block(block_id)
                    .call()
                    .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        failures.push(format!(
                            "{} chain get_dy failed {}->{} amount_in={} err={:?}",
                            pool_address, i, j, amount_in, e
                        ));
                        continue;
                    }
                };

                if local_out != chain_out {
                    failures.push(format!(
                        "{} strict parity mismatch {}->{} amount_in={} local_out={} chain_out={}",
                        pool_address, i, j, amount_in, local_out, chain_out
                    ));
                }
            }
        }
    }

    if !failures.is_empty() {
        println!("===== Targeted CurveNG Regression Failures =====");
        for (idx, msg) in failures.iter().enumerate() {
            println!("{}. {}", idx + 1, msg);
        }
    }

    assert!(
        failures.is_empty(),
        "Targeted CurveNG regression failed with {} issues",
        failures.len()
    );

    Ok(())
}

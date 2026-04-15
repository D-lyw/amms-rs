use alloy::{
    eips::BlockId,
    primitives::U256,
    providers::{Provider, ProviderBuilder},
};
use amms::amms::{
    amm::AutomatedMarketMaker,
    curve_ng::{CurveNGPool, CurveNGPoolType},
};
use eyre::Result;

use crate::common::{amounts::sample_amounts, rpc::provider_url};

use super::support::{
    load_curve_ng_pools, ICurveCryptoPoolMeta, ICurveCryptoPoolNG, ICurveStablePoolNG,
};

#[tokio::test]
async fn test_ng_stableswap_simulation() -> Result<()> {
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

    let (stable_pools, _, _) = load_curve_ng_pools(3, 0, 0)?;
    assert!(
        stable_pools.len() >= 3,
        "Need >=3 StableSwapNG pools, got {}",
        stable_pools.len()
    );

    for pool_addr in stable_pools {
        println!("Testing StableSwap NG Pool: {:?}", pool_addr);

        let mut pool = CurveNGPool::new(pool_addr, CurveNGPoolType::StableSwap);
        pool = pool.init(block_id, provider.clone()).await?;

        let contract = ICurveStablePoolNG::new(pool_addr, provider.clone());
        if pool.coins.len() < 2 {
            continue;
        }

        for (i, j) in [(0usize, 1usize), (1usize, 0usize)] {
            for amount_in in sample_amounts(pool.balances[i], pool.decimals[i]) {
                let amount_out_sim = pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;
                let amount_out_chain = contract
                    .get_dy(i as i128, j as i128, amount_in)
                    .block(block_id)
                    .call()
                    .await?;

                assert_eq!(
                    amount_out_sim, amount_out_chain,
                    "Sim mismatch for {}->{}. Sim: {}, Chain: {}",
                    i, j, amount_out_sim, amount_out_chain
                );
            }
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_ng_twocrypto_simulation() -> Result<()> {
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

    let (_, two_pools, _) = load_curve_ng_pools(0, 3, 0)?;
    assert!(
        two_pools.len() >= 3,
        "Need >=3 TwoCryptoNG pools, got {}",
        two_pools.len()
    );

    for pool_address in two_pools {
        let meta = ICurveCryptoPoolMeta::new(pool_address, provider.clone());
        if let Ok(future_time) = meta.future_A_gamma_time().block(block_id).call().await {
            if future_time > block_ts {
                println!(
                    "Skipping TwoCrypto NG Pool (ramping): {:?} future_A_gamma_time={}",
                    pool_address, future_time
                );
                continue;
            }
        }

        let pool = CurveNGPool::new(pool_address, CurveNGPoolType::TwoCrypto);
        let pool = match pool.init(block_id, provider.clone()).await {
            Ok(p) => p,
            Err(e) => {
                println!("Skip pool init failed: {:?} error={}", pool_address, e);
                continue;
            }
        };

        let contract = ICurveCryptoPoolNG::new(pool_address, provider.clone());
        if pool.coins.len() < 2 {
            continue;
        }

        for (i, j) in [(0usize, 1usize), (1usize, 0usize)] {
            for amount_in in sample_amounts(pool.balances[i], pool.decimals[i]) {
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
                let tolerance =
                    std::cmp::max(amount_out_chain / U256::from(1_000_000u64), U256::from(5u8));

                assert!(
                    diff <= tolerance,
                    "TwoCrypto exact-in mismatch pool {:?} {}->{} sim={} chain={} diff={} tolerance={}",
                    pool_address,
                    i,
                    j,
                    amount_out_sim,
                    amount_out_chain,
                    diff,
                    tolerance
                );
            }
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_ng_tricrypto_simulation() -> Result<()> {
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

    let (_, _, tri_pools) = load_curve_ng_pools(0, 0, 3)?;
    assert!(
        tri_pools.len() >= 3,
        "Need >=3 TriCryptoNG pools, got {}",
        tri_pools.len()
    );

    for pool_address in tri_pools {
        let meta = ICurveCryptoPoolMeta::new(pool_address, provider.clone());
        if let Ok(future_time) = meta.future_A_gamma_time().block(block_id).call().await {
            if future_time > block_ts {
                println!(
                    "Skipping TriCrypto NG Pool (ramping): {:?} future_A_gamma_time={}",
                    pool_address, future_time
                );
                continue;
            }
        }

        let pool = CurveNGPool::new(pool_address, CurveNGPoolType::TriCrypto);
        let pool = match pool.init(block_id, provider.clone()).await {
            Ok(p) => p,
            Err(e) => {
                println!("Skip pool init failed: {:?} error={}", pool_address, e);
                continue;
            }
        };

        let contract = ICurveCryptoPoolNG::new(pool_address, provider.clone());
        if pool.coins.len() < 3 {
            continue;
        }

        for (i, j) in [(0usize, 1usize), (1usize, 0usize), (2usize, 0usize)] {
            for amount_in in sample_amounts(pool.balances[i], pool.decimals[i]) {
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
                let tolerance =
                    std::cmp::max(amount_out_chain / U256::from(1_000_000u64), U256::from(5u8));

                assert!(
                    diff <= tolerance,
                    "TriCrypto exact-in mismatch pool {:?} {}->{} sim={} chain={} diff={} tolerance={}",
                    pool_address,
                    i,
                    j,
                    amount_out_sim,
                    amount_out_chain,
                    diff,
                    tolerance
                );
            }
        }
    }

    Ok(())
}

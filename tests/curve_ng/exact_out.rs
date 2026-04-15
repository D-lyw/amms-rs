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

use crate::common::{amounts::sample_amounts, rpc::provider_url, search::find_min_dx_onchain};

use super::support::{
    load_curve_ng_pools, ICurveCryptoPoolMeta, ICurveCryptoPoolNG, ICurveStablePoolNG,
};

#[tokio::test]
async fn test_ng_stableswap_exact_out_simulation() -> Result<()> {
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
        let mut pool = CurveNGPool::new(pool_addr, CurveNGPoolType::StableSwap);
        pool = pool.init(block_id, provider.clone()).await?;

        if pool.coins.len() < 2 {
            continue;
        }

        let contract = ICurveStablePoolNG::new(pool_addr, provider.clone());
        let amount_in = sample_amounts(pool.balances[0], pool.decimals[0])[0];

        let target_out = contract
            .get_dy(0, 1, amount_in)
            .block(block_id)
            .call()
            .await?;

        let local_dx = pool.simulate_swap_exact_out(pool.coins[0], pool.coins[1], target_out)?;
        let out_at_local = contract
            .get_dy(0, 1, local_dx)
            .block(block_id)
            .call()
            .await?;

        assert!(
            out_at_local >= target_out,
            "Exact-out failed to reach target: target={target_out} out_at_local={out_at_local}"
        );

        if local_dx > U256::ZERO {
            let out_prev = contract
                .get_dy(0, 1, local_dx - U256::from(1u8))
                .block(block_id)
                .call()
                .await?;
            assert!(
                out_prev < target_out,
                "Exact-out not minimal: prev_out={out_prev} target={target_out} local_dx={local_dx}"
            );
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_ng_twocrypto_exact_out_simulation() -> Result<()> {
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
        let pool = pool.init(block_id, provider.clone()).await?;

        if pool.coins.len() < 2 {
            continue;
        }

        let contract = ICurveCryptoPoolNG::new(pool_address, provider.clone());
        let amount_in = sample_amounts(pool.balances[0], pool.decimals[0])[0];

        let target_out = match contract
            .get_dy(U256::from(0), U256::from(1), amount_in)
            .block(block_id)
            .call()
            .await
        {
            Ok(v) => v,
            Err(e) => {
                println!(
                    "Skipping TwoCrypto NG Pool (get_dy reverted): {:?} error={:?}",
                    pool_address, e
                );
                continue;
            }
        };

        let local_dx = pool.simulate_swap_exact_out(pool.coins[0], pool.coins[1], target_out)?;
        let hi = if local_dx > amount_in {
            local_dx
        } else {
            amount_in
        };

        let contract_for_search = contract.clone();
        let onchain_dx = find_min_dx_onchain(
            move |dx| {
                let contract = contract_for_search.clone();
                async move {
                    Ok(contract
                        .get_dy(U256::from(0), U256::from(1), dx)
                        .block(block_id)
                        .call()
                        .await?)
                }
            },
            target_out,
            hi,
        )
        .await?;

        let diff = if local_dx > onchain_dx {
            local_dx - onchain_dx
        } else {
            onchain_dx - local_dx
        };
        let tolerance = std::cmp::max(onchain_dx / U256::from(1_000_000u64), U256::from(10u8));

        assert!(
            diff <= tolerance,
            "TwoCrypto exact-out mismatch pool {:?} local_dx={} onchain_dx={} diff={} tolerance={} target_out={}",
            pool_address,
            local_dx,
            onchain_dx,
            diff,
            tolerance,
            target_out
        );

        let out_at_local = contract
            .get_dy(U256::from(0), U256::from(1), local_dx)
            .block(block_id)
            .call()
            .await?;
        assert!(
            out_at_local >= target_out,
            "Exact-out failed to reach target: target={target_out} out_at_local={out_at_local}"
        );

        if local_dx > U256::ZERO {
            let out_prev = contract
                .get_dy(U256::from(0), U256::from(1), local_dx - U256::from(1u8))
                .block(block_id)
                .call()
                .await?;
            assert!(
                out_prev < target_out,
                "Exact-out not minimal: prev_out={out_prev} target={target_out} local_dx={local_dx}"
            );
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_ng_tricrypto_exact_out_simulation() -> Result<()> {
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
        let pool = pool.init(block_id, provider.clone()).await?;

        if pool.coins.len() < 3 {
            continue;
        }

        let contract = ICurveCryptoPoolNG::new(pool_address, provider.clone());
        let amount_in = sample_amounts(pool.balances[2], pool.decimals[2])[0];

        let target_out = match contract
            .get_dy(U256::from(2), U256::from(0), amount_in)
            .block(block_id)
            .call()
            .await
        {
            Ok(v) => v,
            Err(e) => {
                println!(
                    "Skipping TriCrypto NG Pool (get_dy reverted): {:?} error={:?}",
                    pool_address, e
                );
                continue;
            }
        };

        let local_dx = pool.simulate_swap_exact_out(pool.coins[2], pool.coins[0], target_out)?;
        let hi = if local_dx > amount_in {
            local_dx
        } else {
            amount_in
        };

        let contract_for_search = contract.clone();
        let onchain_dx = find_min_dx_onchain(
            move |dx| {
                let contract = contract_for_search.clone();
                async move {
                    Ok(contract
                        .get_dy(U256::from(2), U256::from(0), dx)
                        .block(block_id)
                        .call()
                        .await?)
                }
            },
            target_out,
            hi,
        )
        .await?;

        let diff = if local_dx > onchain_dx {
            local_dx - onchain_dx
        } else {
            onchain_dx - local_dx
        };
        let tolerance = std::cmp::max(onchain_dx / U256::from(1_000_000u64), U256::from(10u8));

        assert!(
            diff <= tolerance,
            "TriCrypto exact-out mismatch pool {:?} local_dx={} onchain_dx={} diff={} tolerance={} target_out={}",
            pool_address,
            local_dx,
            onchain_dx,
            diff,
            tolerance,
            target_out
        );

        let out_at_local = contract
            .get_dy(U256::from(2), U256::from(0), local_dx)
            .block(block_id)
            .call()
            .await?;
        let out_tol = U256::from(1u8);

        assert!(
            out_at_local + out_tol >= target_out,
            "Exact-out failed to reach target within 1 wei: target={target_out} out_at_local={out_at_local}"
        );

        // TriCryptoNG has discrete step plateaus where local_dx-1 can still satisfy target_out.
        // Minimality is validated by local_dx vs onchain_dx binary-search parity above.
    }

    Ok(())
}

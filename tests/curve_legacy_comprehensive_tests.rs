use alloy::{
    primitives::{address, U256},
    providers::ProviderBuilder,
    sol,
};
use amms::amms::{
    amm::AutomatedMarketMaker,
    curve_legacy::types::{CurveLegacyPool, CurveLegacyPoolType},
};
use eyre::Result;
use std::env;

sol! {
    #[sol(rpc)]
    interface ICurveStablePool {
        function get_dy(int128 i, int128 j, uint256 dx) external view returns (uint256);
    }

    #[sol(rpc)]
    interface ICurveCryptoPool {
        function get_dy(uint256 i, uint256 j, uint256 dx) external view returns (uint256);
    }
}

#[tokio::test]
async fn test_curve_legacy_ethx_weth_stored_rates() -> Result<()> {
    dotenv::dotenv().ok();
    let rpc_url = env::var("ETHEREUM_PROVIDER").expect("ETHEREUM_PROVIDER must be set");
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    let pool_addr = address!("59ab5a5b5d617e478a2479b0cad80da7e2831492");

    let mut pool = CurveLegacyPool::new(pool_addr, CurveLegacyPoolType::StableSwap);
    pool = pool
        .init(alloy::eips::BlockId::latest(), provider.clone())
        .await?;

    println!("\n=== ETHx/WETH Pool ===");
    println!("Pool: {}", pool_addr);
    println!("Coins: {:?}", pool.coins);
    println!("Stable Type: {:?}", pool.stable_type);
    println!("Rates: {:?}", pool.rates);

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
        let amount_out_chain = contract.get_dy(i as i128, j as i128, amount_in).call().await?;

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

        println!(
            "  {}->{}: In={}, Sim={}, Chain={}, Diff={:.4}%",
            i, j, amount_in, amount_out_sim, amount_out_chain, error_pct
        );

        assert!(
            error_pct < 0.01,
            "ETHx/WETH sim error {}% too high. Rates: {:?}",
            error_pct, pool.rates
        );
    }

    Ok(())
}

#[tokio::test]
async fn test_curve_legacy_all_pools_comprehensive() -> Result<()> {
    dotenv::dotenv().ok();
    let rpc_url = env::var("ETHEREUM_PROVIDER").expect("ETHEREUM_PROVIDER must be set");
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    let pools = vec![
        ("rETH/wstETH", address!("447ddd4960d9fdbf6af9a790560d0af76795cb08"), CurveLegacyPoolType::StableSwap),
        ("ETHx/WETH", address!("59ab5a5b5d617e478a2479b0cad80da7e2831492"), CurveLegacyPoolType::StableSwap),
        ("3pool", address!("bebc44782c7db0a1a60cb6fe97d0b483032ff1c7"), CurveLegacyPoolType::StableSwap),
        ("TricryptoUSDT", address!("80466c64868e1ab14a1ddf27a676c3fcbe638fe5"), CurveLegacyPoolType::StableSwap),
        ("FRAX/USDC", address!("dcef968d416a41cdac0ed8702fac8128a64241a2"), CurveLegacyPoolType::StableSwap),
        ("Tricrypto2", address!("d51a44d3fae010294c616388b506acda1bfaae46"), CurveLegacyPoolType::CryptoSwap),
        ("LDO/USDC", address!("3211c6cbef1429da3d0d58494938299c92ad5860"), CurveLegacyPoolType::CryptoSwap),
        ("WETH/B-ether.fi", address!("5fae7e604fc3e24fd43a72867cebac94c65b404a"), CurveLegacyPoolType::CryptoSwap),
        ("WETH/rETH", address!("0f3159811670c117c372428d4e69ac32325e4d0f"), CurveLegacyPoolType::CryptoSwap),
    ];

    let mut failed_pools = Vec::new();
    let mut passed_pools = Vec::new();

    for (name, pool_addr, pool_type) in pools {
        println!("\n=== Testing {} ({}) ===", name, pool_addr);

        let mut pool = CurveLegacyPool::new(pool_addr, pool_type);
        match pool.init(alloy::eips::BlockId::latest(), provider.clone()).await {
            Ok(p) => pool = p,
            Err(e) => {
                println!("  FAILED to init: {}", e);
                failed_pools.push((name, "init_failed".to_string()));
                continue;
            }
        }

        println!("  Coins: {:?}", pool.coins);
        println!("  Stable Type: {:?}", pool.stable_type);
        println!("  Rates: {:?}", pool.rates);

        let n_coins = pool.coins.len();
        if n_coins < 2 {
            println!("  SKIPPED: less than 2 coins");
            continue;
        }

        let mut pool_errors = Vec::new();

        for i in 0..n_coins.min(3) {
            for j in 0..n_coins.min(3) {
                if i == j {
                    continue;
                }

                let decimals = if i < pool.decimals.len() { pool.decimals[i] } else { 18 };
                let test_amount = U256::from(10).pow(U256::from(decimals as u64));

                let amount_out_sim = match pool.simulate_swap(pool.coins[i], pool.coins[j], test_amount) {
                    Ok(v) => v,
                    Err(e) => {
                        println!("  {}->{}: Sim failed: {}", i, j, e);
                        pool_errors.push(format!("{}->{} sim failed", i, j));
                        continue;
                    }
                };

                let amount_out_chain = if pool_type == CurveLegacyPoolType::StableSwap {
                    let contract = ICurveStablePool::new(pool_addr, provider.clone());
                    contract.get_dy(i as i128, j as i128, test_amount).call().await
                } else {
                    let contract = ICurveCryptoPool::new(pool_addr, provider.clone());
                    contract.get_dy(U256::from(i), U256::from(j), test_amount).call().await
                };

                let amount_out_chain = match amount_out_chain {
                    Ok(v) => v,
                    Err(e) => {
                        println!("  {}->{}: Chain call failed: {}", i, j, e);
                        continue;
                    }
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

                println!(
                    "  {}->{}: Sim={}, Chain={}, Error={:.4}%",
                    i, j, amount_out_sim, amount_out_chain, error_pct
                );

                if error_pct >= 0.5 {
                    pool_errors.push(format!("{}->{} error={:.4}%", i, j, error_pct));
                }
            }
        }

        if pool_errors.is_empty() {
            passed_pools.push(name);
        } else {
            failed_pools.push((name, pool_errors.join("; ")));
        }
    }

    println!("\n=== Summary ===");
    println!("Passed: {:?}", passed_pools);
    println!("Failed: {:?}", failed_pools);

    if !failed_pools.is_empty() {
        panic!("Some pools failed: {:?}", failed_pools);
    }

    Ok(())
}

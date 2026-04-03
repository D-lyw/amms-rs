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
async fn test_curve_legacy_3pool_simulation() -> Result<()> {
    let rpc_url =
        env::var("ETHEREUM_PROVIDER").unwrap_or("https://ethereum-rpc.publicnode.com".to_string());
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    // 3pool Address (DAI/USDC/USDT)
    let pool_addr = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");

    let mut pool = CurveLegacyPool::new(pool_addr, CurveLegacyPoolType::StableSwap);
    pool = pool
        .init(alloy::eips::BlockId::latest(), provider.clone())
        .await?;

    println!("3pool Initialized");
    println!("Coins: {:?}", pool.coins);

    let contract = ICurveStablePool::new(pool_addr, provider.clone());

    // Test Cases: DAI -> USDC
    let dai_idx = 0;
    let usdc_idx = 1;

    let amounts = vec![
        U256::from(1) * U256::from(10).pow(U256::from(18)), // 1 DAI
        U256::from(1000) * U256::from(10).pow(U256::from(18)), // 1000 DAI
        U256::from(100000) * U256::from(10).pow(U256::from(18)), // 100k DAI
    ];

    for amount_in in amounts {
        let amount_out_sim =
            pool.simulate_swap(pool.coins[dai_idx], pool.coins[usdc_idx], amount_in)?;

        let amount_out_chain = contract
            .get_dy(dai_idx as i128, usdc_idx as i128, amount_in)
            .call()
            .await?;

        println!(
            "DAI->USDC In: {}, Sim Out: {}, Chain Out: {}",
            amount_in, amount_out_sim, amount_out_chain
        );

        let diff = if amount_out_sim > amount_out_chain {
            amount_out_sim - amount_out_chain
        } else {
            amount_out_chain - amount_out_sim
        };

        if amount_out_chain > U256::ZERO {
            // Allow 0.01% error for StableSwap
            let error_pct =
                (diff * U256::from(10000) / amount_out_chain).to::<u64>() as f64 / 100.0;
            assert!(error_pct < 0.01, "Sim error {}% too high", error_pct);
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_curve_legacy_tricrypto2_simulation() -> Result<()> {
    let rpc_url = env::var("ETHEREUM_PROVIDER").unwrap_or("https://eth.merkle.io".to_string());
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    // Tricrypto2 Address (USDT/WBTC/WETH)
    let pool_addr = address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46");

    let mut pool = CurveLegacyPool::new(pool_addr, CurveLegacyPoolType::CryptoSwap);
    pool = pool
        .init(alloy::eips::BlockId::latest(), provider.clone())
        .await?;

    println!("Tricrypto2 Initialized");

    let contract = ICurveCryptoPool::new(pool_addr, provider.clone());

    // Test cases for multiple directions and amounts
    let test_cases = vec![
        // USDT (0) -> WBTC (1)
        (
            0,
            1,
            vec![
                U256::from(100) * U256::from(10).pow(U256::from(6)), // 100 USDT
                U256::from(10000) * U256::from(10).pow(U256::from(6)), // 10k USDT
            ],
        ),
        // WBTC (1) -> USDT (0)
        (
            1,
            0,
            vec![
                U256::from(1000),     // 1000 satoshi (small)
                U256::from(10000000), // 0.1 BTC
            ],
        ),
        // WETH (2) -> USDT (0)
        (
            2,
            0,
            vec![
                U256::from(1) * U256::from(10).pow(U256::from(18)), // 1 ETH
            ],
        ),
    ];

    for (i, j, amounts) in test_cases {
        for amount_in in amounts {
            let amount_out_sim = pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;

            let amount_out_chain = contract
                .get_dy(U256::from(i), U256::from(j), amount_in)
                .call()
                .await?;

            println!(
                "{}->{} In: {}, Sim Out: {}, Chain Out: {}",
                i, j, amount_in, amount_out_sim, amount_out_chain
            );

            let diff = if amount_out_sim > amount_out_chain {
                amount_out_sim - amount_out_chain
            } else {
                amount_out_chain - amount_out_sim
            };

            // CryptoSwap math is complex
            if amount_out_chain > U256::ZERO {
                // Allow absolute diff of <= 5 units to account for minor rounding differences
                if diff <= U256::from(5) {
                    continue;
                }

                // Precision 0.01%
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

#[tokio::test]
async fn test_curve_legacy_3pool_exact_out() -> Result<()> {
    let rpc_url = env::var("ETHEREUM_PROVIDER").unwrap_or("https://eth.merkle.io".to_string());
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    // 3pool Address (DAI/USDC/USDT)
    let pool_addr = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");

    let pool = CurveLegacyPool::new(pool_addr, CurveLegacyPoolType::StableSwap);
    let pool = pool
        .init(alloy::eips::BlockId::latest(), provider.clone())
        .await?;

    println!("3pool Exact-Out Test");
    println!("Coins: {:?}", pool.coins);

    // Test Cases: DAI -> USDC (exact-out)
    let dai_idx = 0;
    let usdc_idx = 1;

    let target_outs = vec![
        U256::from(100) * U256::from(10).pow(U256::from(6)), // 100 USDC
        U256::from(1000) * U256::from(10).pow(U256::from(6)), // 1000 USDC
        U256::from(10000) * U256::from(10).pow(U256::from(6)), // 10k USDC
    ];

    for target_out in target_outs {
        let amount_in =
            pool.simulate_swap_exact_out(pool.coins[dai_idx], pool.coins[usdc_idx], target_out)?;

        // Verify: use calculated amount_in to do exact-in, should get >= target_out
        let actual_out =
            pool.simulate_swap(pool.coins[dai_idx], pool.coins[usdc_idx], amount_in)?;

        println!(
            "DAI->USDC Target: {}, Amount In: {}, Actual Out: {}",
            target_out, amount_in, actual_out
        );

        assert!(
            actual_out >= target_out,
            "Exact-out verification failed: actual_out {} < target_out {}",
            actual_out,
            target_out
        );

        // Check error: should be minimal (ideally <= 1 unit)
        let diff = actual_out - target_out;
        let diff_pct = if target_out > U256::ZERO {
            (diff * U256::from(10000) / target_out).to::<u64>() as f64 / 100.0
        } else {
            0.0
        };
        println!("Diff: {} ({:.4}%)", diff, diff_pct);

        // Allow up to 0.1% slippage (conservative for binary search)
        assert!(diff_pct < 0.1, "Exact-out diff {}% too high", diff_pct);
    }

    Ok(())
}

#[tokio::test]
async fn test_curve_legacy_tricrypto2_exact_out() -> Result<()> {
    dotenv::dotenv().ok();
    let rpc_url = env::var("ETHEREUM_PROVIDER").unwrap_or("https://rpc.flashbots.net".to_string());
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    // Tricrypto2 Address (USDT/WBTC/WETH)
    let pool_addr = address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46");

    let pool = CurveLegacyPool::new(pool_addr, CurveLegacyPoolType::CryptoSwap);
    let pool = pool
        .init(alloy::eips::BlockId::latest(), provider.clone())
        .await?;

    println!("Tricrypto2 Exact-Out Test");

    // Test cases for exact-out
    let test_cases = vec![
        // USDT (0) -> WBTC (1): target WBTC output
        (
            0,
            1,
            vec![
                U256::from(100000),   // 100000 satoshi ~ 0.001 BTC
                U256::from(1000000),  // 0.01 BTC
                U256::from(10000000), // 0.1 BTC
            ],
        ),
        // WBTC (1) -> USDT (0): target USDT output
        (
            1,
            0,
            vec![
                U256::from(1000) * U256::from(10).pow(U256::from(6)), // 1000 USDT
                U256::from(10000) * U256::from(10).pow(U256::from(6)), // 10k USDT
            ],
        ),
        // WETH (2) -> USDT (0): target USDT output
        (
            2,
            0,
            vec![
                U256::from(1000) * U256::from(10).pow(U256::from(6)), // 1000 USDT
            ],
        ),
    ];

    for (i, j, targets) in test_cases {
        for target_out in targets {
            let amount_in =
                pool.simulate_swap_exact_out(pool.coins[i], pool.coins[j], target_out)?;

            // Verify: use calculated amount_in to do exact-in
            let actual_out = pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;

            println!(
                "{}->{} Target: {}, Amount In: {}, Actual Out: {}",
                i, j, target_out, amount_in, actual_out
            );

            assert!(
                actual_out >= target_out,
                "Exact-out verification failed for {}->{}: actual_out {} < target_out {}",
                i,
                j,
                actual_out,
                target_out
            );

            // Check error
            let diff = actual_out - target_out;
            let diff_pct = if target_out > U256::ZERO {
                (diff * U256::from(10000) / target_out).to::<u64>() as f64 / 100.0
            } else {
                0.0
            };
            println!("Diff: {} ({:.4}%)", diff, diff_pct);

            // Allow up to 0.1% slippage for CryptoSwap (more complex)
            assert!(diff_pct < 0.1, "Exact-out diff {}% too high", diff_pct);
        }
    }

    Ok(())
}

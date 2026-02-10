use alloy::{
    primitives::{address, Address, U256},
    providers::ProviderBuilder,
    sol,
};
use amms::amms::{
    amm::AutomatedMarketMaker,
    curve_ng::{CurveNGPool, CurveNGPoolType},
};
use eyre::Result;
use std::env;

sol! {
    #[sol(rpc)]
    interface ICurveStablePoolNG {
        function get_dy(int128 i, int128 j, uint256 dx) external view returns (uint256);
    }

    #[sol(rpc)]
    interface ICurveCryptoPoolNG {
        function get_dy(uint256 i, uint256 j, uint256 dx) external view returns (uint256);
    }
}

// Helper to find token index
fn find_index(coins: &[Address], target: Address) -> usize {
    coins
        .iter()
        .position(|&c| c == target)
        .expect("Token not found in pool")
}

#[tokio::test]
async fn test_ng_stableswap_simulation() -> Result<()> {
    let rpc_url = env::var("ETHEREUM_RPC_URL").unwrap_or("https://eth.merkle.io".to_string());
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    // StableSwap NG: crvUSD/USDC
    let pool_addr = address!("4DEcE678ceceb27446b35C672dC7d61F30bAD69E");
    println!("Testing StableSwap NG Pool: {:?}", pool_addr);

    let mut pool = CurveNGPool::new(pool_addr, CurveNGPoolType::StableSwap);
    pool = pool
        .init(alloy::eips::BlockId::latest(), provider.clone())
        .await?;

    println!("Pool Initialized");
    println!("Coins: {:?}", pool.coins);

    let contract = ICurveStablePoolNG::new(pool_addr, provider.clone());

    let crv_usd = address!("f939E0A03FB07F59A73314E73794Be0E57ac1B4E"); // 18 dec
    let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"); // 6 dec

    let i_crv_usd = find_index(&pool.coins, crv_usd);
    let i_usdc = find_index(&pool.coins, usdc);

    let test_cases = vec![
        // crvUSD -> USDC
        (
            i_crv_usd,
            i_usdc,
            vec![
                U256::from(1) * U256::from(10).pow(U256::from(18)), // 1 crvUSD
                U256::from(1000) * U256::from(10).pow(U256::from(18)), // 1000 crvUSD
            ],
        ),
        // USDC -> crvUSD
        (
            i_usdc,
            i_crv_usd,
            vec![
                U256::from(1) * U256::from(10).pow(U256::from(6)), // 1 USDC
                U256::from(1000) * U256::from(10).pow(U256::from(6)), // 1000 USDC
            ],
        ),
    ];

    for (i, j, amounts) in test_cases {
        for amount_in in amounts {
            let amount_out_sim = pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;

            let amount_out_chain = contract
                .get_dy(i as i128, j as i128, amount_in)
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

            if amount_out_chain > U256::ZERO {
                // Allow absolute diff of <= 10 units
                if diff <= U256::from(10) {
                    continue;
                }

                // Precision 0.05% (StableSwap NG dynamic fee might have slight mismatch with snapshot)
                let error_pct =
                    (diff * U256::from(1_000_000) / amount_out_chain).to::<u64>() as f64 / 10000.0;
                assert!(
                    error_pct < 0.05,
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
async fn test_ng_twocrypto_simulation() -> Result<()> {
    let rpc_url = env::var("ETHEREUM_RPC_URL").unwrap_or("https://eth.merkle.io".to_string());
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    // TwoCrypto NG: Valid Liquid Pool found on Fork
    // Pool: 0xca546ae6c3b2bb9fba2b6e5eeb0881097cece5b0 (D ~ 3.3e23)
    let pool_address = address!("ca546ae6c3b2bb9fba2b6e5eeb0881097cece5b0");
    println!("Testing TwoCrypto NG Pool: {:?}", pool_address);

    let mut pool = CurveNGPool::new(pool_address, CurveNGPoolType::TwoCrypto);
    pool = pool
        .init(alloy::eips::BlockId::latest(), provider.clone())
        .await?;

    println!("Pool Initialized");
    println!("Coins: {:?}", pool.coins);

    let contract = ICurveCryptoPoolNG::new(pool_address, provider.clone());

    let crv_usd = address!("f939E0A03FB07F59A73314E73794Be0E57ac1B4E"); // Coin 0
    let other_token = address!("1cfa5641c01406ab8ac350ded7d735ec41298372"); // Coin 1 (e.g. wstETH or similar?)

    let i_crv_usd = find_index(&pool.coins, crv_usd);
    let i_other = find_index(&pool.coins, other_token);

    let test_cases = vec![
        // crvUSD -> Token
        (
            i_crv_usd,
            i_other,
            vec![
                U256::from(1000) * U256::from(10).pow(U256::from(18)), // 1000 crvUSD
                U256::from(10000) * U256::from(10).pow(U256::from(18)), // 10k crvUSD
            ],
        ),
        // Token -> crvUSD
        (
            i_other,
            i_crv_usd,
            vec![
                U256::from(1) * U256::from(10).pow(U256::from(18)), // 1 Token unit
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

            if amount_out_chain > U256::ZERO {
                // Allow error
                if diff <= U256::from(100) {
                    // CryptoSwap might have larger rounding
                    continue;
                }
                let error_pct =
                    (diff * U256::from(1_000_000) / amount_out_chain).to::<u64>() as f64 / 10000.0;
                assert!(
                    error_pct < 0.05,
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
async fn test_ng_tricrypto_simulation() -> Result<()> {
    let rpc_url = env::var("ETHEREUM_RPC_URL").unwrap_or("https://eth.merkle.io".to_string());
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    let pool_address = address!("7f86bf177dd4f3494b841a37e810a34dd56c829b");
    println!("Testing Tricrypto NG Pool: {:?}", pool_address);

    let mut pool = CurveNGPool::new(pool_address, CurveNGPoolType::TriCrypto);
    pool = pool
        .init(alloy::eips::BlockId::latest(), provider.clone())
        .await?;

    println!("Pool Initialized");
    println!("Coins: {:?}", pool.coins);

    // ICurveCryptoPoolNG works for TriCrypto too (get_dy interface is same)
    let contract = ICurveCryptoPoolNG::new(pool_address, provider.clone());

    let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"); // 6 dec
    let wbtc = address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599"); // 8 dec
    let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"); // 18 dec

    let i_usdc = find_index(&pool.coins, usdc);
    let i_wbtc = find_index(&pool.coins, wbtc);
    let i_weth = find_index(&pool.coins, weth);

    let test_cases = vec![
        // USDC -> wBTC
        (
            i_usdc,
            i_wbtc,
            vec![
                U256::from(10000) * U256::from(10).pow(U256::from(6)), // 10k USDC
            ],
        ),
        // wBTC -> USDC
        (
            i_wbtc,
            i_usdc,
            vec![
                U256::from(1_000_000), // 0.01 BTC (1M sats)
            ],
        ),
        // wETH -> USDC
        (
            i_weth,
            i_usdc,
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

            if amount_out_chain > U256::ZERO {
                if diff <= U256::from(100) {
                    continue;
                }
                let error_pct =
                    (diff * U256::from(1_000_000) / amount_out_chain).to::<u64>() as f64 / 10000.0;
                assert!(
                    error_pct < 0.05,
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

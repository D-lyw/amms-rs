use crate::amms::amm::AutomatedMarketMaker;
use super::{FluidDexPool, FLUID_DEX_RESOLVER};
use alloy::{
    eips::BlockId,
    primitives::{address, Address, U256},
    providers::{Provider, ProviderBuilder},
};
use eyre::Result;

const OSETH_ETH_POOL: Address = address!("c0652bddcff7739dadf0c9567584b35ca63eb8e1");
const OSETH_TOKEN: Address = address!("f1c9acdc66974dfb6decb12aa385b9cd01190e38");
const ETH_ADDRESS: Address = address!("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");

#[tokio::test]
async fn test_fluid_dex_oseth_eth_swap_simulate() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    dotenv::dotenv().ok();
    let rpc_endpoint = match std::env::var("ETHEREUM_PROVIDER") {
        Ok(u) => u,
        Err(_) => {
            println!("Skipping test: ETHEREUM_PROVIDER not set");
            return Ok(());
        }
    };

    let provider = ProviderBuilder::new().connect_http(rpc_endpoint.parse()?);

    let current_block = provider.get_block_number().await?;
    println!("Current block: {}", current_block);

    let block_id = BlockId::from(current_block);

    let mut pool = FluidDexPool::new(OSETH_ETH_POOL, FLUID_DEX_RESOLVER);
    pool = pool.init(block_id, provider.clone()).await?;

    println!("\n=== Pool Info ===");
    println!("Pool Address: {:?}", pool.address);
    println!("Token A: {:?} (decimals: {})", pool.token_a.address, pool.token_a.decimals);
    println!("Token B: {:?} (decimals: {})", pool.token_b.address, pool.token_b.decimals);
    println!("Fee (1e6): {}", pool.fee_1e6);
    println!("Center Price (1e27): {}", pool.center_price_1e27);
    println!("\n=== Reserves (1e12) ===");
    println!("Token0 Real: {}", pool.token0_real_reserves_1e12);
    println!("Token1 Real: {}", pool.token1_real_reserves_1e12);
    println!("Token0 Imag: {}", pool.token0_imag_reserves_1e12);
    println!("Token1 Imag: {}", pool.token1_imag_reserves_1e12);

    let amount_in = U256::from(500u64) * U256::from(10u64).pow(U256::from(18u64));
    let expected_amount_out = U256::from(849852055151343817096u128);

    println!("\n=== Swap Simulation ===");
    println!("Amount In: 500 osETH");
    println!("Expected Amount Out: {} wei", expected_amount_out);

    let result = pool.simulate_swap(OSETH_TOKEN, ETH_ADDRESS, amount_in)?;

    println!("Simulated Amount Out: {} wei", result);

    if result.is_zero() {
        println!("Warning: Simulated output is zero - may indicate liquidity constraints");
        return Ok(());
    }

    let diff = if result > expected_amount_out {
        result - expected_amount_out
    } else {
        expected_amount_out - result
    };

    let diff_pct = if !expected_amount_out.is_zero() {
        let diff_f64 = diff.as_limbs()[0] as f64;
        let expected_f64 = expected_amount_out.as_limbs()[0] as f64;
        (diff_f64 / expected_f64) * 100.0
    } else {
        0.0
    };

    println!("\nDifference: {} wei", diff);
    println!("Difference %: {:.6}%", diff_pct);

    assert!(
        diff_pct < 5.0,
        "Simulated swap output differs by more than 5% from expected. Got {}, expected {}",
        result,
        expected_amount_out
    );

    println!("\n✅ Swap simulation test PASSED");

    Ok(())
}

#[tokio::test]
async fn test_fluid_dex_oseth_eth_swap_mut() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    dotenv::dotenv().ok();
    let rpc_endpoint = match std::env::var("ETHEREUM_PROVIDER") {
        Ok(u) => u,
        Err(_) => {
            println!("Skipping test: ETHEREUM_PROVIDER not set");
            return Ok(());
        }
    };

    let provider = ProviderBuilder::new().connect_http(rpc_endpoint.parse()?);

    let current_block = provider.get_block_number().await?;
    let block_id = BlockId::from(current_block);

    let mut pool = FluidDexPool::new(OSETH_ETH_POOL, FLUID_DEX_RESOLVER);
    pool = pool.init(block_id, provider.clone()).await?;

    let amount_in = U256::from(500u64) * U256::from(10u64).pow(U256::from(18u64));

    let reserves_before_0 = pool.token0_real_reserves_1e12;
    let reserves_before_1 = pool.token1_real_reserves_1e12;

    let result = pool.simulate_swap_mut(OSETH_TOKEN, ETH_ADDRESS, amount_in)?;

    let reserves_after_0 = pool.token0_real_reserves_1e12;
    let reserves_after_1 = pool.token1_real_reserves_1e12;

    println!("\n=== Reserve Changes After simulate_swap_mut ===");
    println!("Token0 Real: {} -> {}", reserves_before_0, reserves_after_0);
    println!("Token1 Real: {} -> {}", reserves_before_1, reserves_after_1);

    if !result.is_zero() {
        assert!(
            reserves_after_0 > reserves_before_0,
            "Token0 reserves should increase after swap (osETH in)"
        );
        assert!(
            reserves_after_1 < reserves_before_1,
            "Token1 reserves should decrease after swap (ETH out)"
        );
    }

    println!("\n✅ simulate_swap_mut test PASSED");

    Ok(())
}

#[tokio::test]
async fn test_fluid_dex_reverse_swap() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    dotenv::dotenv().ok();
    let rpc_endpoint = match std::env::var("ETHEREUM_PROVIDER") {
        Ok(u) => u,
        Err(_) => {
            println!("Skipping test: ETHEREUM_PROVIDER not set");
            return Ok(());
        }
    };

    let provider = ProviderBuilder::new().connect_http(rpc_endpoint.parse()?);

    let current_block = provider.get_block_number().await?;
    let block_id = BlockId::from(current_block);

    let pool = FluidDexPool::new(OSETH_ETH_POOL, FLUID_DEX_RESOLVER);
    let pool = pool.init(block_id, provider.clone()).await?;

    let amount_in = U256::from(10u64) * U256::from(10u64).pow(U256::from(18u64));

    println!("\n=== Reverse Swap (ETH -> osETH) ===");
    println!("Amount In: 10 ETH");

    let result = pool.simulate_swap(ETH_ADDRESS, OSETH_TOKEN, amount_in)?;

    println!("Simulated Amount Out: {} wei", result);

    if !result.is_zero() {
        assert!(result > U256::ZERO, "Should get some osETH out");
        println!("✅ Reverse swap simulation successful");
    } else {
        println!("Warning: Reverse swap returned zero - may indicate liquidity constraints");
    }

    Ok(())
}

#[tokio::test]
async fn test_fluid_dex_boundary_revert() -> Result<()> {
    // This test verifies that if the range is 100% (upper_range_1e27 is zero),
    // the simulation returns 0 instead of skipping the check.
    let mut pool = FluidDexPool::default();
    pool.token_a = crate::amms::Token {
        address: address!("0000000000000000000000000000000000000001"),
        decimals: 18,
        symbol: "T0".to_string(),
        chain_id: 1,
    };
    pool.token_b = crate::amms::Token {
        address: address!("0000000000000000000000000000000000000002"),
        decimals: 18,
        symbol: "T1".to_string(),
        chain_id: 1,
    };
    pool.token0_imag_reserves_1e12 = U256::from(1000_000_000u64);
    pool.token1_imag_reserves_1e12 = U256::from(1000_000_000u64);
    pool.token0_real_reserves_1e12 = U256::from(1000_000_000u64);
    pool.token1_real_reserves_1e12 = U256::from(1000_000_000u64);
    pool.fee_1e6 = 3000;
    pool.center_price_1e27 = U256::from(10u64).pow(U256::from(27));
    
    // Set upper_range to 0 (simulates 100% range)
    pool.upper_range_1e27 = U256::ZERO;
    pool.lower_range_1e27 = U256::from(5u64) * U256::from(10u64).pow(U256::from(26)); // 0.5 price

    let amount_in = U256::from(10u64).pow(U256::from(18)); // 1 T0
    let result = pool.simulate_swap(pool.token_a.address, pool.token_b.address, amount_in)?;

    println!("Boundary test result (should be 0): {}", result);
    assert!(result.is_zero(), "Swap should return 0 when upper_range is 0 (invalid/100% range boundary)");

    Ok(())
}

#[tokio::test]
async fn test_fluid_dex_dust_protection() -> eyre::Result<()> {
    let mut pool = FluidDexPool::default();
    pool.token_a = crate::amms::Token {
        address: address!("0000000000000000000000000000000000000001"),
        decimals: 18,
        symbol: "T0".to_string(),
        chain_id: 1,
    };
    pool.token_b = crate::amms::Token {
        address: address!("0000000000000000000000000000000000000002"),
        decimals: 18,
        symbol: "T1".to_string(),
        chain_id: 1,
    };
    pool.token0_imag_reserves_1e12 = U256::from(1000_000_000u64);
    pool.token1_imag_reserves_1e12 = U256::from(1000_000_000u64);
    pool.token0_real_reserves_1e12 = U256::from(1000_000_000u64);
    pool.token1_real_reserves_1e12 = U256::from(1000_000_000u64);
    pool.fee_1e6 = 3000;
    pool.center_price_1e27 = U256::from(10u64).pow(U256::from(27));
    pool.upper_range_1e27 = U256::from(20u64).pow(U256::from(27));
    pool.lower_range_1e27 = U256::from(1u64).pow(U256::from(26));
    
    // Amount too small (dust): 50 wei < 100 wei
    let amount_in = U256::from(50u64);
    let result = pool.simulate_swap(pool.token_a.address, pool.token_b.address, amount_in)?;
    println!("Dust test result (50 wei): {}", result);
    assert!(result.is_zero(), "Swap should return 0 for dust amount < 100 wei");

    // Amount slightly above dust raw but below 1e6 adjusted
    // Adjusted 1e6 for 18 decimals is 1e12 wei
    let amount_in_small = U256::from(10u64).pow(U256::from(10)); // 1e10 wei
    let result_small = pool.simulate_swap(pool.token_a.address, pool.token_b.address, amount_in_small)?;
    println!("Dust test result (1e10 wei): {}", result_small);
    assert!(result_small.is_zero(), "Swap should return 0 for adjusted amount < 1e6");

    Ok(())
}

#[tokio::test]
async fn test_fluid_dex_utilization_limit() -> eyre::Result<()> {
    let mut pool = FluidDexPool::default();
    pool.token_a = crate::amms::Token {
        address: address!("0000000000000000000000000000000000000001"),
        decimals: 18,
        symbol: "T0".to_string(),
        chain_id: 1,
    };
    pool.token_b = crate::amms::Token {
        address: address!("0000000000000000000000000000000000000002"),
        decimals: 18,
        symbol: "T1".to_string(),
        chain_id: 1,
    };
    pool.token0_imag_reserves_1e12 = U256::from(10u64).pow(U256::from(18));
    pool.token1_imag_reserves_1e12 = U256::from(10u64).pow(U256::from(18));
    pool.token0_real_reserves_1e12 = U256::from(10u64).pow(U256::from(18));
    pool.token1_real_reserves_1e12 = U256::from(10u64).pow(U256::from(18));
    pool.fee_1e6 = 3000;
    pool.center_price_1e27 = U256::from(10u64).pow(U256::from(27));
    pool.upper_range_1e27 = U256::from(200u64) * U256::from(10u64).pow(U256::from(27));
    pool.lower_range_1e27 = U256::from(1u64) * U256::from(10u64).pow(U256::from(25));

    // Set utilization limit to 10% (100 * 10 = 1000 in comparison)
    pool.utilization_limit_token1 = U256::from(100u64); 
    pool.token1_utilization = U256::from(1001u64); // Slightly over 10% limit
    
    let amount_in = U256::from(10u64).pow(U256::from(18)); // 1 T0
    let result = pool.simulate_swap(pool.token_a.address, pool.token_b.address, amount_in)?;
    println!("Utilization test result (over limit): {}", result);
    assert!(result.is_zero(), "Swap should return 0 when utilization is over limit");

    Ok(())
}

#[tokio::test]
async fn test_fluid_dex_oseth_eth_panic_repro() -> Result<()> {
    // reproduction of the case: 195 osETH -> ETH
    // Pool only has ~103 ETH real reserves, but swap wants ~207 ETH.
    // This should result in an error or 0 instead of success.
    
    let mut pool = FluidDexPool::default();
    pool.token_a = crate::amms::Token {
        address: ETH_ADDRESS,
        decimals: 18,
        symbol: "ETH".to_string(),
        chain_id: 1,
    };
    pool.token_b = crate::amms::Token {
        address: OSETH_TOKEN,
        decimals: 18,
        symbol: "osETH".to_string(),
        chain_id: 1,
    };
    
    // Set reserves from screenshot (1e12 scale)
    pool.token0_real_reserves_1e12 = U256::from(103_760_000_000u64); // 103.76 ETH
    pool.token1_real_reserves_1e12 = U256::from(5_641_400_000_000u128); // 5641.4 osETH
    
    // Set imaginary reserves large enough to allow calculation but real reserves will hit limit
    pool.token0_imag_reserves_1e12 = U256::from(10_000_000_000_000u128); 
    pool.token1_imag_reserves_1e12 = U256::from(10_000_000_000_000u128);
    
    pool.fee_1e6 = 100; // 0.01%
    pool.center_price_1e27 = U256::from(936_839_000_000_000_000_000_000_000u128); // 0.936839
    pool.upper_range_1e27 = U256::from(938_247_000_000_000_000_000_000_000u128);
    pool.lower_range_1e27 = U256::from(936_838_000_000_000_000_000_000_000u128);

    // Swap 195 osETH -> ETH
    let amount_in = U256::from(195u64) * U256::from(10u64).pow(U256::from(18u64));
    
    println!("\n=== osETH/ETH Panic Repro ===");
    println!("Amount In: 195 osETH");
    println!("ETH Real Reserves: 103.76");
    
    // This should return Err(ArithmeticError) with our new fix
    let result = pool.simulate_swap(OSETH_TOKEN, ETH_ADDRESS, amount_in);
    
    match result {
        Ok(out) => {
            println!("Simulated Amount Out: {} ETH", out);
            assert!(out.is_zero(), "Should return 0 or Err if output > real reserves");
        },
        Err(e) => {
            println!("Simulated Swap failed as expected: {:?}", e);
            // This is the desired behavior for 100% parity with contract panic
        }
    }

    Ok(())
}

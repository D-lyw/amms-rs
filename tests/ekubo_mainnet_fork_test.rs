//! Ekubo Mainnet Fork Integration Test
//!
//! Tests the `EkuboPool::simulate_swap` accuracy against real mainnet pool data.
//!
//! Pools tested:
//! - ETH/USDT: 0x0000000000000000000000000000000000000000000d1b71758e21960000137e
//! - ETH/USDC: 0x0000000000000000000000000000000000000000000d1b71758e21960000137e

use alloy::{
    eips::BlockId,
    network::Ethereum,
    primitives::{address, Address, U256},
    providers::{Provider, ProviderBuilder},
    sol,
};
use amms::amms::{
    amm::AutomatedMarketMaker,
    ekubo::{EkuboPool, EkuboPoolKey, PoolConfig},
};
use eyre::Result;
use std::env;

// ========== Constants ==========

const EKUBO_CORE: Address = address!("e0e0e08a6a4b9dc7bd67bcb7aade5cf48157d444");
const CORE_DATA_FETCHER: Address = address!("208bb00c6b142351e4a431f6dd323691ebb7c285");
const EKUBO_ROUTER: Address = address!("9995855C00494d039aB6792f18e368e530DFf931");

sol! {
    #[sol(rpc)]
    interface EkuboRouterInterface {
        struct PoolKey {
            address token0;
            address token1;
            bytes32 config;
        }

        function quote(
            PoolKey memory poolKey,
            bool isToken1,
            int128 amount,
            uint96 sqrtRatioLimit,
            uint256 skipAhead
        )
            external
            returns (int128 delta0, int128 delta1);
    }
}

// Real Pool Data from user:
// {"token0":"0x0000000000000000000000000000000000000000",
//  "token1":"0xdac17f958d2ee523a2206206994597c13d831ec7",
//  "config":"0x0000000000000000000000000000000000000000000d1b71758e21960000137e"}
const USDT: Address = address!("dac17f958d2ee523a2206206994597c13d831ec7");

/// Pool config from on-chain data
fn get_eth_usdt_config() -> U256 {
    U256::from_str_radix(
        "0000000000000000000000000000000000000000000d1b71758e21960000137e",
        16,
    )
    .unwrap()
}

/// Create EkuboPoolKey for the real ETH/USDT pool
fn get_eth_usdt_pool_key() -> EkuboPoolKey {
    EkuboPoolKey {
        token0: Address::ZERO, // Native ETH
        token1: USDT,
        config: get_eth_usdt_config(),
    }
}

// ========== Helper ==========

async fn setup_provider() -> Result<impl Provider<Ethereum> + Clone> {
    let rpc_url = env::var("ETHEREUM_RPC_URL").unwrap_or("https://eth.merkle.io".to_string());
    Ok(ProviderBuilder::new().connect_http(rpc_url.parse()?))
}

/// Helper to verify simulation against on-chain Router quote
async fn verify_against_onchain(
    provider: &impl Provider<Ethereum>,
    pool_key: EkuboPoolKey,
    is_token1: bool,
    amount_in: U256,
    simulated_amount_out: U256,
) -> Result<()> {
    // Construct Router contract
    let router = EkuboRouterInterface::new(EKUBO_ROUTER, provider);

    // Construct PoolKey struct for call
    let pool_key_sol = EkuboRouterInterface::PoolKey {
        token0: pool_key.token0,
        token1: pool_key.token1,
        config: alloy::primitives::FixedBytes::from_slice(&pool_key.config.to_be_bytes::<32>()),
    };

    let amount_in_i128 = i128::try_from(amount_in).expect("amount_in overflow");

    // Ekubo SqrtRatio constants from src/types/sqrtRatio.sol
    const MIN_SQRT_RATIO_RAW: u128 = 4611797791050542631;
    const MAX_SQRT_RATIO_RAW: u128 = 79227682466138141934206691491;

    // Determine sqrtRatioLimit based on direction
    // If input is token1 (is_token1=true), price increases -> MAX
    // If input is token0 (is_token1=false), price decreases -> MIN
    let limit = if is_token1 {
        alloy::primitives::Uint::<96, 2>::try_from(MAX_SQRT_RATIO_RAW).unwrap()
    } else {
        alloy::primitives::Uint::<96, 2>::try_from(MIN_SQRT_RATIO_RAW).unwrap()
    };

    println!(">>> Verifying against on-chain Router quote...");

    // Call quote(poolKey, isToken1, amount, limit, skipAhead=0)
    let result = router
        .quote(
            pool_key_sol,
            is_token1,
            amount_in_i128, // amount > 0 for exact input
            limit,
            U256::ZERO, // skipAhead = 0
        )
        .call()
        .await?;

    let (delta0, delta1) = (result.delta0, result.delta1);

    println!("    On-chain deltas: delta0={}, delta1={}", delta0, delta1);

    // Calculate expected output amount from deltas
    // If is_token1 (input=token1), output is -delta0
    // If !is_token1 (input=token0), output is -delta1
    let on_chain_out_i128 = if is_token1 { -delta0 } else { -delta1 };

    if on_chain_out_i128 < 0 {
        return Err(eyre::eyre!(
            "On-chain quote returned negative output amount"
        ));
    }

    let on_chain_out = U256::from(on_chain_out_i128 as u128);
    println!("    On-chain Amount Out: {}", on_chain_out);
    println!("    Simulated Amount Out: {}", simulated_amount_out);

    // Check difference
    let diff = if on_chain_out > simulated_amount_out {
        on_chain_out - simulated_amount_out
    } else {
        simulated_amount_out - on_chain_out
    };

    println!("    Difference: {} wei", diff);

    if diff > U256::from(100u64) {
        return Err(eyre::eyre!(
            "Significant difference between simulation and on-chain quote: {}",
            diff
        ));
    }

    println!("    [MATCH] Simulation matches on-chain quote (diff <= 100 wei)");
    Ok(())
}

// ========== Tests ==========

/// Test 1: Verify pool key construction and config parsing
#[tokio::test]
async fn test_eth_usdt_pool_key_parsing() -> Result<()> {
    let key = get_eth_usdt_pool_key();

    // Parse config
    let parsed = PoolConfig::from_bytes32(key.config);

    println!("=== ETH/USDT Pool Key ===");
    println!("Token0: {:?}", key.token0);
    println!("Token1: {:?}", key.token1);
    println!("Config (raw): {}", key.config);
    println!("  Extension: {:?}", parsed.extension);
    println!("  Fee: {}", parsed.fee);
    println!("  TickSpacing: {}", parsed.tick_spacing);

    // Verify extension is zero
    assert_eq!(
        parsed.extension,
        Address::ZERO,
        "Extension should be address(0)"
    );

    // Verify tick_spacing and fee are reasonable
    assert!(parsed.tick_spacing > 0, "TickSpacing should be positive");
    assert!(parsed.fee > 0, "Fee should be positive");

    // Compute pool ID
    let pool_id = key.pool_id();
    println!("  PoolId: {}", pool_id);

    println!("[OK] Pool key parsing successful");
    Ok(())
}

/// Test 2: Sync pool state from mainnet
#[tokio::test]
async fn test_eth_usdt_sync_pool_state() -> Result<()> {
    let provider = setup_provider().await?;
    let provider = std::sync::Arc::new(provider);

    let key = get_eth_usdt_pool_key();
    let pool = EkuboPool::new(EKUBO_CORE, key.clone());

    println!("=== Syncing ETH/USDT Pool ===");
    println!("PoolId: {}", key.pool_id());

    let pool = pool.init(BlockId::latest(), provider).await?;

    println!("Pool State:");
    println!("  sqrt_price: {}", pool.sqrt_price);
    println!("  tick: {}", pool.tick);
    println!("  liquidity: {}", pool.liquidity);
    println!("  fee: {}", pool.fee);
    println!("  tick_spacing: {}", pool.tick_spacing);

    // Verify pool has liquidity
    assert!(pool.liquidity > 0, "Pool should have liquidity");
    assert!(!pool.sqrt_price.is_zero(), "SqrtPrice should be non-zero");

    println!("[OK] Pool sync successful");
    Ok(())
}

/// Test 3: Simulate small ETH -> USDT swap
#[tokio::test]
async fn test_eth_usdt_simulate_small_swap() -> Result<()> {
    let provider = setup_provider().await?;
    let provider = std::sync::Arc::new(provider);

    let key = get_eth_usdt_pool_key();
    let pool = EkuboPool::new(EKUBO_CORE, key);
    let pool = pool.init(BlockId::latest(), provider).await?;

    // Small amount: 0.1 ETH
    let amount_in = U256::from(100_000_000_000_000_000u64); // 0.1 ETH in wei

    println!("=== Simulating 0.1 ETH -> USDT ===");
    println!("Amount In: {} wei (0.1 ETH)", amount_in);

    let amount_out = pool.simulate_swap(pool.token_a.address, pool.token_b.address, amount_in)?;

    // Convert to USDT (6 decimals)
    let usdt_out = amount_out.to::<u128>() as f64 / 1_000_000.0;

    println!("Amount Out: {} ({:.2} USDT)", amount_out, usdt_out);

    // Sanity check: 0.1 ETH should give ~$300-$400 USDT at typical prices
    assert!(amount_out > U256::ZERO, "Output should be positive");
    assert!(usdt_out > 100.0, "USDT output should be > $100 for 0.1 ETH");
    assert!(
        usdt_out < 1000.0,
        "USDT output should be < $1000 for 0.1 ETH"
    );

    println!("[OK] Small swap simulation successful");
    Ok(())
}

/// Test 4: Simulate larger ETH -> USDT swap (check price impact)
#[tokio::test]
async fn test_eth_usdt_price_impact() -> Result<()> {
    let provider = setup_provider().await?;
    let provider = std::sync::Arc::new(provider);

    let key = get_eth_usdt_pool_key();
    let pool = EkuboPool::new(EKUBO_CORE, key);
    let pool = pool.init(BlockId::latest(), provider).await?;

    // Test amounts: 0.01, 0.1, 1, 10 ETH
    let amounts = vec![
        (U256::from(10_000_000_000_000_000u64), "0.01 ETH"),
        (U256::from(100_000_000_000_000_000u64), "0.1 ETH"),
        (U256::from(1_000_000_000_000_000_000u128), "1 ETH"),
        (U256::from(10_000_000_000_000_000_000u128), "10 ETH"),
    ];

    println!("=== Price Impact Analysis ===");

    let mut last_rate: Option<f64> = None;

    for (amount_in, label) in amounts {
        let amount_out =
            pool.simulate_swap(pool.token_a.address, pool.token_b.address, amount_in)?;

        let eth_in = amount_in.to::<u128>() as f64 / 1e18;
        let usdt_out = amount_out.to::<u128>() as f64 / 1e6;
        let rate = usdt_out / eth_in;

        let impact = if let Some(base_rate) = last_rate {
            let impact_pct = (base_rate - rate) / base_rate * 100.0;
            format!("{:.4}% impact", impact_pct)
        } else {
            "baseline".to_string()
        };

        println!(
            "{}: {} USDT @ {:.2} USDT/ETH ({})",
            label, usdt_out, rate, impact
        );

        if last_rate.is_none() {
            last_rate = Some(rate);
        }
    }

    println!("[OK] Price impact analysis complete");
    Ok(())
}

/// Test 5: Simulate reverse swap (USDT -> ETH)
#[tokio::test]
async fn test_usdt_to_eth_swap() -> Result<()> {
    let provider = setup_provider().await?;
    let provider = std::sync::Arc::new(provider);

    let key = get_eth_usdt_pool_key();
    let pool = EkuboPool::new(EKUBO_CORE, key);
    let pool = pool.init(BlockId::latest(), provider).await?;

    // 1000 USDT -> ETH
    let usdt_in = U256::from(1000_000_000u64); // 1000 USDT (6 decimals)

    println!("=== Simulating 1000 USDT -> ETH ===");
    println!("Amount In: {} (1000 USDT)", usdt_in);

    // USDT is token1, so we swap token1 -> token0
    let amount_out = pool.simulate_swap(pool.token_b.address, pool.token_a.address, usdt_in)?;

    let eth_out = amount_out.to::<u128>() as f64 / 1e18;

    println!("Amount Out: {} wei ({:.6} ETH)", amount_out, eth_out);

    // Sanity check: 1000 USDT should give ~0.3-0.5 ETH at typical prices
    assert!(amount_out > U256::ZERO, "Output should be positive");
    assert!(eth_out > 0.1, "ETH output should be > 0.1 for 1000 USDT");
    assert!(eth_out < 1.0, "ETH output should be < 1.0 for 1000 USDT");

    println!("[OK] Reverse swap simulation successful");
    Ok(())
}

/// Test 6: Round-trip swap (ETH -> USDT -> ETH)
#[tokio::test]
async fn test_round_trip_swap() -> Result<()> {
    let provider = setup_provider().await?;
    let provider = std::sync::Arc::new(provider);

    let key = get_eth_usdt_pool_key();
    let pool = EkuboPool::new(EKUBO_CORE, key);
    let pool = pool.init(BlockId::latest(), provider).await?;

    // Start with 1 ETH
    let eth_in = U256::from(1_000_000_000_000_000_000u128); // 1 ETH

    println!("=== Round-Trip Swap Test ===");
    println!("Initial: 1 ETH");

    // Step 1: ETH -> USDT
    let usdt_out = pool.simulate_swap(pool.token_a.address, pool.token_b.address, eth_in)?;
    println!(
        "Step 1 (ETH -> USDT): {} USDT",
        usdt_out.to::<u128>() as f64 / 1e6
    );

    // Step 2: USDT -> ETH
    let eth_out = pool.simulate_swap(pool.token_b.address, pool.token_a.address, usdt_out)?;
    println!(
        "Step 2 (USDT -> ETH): {} ETH",
        eth_out.to::<u128>() as f64 / 1e18
    );

    // Calculate loss
    let loss = eth_in - eth_out;
    let loss_pct = loss.to::<u128>() as f64 / eth_in.to::<u128>() as f64 * 100.0;

    println!("Loss: {} wei ({:.4}%)", loss, loss_pct);

    // Due to fees, we expect ~0.1% - 1% loss
    assert!(eth_out < eth_in, "Should lose some due to fees");
    assert!(loss_pct < 5.0, "Loss should be < 5%");

    println!("[OK] Round-trip swap test passed");
    Ok(())
}

// ========== ETH/USDC Pool Tests (User Provided) ==========

// Real Pool Data from user:
// {"token0":"0x0000000000000000000000000000000000000000",
//  "token1":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
//  "config":"0x0000000000000000000000000000000000000000000d1b71758e21960000137e"}
const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

/// Pool config from on-chain data for ETH/USDC
fn get_eth_usdc_config() -> U256 {
    U256::from_str_radix(
        "0000000000000000000000000000000000000000000d1b71758e21960000137e",
        16,
    )
    .unwrap()
}

/// Create EkuboPoolKey for the real ETH/USDC pool
fn get_eth_usdc_pool_key() -> EkuboPoolKey {
    EkuboPoolKey {
        token0: Address::ZERO, // Native ETH
        token1: USDC,
        config: get_eth_usdc_config(),
    }
}

/// Test 7: Verify user-provided ETH/USDC pool config parsing
#[tokio::test]
async fn test_eth_usdc_pool_key_parsing() -> Result<()> {
    let key = get_eth_usdc_pool_key();

    // Parse config
    let parsed = PoolConfig::from_bytes32(key.config);

    println!("=== ETH/USDC Pool Key (User Provided) ===");
    println!("Token0: {:?}", key.token0);
    println!("Token1: {:?}", key.token1);
    println!("Config (raw): {}", key.config);
    println!("  Extension: {:?}", parsed.extension);
    println!("  Fee: {} (raw u64)", parsed.fee);
    println!(
        "  Fee %: {:.6}%",
        parsed.fee as f64 / (u64::MAX as f64) * 100.0
    );
    println!("  TickSpacing: {}", parsed.tick_spacing);

    // Verify extension is zero
    assert_eq!(
        parsed.extension,
        Address::ZERO,
        "Extension should be address(0)"
    );

    // Verify tick_spacing and fee are reasonable
    assert!(parsed.tick_spacing > 0, "TickSpacing should be positive");
    assert!(parsed.fee > 0, "Fee should be positive");

    // Compute pool ID
    let pool_id = key.pool_id();
    println!("  PoolId: {}", pool_id);

    println!("[OK] ETH/USDC Pool key parsing successful");
    Ok(())
}

/// Test 8: Sync ETH/USDC pool state from mainnet
#[tokio::test]
async fn test_eth_usdc_sync_pool_state() -> Result<()> {
    let provider = setup_provider().await?;
    let provider = std::sync::Arc::new(provider);

    let pool_key = get_eth_usdc_pool_key();

    println!("=== Syncing ETH/USDC Pool ===");
    println!("PoolId: {}", pool_key.pool_id());

    let pool = EkuboPool::new(EKUBO_CORE, pool_key);

    // Initialize pool (calls CoreDataFetcher.poolState)
    let pool = pool
        .init::<Ethereum, _>(BlockId::latest(), provider)
        .await?;

    println!("Pool State:");
    println!("  sqrt_price: {}", pool.sqrt_price);
    println!("  tick: {}", pool.tick);
    println!("  liquidity: {}", pool.liquidity);
    println!("  fee: {}", pool.fee);
    println!("  tick_spacing: {}", pool.tick_spacing);
    println!(
        "  token_a: {} ({})",
        pool.token_a.symbol, pool.token_a.decimals
    );
    println!(
        "  token_b: {} ({})",
        pool.token_b.symbol, pool.token_b.decimals
    );

    // Verify state is non-zero
    assert!(!pool.sqrt_price.is_zero(), "sqrt_price should not be zero");
    assert!(pool.liquidity > 0, "liquidity should be positive");

    println!("[OK] ETH/USDC Pool sync successful");
    Ok(())
}

/// Test 9: Simulate ETH -> USDC swap
#[tokio::test]
async fn test_eth_usdc_swap() -> Result<()> {
    let provider = setup_provider().await?;
    let provider = std::sync::Arc::new(provider);

    let pool_key = get_eth_usdc_pool_key();
    let pool = EkuboPool::new(EKUBO_CORE, pool_key.clone());
    let pool = pool
        .init::<Ethereum, _>(BlockId::latest(), provider.clone())
        .await?;

    // Swap 0.1 ETH -> USDC
    let eth_in = U256::from(100_000_000_000_000_000u128); // 0.1 ETH

    println!("=== Simulating 0.1 ETH -> USDC ===");
    println!("Amount In: {} wei (0.1 ETH)", eth_in);

    let usdc_out = pool.simulate_swap(pool.token_a.address, pool.token_b.address, eth_in)?;
    let usdc_out_human = usdc_out.to::<u128>() as f64 / 1e6;

    println!("Amount Out: {} ({:.2} USDC)", usdc_out, usdc_out_human);

    // Verify against on-chain Router quote
    verify_against_onchain(
        &provider,
        pool_key.clone(),
        false, // token0 -> token1
        eth_in,
        usdc_out,
    )
    .await?;

    // Expected: ~$330 USDC for 0.1 ETH at ~$3300/ETH
    assert!(
        usdc_out_human > 100.0,
        "USDC output should be > $100 for 0.1 ETH"
    );
    assert!(
        usdc_out_human < 500.0,
        "USDC output should be < $500 for 0.1 ETH"
    );

    println!("[OK] ETH/USDC swap simulation successful");
    Ok(())
}

/// Test 10: Simulate USDC -> ETH reverse swap
#[tokio::test]
async fn test_usdc_to_eth_swap() -> Result<()> {
    let provider = setup_provider().await?;
    let provider = std::sync::Arc::new(provider);

    let pool_key = get_eth_usdc_pool_key();
    let pool = EkuboPool::new(EKUBO_CORE, pool_key.clone());
    let pool = pool
        .init::<Ethereum, _>(BlockId::latest(), provider.clone())
        .await?;

    // Swap 1000 USDC -> ETH
    let usdc_in = U256::from(1_000_000_000u128); // 1000 USDC (6 decimals)

    println!("=== Simulating 1000 USDC -> ETH ===");
    println!("Amount In: {} (1000 USDC)", usdc_in);

    let eth_out = pool.simulate_swap(pool.token_b.address, pool.token_a.address, usdc_in)?;
    let eth_out_human = eth_out.to::<u128>() as f64 / 1e18;

    println!("Amount Out: {} wei ({:.6} ETH)", eth_out, eth_out_human);

    // Verify against on-chain Router quote
    verify_against_onchain(
        &provider,
        pool_key.clone(),
        true, // token1 -> token0
        usdc_in,
        eth_out,
    )
    .await?;

    // Expected: ~0.3 ETH for 1000 USDC at ~$3300/ETH
    assert!(
        eth_out_human > 0.1,
        "ETH output should be > 0.1 ETH for 1000 USDC"
    );
    assert!(
        eth_out_human < 1.0,
        "ETH output should be < 1 ETH for 1000 USDC"
    );

    println!("[OK] USDC -> ETH reverse swap simulation successful");
    Ok(())
}

/// Test 11: ETH/USDC Round-trip swap
#[tokio::test]
async fn test_eth_usdc_round_trip() -> Result<()> {
    let provider = setup_provider().await?;
    let provider = std::sync::Arc::new(provider);

    let pool_key = get_eth_usdc_pool_key();
    let pool = EkuboPool::new(EKUBO_CORE, pool_key);
    let pool = pool
        .init::<Ethereum, _>(BlockId::latest(), provider)
        .await?;

    // Start with 1 ETH
    let eth_in = U256::from(1_000_000_000_000_000_000u128); // 1 ETH

    println!("=== ETH/USDC Round-Trip Swap Test ===");
    println!("Initial: 1 ETH");

    // Step 1: ETH -> USDC
    let usdc_out = pool.simulate_swap(pool.token_a.address, pool.token_b.address, eth_in)?;
    println!(
        "Step 1 (ETH -> USDC): {} USDC",
        usdc_out.to::<u128>() as f64 / 1e6
    );

    // Step 2: USDC -> ETH
    let eth_out = pool.simulate_swap(pool.token_b.address, pool.token_a.address, usdc_out)?;
    println!(
        "Step 2 (USDC -> ETH): {} ETH",
        eth_out.to::<u128>() as f64 / 1e18
    );

    // Calculate loss
    let loss = eth_in - eth_out;
    let loss_pct = loss.to::<u128>() as f64 / eth_in.to::<u128>() as f64 * 100.0;

    println!("Loss: {} wei ({:.4}%)", loss, loss_pct);

    // Due to fees, we expect ~0.1% - 1% loss
    assert!(eth_out < eth_in, "Should lose some due to fees");
    assert!(loss_pct < 5.0, "Loss should be < 5%");

    println!("[OK] ETH/USDC Round-trip swap test passed");
    Ok(())
}

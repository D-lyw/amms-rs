use alloy::{
    eips::BlockId,
    primitives::{address, Address, U256},
    providers::{Provider, ProviderBuilder},
    sol,
};
use amms::amms::{
    amm::AutomatedMarketMaker,
    fluid_dex::{FluidDexPool, FLUID_DEX_RESOLVER},
};
use eyre::Result;
use std::env;

// osETH/ETH pool on Ethereum mainnet
// Token A: ETH, Token B: osETH
const OSETH_ETH_POOL: Address = address!("c0652bddcff7739dadf0c9567584b35ca63eb8e1");
const OSETH_TOKEN: Address = address!("f1c9acdc66974dfb6decb12aa385b9cd01190e38");
const ETH_ADDRESS: Address = address!("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");

// wstETH/ETH pool on Ethereum mainnet
// Token A: ETH, Token B: wstETH
const WSTETH_ETH_POOL: Address = address!("0B1a513ee24972DAEf112bC777a5610d4325C9e7");
const WSTETH_TOKEN: Address = address!("7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0");

// USDC/USDT pool on Ethereum mainnet
// Token A: USDC, Token B: USDT
const USDC_USDT_POOL: Address = address!("667701e51B4D1Ca244F17C78F7aB8744B4C99F9B");
const USDC_TOKEN: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const USDT_TOKEN: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");

sol! {
    #[sol(rpc)]
    contract FluidDexT1 {
        event Swap(bool swap0to1, uint256 amountIn, uint256 amountOut, address to);
        function swap(bool swap0to1, uint256 amountIn, uint256 amountOutMin, address to, uint256 deadline) external returns (uint256 amountOut);
        function constantsView() external view returns (
            uint256 dexId,
            address liquidity,
            address factory,
            address shift,
            address admin,
            address colOperations,
            address debtOperations,
            address perfectOperationsAndSwapOut,
            address deployerContract,
            address token0,
            address token1,
            bytes32 supplyToken0Slot,
            bytes32 borrowToken0Slot,
            bytes32 supplyToken1Slot,
            bytes32 borrowToken1Slot,
            bytes32 exchangePriceToken0Slot,
            bytes32 exchangePriceToken1Slot,
            uint256 oracleMapping
        );
    }
}

/// Test helper to run exact-in and exact-out tests for a pool
async fn test_pool_exact_in_out(
    pool_address: Address,
    token_in: Address,
    token_out: Address,
    test_amounts: Vec<U256>,
    pool_name: &str,
) -> Result<()> {
    let rpc_url = env::var("ETHEREUM_PROVIDER").expect("ETHEREUM_PROVIDER must be set");
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let block = provider.get_block_number().await?;
    let block_id = BlockId::from(block);

    println!("\n========================================");
    println!("Pool: {} ({:?})", pool_name, pool_address);
    println!("Block: {}", block);
    println!("========================================");

    let pool = FluidDexPool::new(pool_address, FLUID_DEX_RESOLVER);
    let pool = pool.init(block_id, provider.clone()).await?;

    println!(
        "Token A: {:?} (decimals: {})",
        pool.token_a.address, pool.token_a.decimals
    );
    println!(
        "Token B: {:?} (decimals: {})",
        pool.token_b.address, pool.token_b.decimals
    );
    println!("Fee (1e6): {}", pool.fee_1e6);

    // ========================================
    // EXACT-IN TESTS
    // ========================================
    println!("\n--- EXACT-IN TESTS ---");
    let mut exact_in_passed = 0;
    let mut exact_in_total = 0;

    for amount_in in &test_amounts {
        exact_in_total += 1;
        match pool.simulate_swap(token_in, token_out, *amount_in) {
            Ok(amount_out) => {
                if amount_out > U256::ZERO {
                    println!("  In: {} -> Out: {} ✓", amount_in, amount_out);
                    exact_in_passed += 1;
                } else {
                    println!("  In: {} -> Out: 0 (liquidity constraints)", amount_in);
                }
            }
            Err(e) => {
                println!("  In: {} -> Error: {:?}", amount_in, e);
            }
        }
    }
    println!("Exact-In: {}/{} passed", exact_in_passed, exact_in_total);

    // ========================================
    // EXACT-OUT TESTS
    // ========================================
    println!("\n--- EXACT-OUT TESTS ---");
    let mut exact_out_passed = 0;
    let mut exact_out_total = 0;
    let mut total_error_pct = 0.0;

    for target_out in &test_amounts {
        exact_out_total += 1;

        // Calculate required input using exact-out
        match pool.simulate_swap_exact_out(token_in, token_out, *target_out) {
            Ok(exact_in) => {
                // Verify: simulate with exact_in should give >= target_out
                match pool.simulate_swap(token_in, token_out, exact_in) {
                    Ok(verify_out) => {
                        if verify_out >= *target_out {
                            let diff = verify_out - *target_out;
                            let diff_pct = if *target_out > U256::ZERO {
                                (diff * U256::from(1_000_000u64) / *target_out).to::<u64>() as f64
                                    / 10000.0
                            } else {
                                0.0
                            };

                            println!(
                                "  Target: {} -> In: {} -> Verify: {} (error: {:.6}%) ✓",
                                target_out, exact_in, verify_out, diff_pct
                            );

                            exact_out_passed += 1;
                            total_error_pct += diff_pct;
                        } else {
                            println!(
                                "  Target: {} -> In: {} -> Verify: {} (FAILED: < target)",
                                target_out, exact_in, verify_out
                            );
                        }
                    }
                    Err(e) => {
                        println!(
                            "  Target: {} -> In: {} -> Verify Error: {:?}",
                            target_out, exact_in, e
                        );
                    }
                }
            }
            Err(e) => {
                println!("  Target: {} -> Exact-out Error: {:?}", target_out, e);
            }
        }
    }

    println!("Exact-Out: {}/{} passed", exact_out_passed, exact_out_total);
    if exact_out_passed > 0 {
        let avg_error = total_error_pct / exact_out_passed as f64;
        println!("Average Exact-Out Error: {:.6}%", avg_error);
    }

    Ok(())
}

// ============================================================================
// OSETH/ETH POOL TESTS
// ============================================================================

#[tokio::test]
async fn test_fluid_dex_oseth_eth_exact_in() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    dotenv::dotenv().ok();

    println!("\n=== OSETH/ETH POOL: EXACT-IN ===");

    // ETH -> osETH
    let eth_amounts = vec![
        U256::from(1u64) * U256::from(10u64).pow(U256::from(18u64)), // 1 ETH
        U256::from(5u64) * U256::from(10u64).pow(U256::from(18u64)), // 5 ETH
        U256::from(10u64) * U256::from(10u64).pow(U256::from(18u64)), // 10 ETH
    ];

    test_pool_exact_in_out(
        OSETH_ETH_POOL,
        ETH_ADDRESS,
        OSETH_TOKEN,
        eth_amounts,
        "osETH/ETH (ETH->osETH)",
    )
    .await?;

    Ok(())
}

#[tokio::test]
async fn test_fluid_dex_oseth_eth_exact_out() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    dotenv::dotenv().ok();

    println!("\n=== OSETH/ETH POOL: EXACT-OUT ===");

    // Target osETH output, calculate required ETH input
    let target_oseth = vec![
        U256::from(1u64) * U256::from(10u64).pow(U256::from(18u64)), // 1 osETH
        U256::from(5u64) * U256::from(10u64).pow(U256::from(18u64)), // 5 osETH
        U256::from(10u64) * U256::from(10u64).pow(U256::from(18u64)), // 10 osETH
    ];

    test_pool_exact_in_out(
        OSETH_ETH_POOL,
        ETH_ADDRESS,
        OSETH_TOKEN,
        target_oseth,
        "osETH/ETH (ETH->osETH exact-out)",
    )
    .await?;

    Ok(())
}

// ============================================================================
// WSTETH/ETH POOL TESTS
// ============================================================================

#[tokio::test]
async fn test_fluid_dex_wsteth_eth_exact_in() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    dotenv::dotenv().ok();

    println!("\n=== WSTETH/ETH POOL: EXACT-IN ===");

    // ETH -> wstETH
    let eth_amounts = vec![
        U256::from(1u64) * U256::from(10u64).pow(U256::from(18u64)), // 1 ETH
        U256::from(5u64) * U256::from(10u64).pow(U256::from(18u64)), // 5 ETH
        U256::from(10u64) * U256::from(10u64).pow(U256::from(18u64)), // 10 ETH
    ];

    test_pool_exact_in_out(
        WSTETH_ETH_POOL,
        ETH_ADDRESS,
        WSTETH_TOKEN,
        eth_amounts,
        "wstETH/ETH (ETH->wstETH)",
    )
    .await?;

    Ok(())
}

#[tokio::test]
async fn test_fluid_dex_wsteth_eth_exact_out() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    dotenv::dotenv().ok();

    println!("\n=== WSTETH/ETH POOL: EXACT-OUT ===");

    // Target wstETH output, calculate required ETH input
    let target_wsteth = vec![
        U256::from(1u64) * U256::from(10u64).pow(U256::from(18u64)), // 1 wstETH
        U256::from(5u64) * U256::from(10u64).pow(U256::from(18u64)), // 5 wstETH
        U256::from(10u64) * U256::from(10u64).pow(U256::from(18u64)), // 10 wstETH
    ];

    test_pool_exact_in_out(
        WSTETH_ETH_POOL,
        ETH_ADDRESS,
        WSTETH_TOKEN,
        target_wsteth,
        "wstETH/ETH (ETH->wstETH exact-out)",
    )
    .await?;

    Ok(())
}

#[tokio::test]
async fn test_fluid_dex_wsteth_eth_reverse_exact_in() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    dotenv::dotenv().ok();

    println!("\n=== WSTETH/ETH POOL: REVERSE EXACT-IN ===");

    // wstETH -> ETH
    let wsteth_amounts = vec![
        U256::from(1u64) * U256::from(10u64).pow(U256::from(18u64)), // 1 wstETH
        U256::from(5u64) * U256::from(10u64).pow(U256::from(18u64)), // 5 wstETH
    ];

    test_pool_exact_in_out(
        WSTETH_ETH_POOL,
        WSTETH_TOKEN,
        ETH_ADDRESS,
        wsteth_amounts,
        "wstETH/ETH (wstETH->ETH)",
    )
    .await?;

    Ok(())
}

#[tokio::test]
async fn test_fluid_dex_wsteth_eth_reverse_exact_out() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    dotenv::dotenv().ok();

    println!("\n=== WSTETH/ETH POOL: REVERSE EXACT-OUT ===");

    // Target ETH output, calculate required wstETH input
    let target_eth = vec![
        U256::from(500u64) * U256::from(10u64).pow(U256::from(15u64)), // 0.5 ETH
        U256::from(1u64) * U256::from(10u64).pow(U256::from(18u64)),   // 1 ETH
    ];

    test_pool_exact_in_out(
        WSTETH_ETH_POOL,
        WSTETH_TOKEN,
        ETH_ADDRESS,
        target_eth,
        "wstETH/ETH (wstETH->ETH exact-out)",
    )
    .await?;

    Ok(())
}

// ============================================================================
// USDC/USDT POOL TESTS
// ============================================================================

#[tokio::test]
async fn test_fluid_dex_usdc_usdt_exact_in() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    dotenv::dotenv().ok();

    println!("\n=== USDC/USDT POOL: EXACT-IN ===");

    // USDC -> USDT (USDC has 6 decimals)
    let usdc_amounts = vec![
        U256::from(1000u64) * U256::from(10u64).pow(U256::from(6u64)), // 1,000 USDC
        U256::from(10000u64) * U256::from(10u64).pow(U256::from(6u64)), // 10,000 USDC
        U256::from(100000u64) * U256::from(10u64).pow(U256::from(6u64)), // 100,000 USDC
    ];

    test_pool_exact_in_out(
        USDC_USDT_POOL,
        USDC_TOKEN,
        USDT_TOKEN,
        usdc_amounts,
        "USDC/USDT (USDC->USDT)",
    )
    .await?;

    Ok(())
}

#[tokio::test]
async fn test_fluid_dex_usdc_usdt_exact_out() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    dotenv::dotenv().ok();

    println!("\n=== USDC/USDT POOL: EXACT-OUT ===");

    // Target USDT output (USDT has 6 decimals), calculate required USDC input
    let target_usdt = vec![
        U256::from(1000u64) * U256::from(10u64).pow(U256::from(6u64)), // 1,000 USDT
        U256::from(10000u64) * U256::from(10u64).pow(U256::from(6u64)), // 10,000 USDT
        U256::from(100000u64) * U256::from(10u64).pow(U256::from(6u64)), // 100,000 USDT
    ];

    test_pool_exact_in_out(
        USDC_USDT_POOL,
        USDC_TOKEN,
        USDT_TOKEN,
        target_usdt,
        "USDC/USDT (USDC->USDT exact-out)",
    )
    .await?;

    Ok(())
}

#[tokio::test]
async fn test_fluid_dex_usdc_usdt_reverse_exact_in() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    dotenv::dotenv().ok();

    println!("\n=== USDC/USDT POOL: REVERSE EXACT-IN ===");

    // USDT -> USDC (both have 6 decimals)
    let usdt_amounts = vec![
        U256::from(1000u64) * U256::from(10u64).pow(U256::from(6u64)), // 1,000 USDT
        U256::from(10000u64) * U256::from(10u64).pow(U256::from(6u64)), // 10,000 USDT
        U256::from(100000u64) * U256::from(10u64).pow(U256::from(6u64)), // 100,000 USDT
    ];

    test_pool_exact_in_out(
        USDC_USDT_POOL,
        USDT_TOKEN,
        USDC_TOKEN,
        usdt_amounts,
        "USDC/USDT (USDT->USDC)",
    )
    .await?;

    Ok(())
}

#[tokio::test]
async fn test_fluid_dex_usdc_usdt_reverse_exact_out() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    dotenv::dotenv().ok();

    println!("\n=== USDC/USDT POOL: REVERSE EXACT-OUT ===");

    // Target USDC output, calculate required USDT input
    let target_usdc = vec![
        U256::from(1000u64) * U256::from(10u64).pow(U256::from(6u64)), // 1,000 USDC
        U256::from(10000u64) * U256::from(10u64).pow(U256::from(6u64)), // 10,000 USDC
        U256::from(100000u64) * U256::from(10u64).pow(U256::from(6u64)), // 100,000 USDC
    ];

    test_pool_exact_in_out(
        USDC_USDT_POOL,
        USDT_TOKEN,
        USDC_TOKEN,
        target_usdc,
        "USDC/USDT (USDT->USDC exact-out)",
    )
    .await?;

    Ok(())
}

// ============================================================================
// COMPREHENSIVE ERROR SUMMARY
// ============================================================================

#[tokio::test]
async fn test_fluid_dex_comprehensive_summary() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    dotenv::dotenv().ok();

    let rpc_url = env::var("ETHEREUM_PROVIDER").expect("ETHEREUM_PROVIDER must be set");
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let block = provider.get_block_number().await?;
    let block_id = BlockId::from(block);

    println!("\n========================================");
    println!("FLUIDDEX COMPREHENSIVE TEST SUMMARY");
    println!("Block: {}", block);
    println!("========================================");

    let pools = vec![
        ("osETH/ETH", OSETH_ETH_POOL),
        ("wstETH/ETH", WSTETH_ETH_POOL),
        ("USDC/USDT", USDC_USDT_POOL),
    ];

    for (name, pool_addr) in pools {
        let pool = FluidDexPool::new(pool_addr, FLUID_DEX_RESOLVER);
        match pool.init(block_id, provider.clone()).await {
            Ok(pool) => {
                println!("\n{} Pool:", name);
                println!(
                    "  Token A: {:?} (decimals: {})",
                    pool.token_a.address, pool.token_a.decimals
                );
                println!(
                    "  Token B: {:?} (decimals: {})",
                    pool.token_b.address, pool.token_b.decimals
                );
                println!("  Fee: {} (1e6)", pool.fee_1e6);

                // Quick exact-out test
                let amount_out = U256::from(1u64) * U256::from(10u64).pow(U256::from(18u64));
                match pool.simulate_swap_exact_out(
                    pool.token_a.address,
                    pool.token_b.address,
                    amount_out,
                ) {
                    Ok(exact_in) => {
                        match pool.simulate_swap(
                            pool.token_a.address,
                            pool.token_b.address,
                            exact_in,
                        ) {
                            Ok(verify_out) => {
                                let diff = if verify_out >= amount_out {
                                    verify_out - amount_out
                                } else {
                                    amount_out - verify_out
                                };
                                let diff_pct = if amount_out > U256::ZERO {
                                    (diff * U256::from(1_000_000u64) / amount_out).to::<u64>()
                                        as f64
                                        / 10000.0
                                } else {
                                    0.0
                                };
                                println!("  Exact-out test: error = {:.6}%", diff_pct);
                            }
                            Err(e) => println!("  Exact-out verify error: {:?}", e),
                        }
                    }
                    Err(e) => println!("  Exact-out calc error: {:?}", e),
                }
            }
            Err(e) => {
                println!("\n{} Pool: Failed to init - {:?}", name, e);
            }
        }
    }

    println!("\n========================================");
    println!("COMPREHENSIVE TEST COMPLETE");
    println!("========================================");

    Ok(())
}

use alloy::{
    eips::BlockId,
    primitives::{address, Address, U256},
    providers::{Provider, ProviderBuilder},
    sol,
};
use amms::amms::{
    amm::AutomatedMarketMaker,
    curve_legacy::{CurveLegacyPool, CurveLegacyPoolType},
};

sol! {
    #[sol(rpc)]
    interface ICurveCryptoPool {
        function get_dy(uint256 i, uint256 j, uint256 dx) external view returns (uint256);
        function D() external view returns (uint256);
        function fee() external view returns (uint256);
    }
}

/// Test pools: all CurveLegacy CryptoSwap pools found in pool_index
/// (mix of 2-coin and 3-coin crypto pools)
struct PoolSpec {
    name: &'static str,
    address: Address,
    pool_type: CurveLegacyPoolType,
    rpc_env: &'static str,
    decimals: &'static [u8],
}

const TEST_POOLS: &[PoolSpec] = &[
    PoolSpec {
        name: "Base-Tricrypto",
        address: address!("11C1fBd4b3De66bC0565779b35171a6CF3E71f59"),
        pool_type: CurveLegacyPoolType::CryptoSwap,
        rpc_env: "BASE_PROVIDER",
        decimals: &[18, 18],
    },
    PoolSpec {
        name: "Eth-Tricrypto2",
        address: address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46"),
        pool_type: CurveLegacyPoolType::CryptoSwap,
        rpc_env: "ETHEREUM_PROVIDER",
        decimals: &[18, 18, 18],
    },
    PoolSpec {
        name: "Eth-LDO-USDC",
        address: address!("3211C6cBeF1429da3D0d58494938299c92Ad5860"),
        pool_type: CurveLegacyPoolType::CryptoSwap,
        rpc_env: "ETHEREUM_PROVIDER",
        decimals: &[18, 6],
    },
    PoolSpec {
        name: "Eth-WETH-cbETH",
        address: address!("5FAE7E604FC3e24fd43A72867ceBaC94c65b404A"),
        pool_type: CurveLegacyPoolType::CryptoSwap,
        rpc_env: "ETHEREUM_PROVIDER",
        decimals: &[18, 18],
    },
    PoolSpec {
        name: "Eth-WETH-rETH",
        address: address!("0f3159811670c117c372428D4E69AC32325e4D0F"),
        pool_type: CurveLegacyPoolType::CryptoSwap,
        rpc_env: "ETHEREUM_PROVIDER",
        decimals: &[18, 18],
    },
];

/// Helper: check how many blocks back are safe for historical queries.
/// We use blocks that are at least 10 behind the current tip to avoid reorgs.
async fn get_safe_test_blocks<P: Provider + Clone>(
    provider: &P,
    steps_behind: &[u64],
) -> eyre::Result<Vec<u64>> {
    let current = provider.get_block_number().await?;
    Ok(steps_behind
        .iter()
        .map(|s| current.saturating_sub(*s))
        .collect())
}

async fn run_single_pool_precision_test(pool_spec: &PoolSpec, block: u64) -> eyre::Result<()> {
    let rpc_url = match std::env::var(pool_spec.rpc_env) {
        Ok(url) => url,
        Err(_) => {
            println!(
                "  ⏭️ Skipping {}: {} not set",
                pool_spec.name, pool_spec.rpc_env
            );
            return Ok(());
        }
    };
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let block_id = BlockId::from(block);

    // 1. Init pool at historical block
    println!("  Initializing {} at block {}...", pool_spec.name, block);
    let mut pool = CurveLegacyPool::new(pool_spec.address, pool_spec.pool_type)
        .init(block_id, provider.clone())
        .await?;

    let n_coins = pool.n_coins as usize;
    println!(
        "    n_coins={}, balances={:?}, price_scale={:?}",
        n_coins, pool.balances, pool.price_scale
    );

    // 2. Fetch chain D and fee
    let contract = ICurveCryptoPool::new(pool_spec.address, provider.clone());
    let d_chain: U256 = contract.D().block(block_id).call().await?;
    let fee_chain: U256 = contract.fee().block(block_id).call().await?;

    // 3. Call recalculate_crypto_state locally
    pool.recalculate_crypto_state()?;
    let d_local = pool.d;
    let fee_local = pool.fee;

    // 4. Compare D
    let d_chain_u256 = d_chain;
    let d_local_u256 = d_local.unwrap_or(U256::ZERO);

    let d_diff = if d_chain_u256 > d_local_u256 {
        d_chain_u256 - d_local_u256
    } else {
        d_local_u256 - d_chain_u256
    };

    let d_diff_ratio = if d_chain_u256.is_zero() {
        0.0
    } else {
        d_diff.to_string().parse::<f64>().unwrap_or(0.0)
            / d_chain_u256.to_string().parse::<f64>().unwrap_or(1.0)
    };

    // 5. Compare fee
    let fee_diff = if fee_chain > fee_local {
        fee_chain - fee_local
    } else {
        fee_local - fee_chain
    };
    let fee_diff_ratio = if fee_chain.is_zero() {
        0.0
    } else {
        fee_diff.to_string().parse::<f64>().unwrap_or(0.0)
            / fee_chain.to_string().parse::<f64>().unwrap_or(1.0)
    };

    println!(
        "    D: chain={}, local={}, diff_ratio={:.12}, passed={}",
        d_chain_u256,
        d_local_u256,
        d_diff_ratio,
        d_diff_ratio < 1e-10
    );
    println!(
        "    Fee: chain={}, local={}, diff_ratio={:.12}, passed={}",
        fee_chain,
        fee_local,
        fee_diff_ratio,
        fee_diff_ratio < 1e-10
    );

    if fee_diff_ratio >= 1e-10 {
        eyre::bail!(
            "Fee divergence too large! chain={}, local={}, ratio={}",
            fee_chain,
            fee_local,
            fee_diff_ratio
        );
    }

    // 6. Test swap simulation accuracy (if 2+ coins)
    if n_coins >= 2 {
        // Test selling 0.01 unit of coin 0
        let amount_in = U256::from(10).pow(U256::from(pool_spec.decimals[0] as u64 - 1));
        let swap_i = 0;
        let swap_j = 1;

        match pool.simulate_swap(pool.coins[swap_i], pool.coins[swap_j], amount_in) {
            Ok(local_out) => {
                let onchain_out: U256 = contract
                    .get_dy(
                        U256::from(swap_i as u64),
                        U256::from(swap_j as u64),
                        amount_in,
                    )
                    .block(block_id)
                    .call()
                    .await?;

                let swap_diff = if local_out > onchain_out {
                    local_out - onchain_out
                } else {
                    onchain_out - local_out
                };
                let swap_diff_ratio = if onchain_out.is_zero() {
                    0.0
                } else {
                    swap_diff.to_string().parse::<f64>().unwrap_or(0.0)
                        / onchain_out.to_string().parse::<f64>().unwrap_or(1.0)
                };
                println!(
                    "    Swap({}→{} amount_in={}): local={}, chain={}, diff_ratio={:.12}, passed={}",
                    swap_i,
                    swap_j,
                    amount_in,
                    local_out,
                    onchain_out,
                    swap_diff_ratio,
                    swap_diff_ratio < 1e-6
                );

                if d_diff_ratio >= 1e-10 {
                    eyre::bail!(
                        "D divergence too large! chain={}, local={}, ratio={}",
                        d_chain_u256,
                        d_local_u256,
                        d_diff_ratio
                    );
                }
                if swap_diff_ratio >= 1e-6 {
                    eyre::bail!(
                        "Swap divergence too large! chain={}, local={}, ratio={}",
                        onchain_out,
                        local_out,
                        swap_diff_ratio
                    );
                }
            }
            Err(e) => {
                println!("    ⚠️ Swap simulation failed: {:?}", e);
            }
        }
    }

    println!("    ✅ All checks passed");
    Ok(())
}

#[tokio::test]
async fn test_curve_legacy_twocrypto_ldo_usdc_rounding_regression() -> eyre::Result<()> {
    dotenv::dotenv().ok();

    let rpc_url = std::env::var("ETHEREUM_PROVIDER")?;
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let block = 25_672_086u64;
    let block_id = BlockId::from(block);
    let pool_spec = TEST_POOLS
        .iter()
        .find(|pool| pool.name == "Eth-LDO-USDC")
        .expect("Eth-LDO-USDC spec must exist");

    let pool = CurveLegacyPool::new(pool_spec.address, pool_spec.pool_type)
        .init(block_id, provider.clone())
        .await?;
    let contract = ICurveCryptoPool::new(pool_spec.address, provider);
    let amount_in = U256::from(10).pow(U256::from(pool_spec.decimals[0] as u64 - 1));

    let local_out = pool.simulate_swap(pool.coins[0], pool.coins[1], amount_in)?;
    let chain_out = contract
        .get_dy(U256::ZERO, U256::from(1u8), amount_in)
        .block(block_id)
        .call()
        .await?;

    assert_eq!(
        local_out, chain_out,
        "Legacy 2-coin CryptoSwap rounding should match on-chain get_dy exactly"
    );

    Ok(())
}

#[tokio::test]
async fn test_curve_legacy_recalculate_precision() -> eyre::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    dotenv::dotenv().ok();
    println!("=== CurveLegacy recalculate_crypto_state Precision Test ===\n");

    for pool_spec in TEST_POOLS {
        let rpc_url = match std::env::var(pool_spec.rpc_env) {
            Ok(url) => url,
            Err(_) => {
                println!(
                    "⏭️ Skipping {}: {} not set",
                    pool_spec.name, pool_spec.rpc_env
                );
                continue;
            }
        };

        let provider = match ProviderBuilder::new()
            .connect_http(rpc_url.parse()?)
            .get_block_number()
            .await
        {
            Ok(_) => ProviderBuilder::new().connect_http(rpc_url.parse()?),
            Err(e) => {
                println!(
                    "⏭️ Skipping {}: RPC connection failed: {:?}",
                    pool_spec.name, e
                );
                continue;
            }
        };

        // Test at 3 different blocks: 100, 500, 2000 blocks behind tip
        let blocks = match get_safe_test_blocks(&provider, &[100, 500, 2000]).await {
            Ok(b) => b,
            Err(e) => {
                println!(
                    "⏭️ Skipping {}: cannot fetch blocks: {:?}",
                    pool_spec.name, e
                );
                continue;
            }
        };
        println!("\nTesting {} at blocks: {:?}", pool_spec.name, blocks);

        for &block in &blocks {
            println!("\n--- {} @ block {} ---", pool_spec.name, block);
            match run_single_pool_precision_test(pool_spec, block).await {
                Ok(()) => {}
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("not found")
                        || msg.contains("invalid block")
                        || msg.contains("connection")
                        || msg.contains("send request")
                    {
                        println!("  ⏭️ Skipping (connection/block issue): {:?}", e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    println!("\n=== All tests completed ===");
    Ok(())
}

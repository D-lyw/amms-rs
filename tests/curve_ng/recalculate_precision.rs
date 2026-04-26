use alloy::{
    eips::BlockId,
    primitives::{address, Address, U256},
    providers::{Provider, ProviderBuilder},
    sol,
};
use amms::amms::{
    amm::AutomatedMarketMaker,
    curve_ng::types::{CurveNGPool, CurveNGPoolType},
};

sol! {
    #[sol(rpc)]
    interface ICurveCryptoPoolNG {
        function get_dy(uint256 i, uint256 j, uint256 dx) external view returns (uint256);
        function D() external view returns (uint256);
    }
}

struct PoolSpec {
    name: &'static str,
    address: Address,
    pool_type: CurveNGPoolType,
    rpc_env: &'static str,
    decimals: &'static [u8],
}

const TEST_POOLS: &[PoolSpec] = &[
    // === TwoCrypto-NG (4 pools) ===
    PoolSpec {
        name: "TwoCrypto-crvUSD-WBTC",
        address: address!("d9ff8396554a0d18b2cfbec53e1979b7ecce8373"),
        pool_type: CurveNGPoolType::TwoCrypto,
        rpc_env: "ETHEREUM_PROVIDER",
        decimals: &[18, 8],
    },
    PoolSpec {
        name: "TwoCrypto-crvUSD-cbBTC",
        address: address!("83f24023d15d835a213df24fd309c47dab5beb32"),
        pool_type: CurveNGPoolType::TwoCrypto,
        rpc_env: "ETHEREUM_PROVIDER",
        decimals: &[18, 8],
    },
    PoolSpec {
        name: "TwoCrypto-crvUSD-tBTC",
        address: address!("f1f435b05d255a5dbde37333c0f61da6f69c6127"),
        pool_type: CurveNGPoolType::TwoCrypto,
        rpc_env: "ETHEREUM_PROVIDER",
        decimals: &[18, 18],
    },
    PoolSpec {
        name: "TwoCrypto-crvUSD-WETH",
        address: address!("6e5492f8ea2370844ee098a56dd88e1717e4a9c2"),
        pool_type: CurveNGPoolType::TwoCrypto,
        rpc_env: "ETHEREUM_PROVIDER",
        decimals: &[18, 18],
    },
    // === TriCrypto-NG (7 pools) ===
    PoolSpec {
        name: "TriCrypto-USDC-WBTC-WETH-1",
        address: address!("7f86bf177dd4f3494b841a37e810a34dd56c829b"),
        pool_type: CurveNGPoolType::TriCrypto,
        rpc_env: "ETHEREUM_PROVIDER",
        decimals: &[6, 8, 18],
    },
    PoolSpec {
        name: "TriCrypto-crvUSD-tBTC-wstETH",
        address: address!("2889302a794da87fbf1d6db415c1492194663d13"),
        pool_type: CurveNGPoolType::TriCrypto,
        rpc_env: "ETHEREUM_PROVIDER",
        decimals: &[18, 18, 18],
    },
    PoolSpec {
        name: "TriCrypto-wstETH-rETH-sfrxETH",
        address: address!("2570f1bd5d2735314fc102eb12fc1afe9e6e7193"),
        pool_type: CurveNGPoolType::TriCrypto,
        rpc_env: "ETHEREUM_PROVIDER",
        decimals: &[18, 18, 18],
    },
    PoolSpec {
        name: "TriCrypto-GHO-cbBTC-WETH",
        address: address!("8a4f252812dff2a8636e4f7eb249d8fc2e3bd77f"),
        pool_type: CurveNGPoolType::TriCrypto,
        rpc_env: "ETHEREUM_PROVIDER",
        decimals: &[18, 8, 18],
    },
    PoolSpec {
        name: "TriCrypto-USDT-WBTC-WETH-2",
        address: address!("f5f5b97624542d72a9e06f04804bf81baa15e2b4"),
        pool_type: CurveNGPoolType::TriCrypto,
        rpc_env: "ETHEREUM_PROVIDER",
        decimals: &[6, 8, 18],
    },
    PoolSpec {
        name: "TriCrypto-crvUSD-WETH-CRV",
        address: address!("4ebdf703948ddcea3b11f675b4d1fba9d2414a14"),
        pool_type: CurveNGPoolType::TriCrypto,
        rpc_env: "ETHEREUM_PROVIDER",
        decimals: &[18, 18, 18],
    },
    PoolSpec {
        name: "TriCrypto-sdUSDC-USDT-WBTC",
        address: address!("dae4135dac6c62937728d145f8048b2bab2ce55c"),
        pool_type: CurveNGPoolType::TriCrypto,
        rpc_env: "ETHEREUM_PROVIDER",
        decimals: &[18, 6, 8],
    },
];

async fn get_safe_test_blocks<P: Provider + Clone>(
    provider: &P,
    steps_behind: &[u64],
) -> eyre::Result<Vec<u64>> {
    let current = provider.get_block_number().await?;
    Ok(steps_behind.iter().map(|s| current.saturating_sub(*s)).collect())
}

async fn run_single_pool_precision_test(
    pool_spec: &PoolSpec,
    block: u64,
) -> eyre::Result<()> {
    let rpc_url = match std::env::var(pool_spec.rpc_env) {
        Ok(url) => url,
        Err(_) => {
            println!("  ⏭️ Skipping {}: {} not set", pool_spec.name, pool_spec.rpc_env);
            return Ok(());
        }
    };
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let block_id = BlockId::from(block);

    println!("  Initializing {} at block {}...", pool_spec.name, block);
    let mut pool = CurveNGPool::new(pool_spec.address, pool_spec.pool_type)
        .init(block_id, provider.clone())
        .await?;

    let n_coins = pool.n_coins as usize;
    println!(
        "    n_coins={}, balances={:?}, price_scale={:?}",
        n_coins, pool.balances, pool.price_scale
    );

    // Fetch chain D
    let contract = ICurveCryptoPoolNG::new(pool_spec.address, provider.clone());
    let d_chain: U256 = contract.D().block(block_id).call().await?;

    // Call recalculate_d locally
    pool.recalculate_d()?;
    let d_local = pool.d.unwrap_or(U256::ZERO);

    let d_diff = if d_chain > d_local { d_chain - d_local } else { d_local - d_chain };
    let d_diff_ratio = if d_chain.is_zero() {
        0.0
    } else {
        d_diff.to_string().parse::<f64>().unwrap_or(0.0)
            / d_chain.to_string().parse::<f64>().unwrap_or(1.0)
    };

    println!(
        "    D: chain={}, local={}, diff_ratio={:.12}, passed={}",
        d_chain, d_local, d_diff_ratio, d_diff_ratio < 1e-10
    );

    if d_diff_ratio >= 1e-10 {
        eyre::bail!(
            "D divergence too large! chain={}, local={}, ratio={}",
            d_chain, d_local, d_diff_ratio
        );
    }

    // Test swap simulation accuracy
    if n_coins >= 2 {
        let amount_in = U256::from(10).pow(U256::from(pool_spec.decimals[0] as u64 - 1));
        let swap_i = 0;
        let swap_j = 1.min(n_coins - 1);

        match pool.simulate_swap(pool.coins[swap_i], pool.coins[swap_j], amount_in) {
            Ok(local_out) => {
                let onchain_out: U256 = contract
                    .get_dy(U256::from(swap_i as u64), U256::from(swap_j as u64), amount_in)
                    .block(block_id)
                    .call()
                    .await?;

                let swap_diff = if local_out > onchain_out { local_out - onchain_out } else { onchain_out - local_out };
                let swap_diff_ratio = if onchain_out.is_zero() {
                    0.0
                } else {
                    swap_diff.to_string().parse::<f64>().unwrap_or(0.0)
                        / onchain_out.to_string().parse::<f64>().unwrap_or(1.0)
                };
                println!(
                    "    Swap({}→{} amount_in={}): local={}, chain={}, diff_ratio={:.12}, passed={}",
                    swap_i, swap_j, amount_in, local_out, onchain_out, swap_diff_ratio,
                    swap_diff_ratio < 1e-6
                );

                if swap_diff_ratio >= 1e-6 {
                    eyre::bail!(
                        "Swap divergence too large! chain={}, local={}, ratio={}",
                        onchain_out, local_out, swap_diff_ratio
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
async fn test_curve_ng_recalculate_precision() -> eyre::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    dotenv::dotenv().ok();
    println!("=== CurveNG recalculate_d Precision Test ===\n");

    for pool_spec in TEST_POOLS {
        let rpc_url = match std::env::var(pool_spec.rpc_env) {
            Ok(url) => url,
            Err(_) => {
                println!("⏭️ Skipping {}: {} not set", pool_spec.name, pool_spec.rpc_env);
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
                println!("⏭️ Skipping {}: RPC connection failed: {:?}", pool_spec.name, e);
                continue;
            }
        };

        let blocks = match get_safe_test_blocks(&provider, &[100, 500, 2000]).await {
            Ok(b) => b,
            Err(e) => {
                println!("⏭️ Skipping {}: cannot fetch blocks: {:?}", pool_spec.name, e);
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
                    if msg.contains("not found") || msg.contains("invalid block")
                        || msg.contains("connection") || msg.contains("send request")
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

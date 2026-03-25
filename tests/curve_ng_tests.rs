use alloy::{
    primitives::{Address, U256},
    providers::{Provider, ProviderBuilder},
    sol,
};
use amms::amms::{
    amm::AutomatedMarketMaker,
    curve_ng::{CurveNGPool, CurveNGPoolType},
};
use eyre::Result;
use std::{
    env,
    fs::File,
    future::Future,
    io::{BufRead, BufReader},
    str::FromStr,
};

fn get_provider_url() -> Option<String> {
    dotenv::dotenv().ok();
    env::var("ETHEREUM_PROVIDER")
        .or_else(|_| env::var("ETHEREUM_RPC_URL"))
        .ok()
}

sol! {
    #[sol(rpc)]
    interface ICurveStablePoolNG {
        function get_dy(int128 i, int128 j, uint256 dx) external view returns (uint256);
    }

    #[sol(rpc)]
    interface ICurveCryptoPoolNG {
        function get_dy(uint256 i, uint256 j, uint256 dx) external view returns (uint256);
    }

    #[sol(rpc)]
    interface ICurveCryptoPoolMeta {
        function future_A_gamma_time() external view returns (uint256);
    }
}

#[derive(serde::Deserialize)]
struct PoolIndexEntry {
    address: String,
    pool_type: Option<String>,
    curve_pool_type: Option<String>,
}

fn load_curve_ng_pools(
    limit_stable: usize,
    limit_two: usize,
    limit_tri: usize,
) -> Result<(Vec<Address>, Vec<Address>, Vec<Address>)> {
    // Pool index is JSONL; we select CurveNG pools here to expand test coverage.
    let path = "/Users/d-lyw/D-lyw/aave-liquidation/config/pool_index_1.json";
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    let mut stable = Vec::new();
    let mut two = Vec::new();
    let mut tri = Vec::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let entry: PoolIndexEntry = serde_json::from_str(&line)?;
        if entry.pool_type.as_deref() != Some("curve") {
            continue;
        }
        match entry.curve_pool_type.as_deref() {
            Some("StableSwapNG") if stable.len() < limit_stable => {
                stable.push(Address::from_str(&entry.address)?);
            }
            Some("TwoCryptoNG") if two.len() < limit_two => {
                two.push(Address::from_str(&entry.address)?);
            }
            Some("TriCryptoNG") if tri.len() < limit_tri => {
                tri.push(Address::from_str(&entry.address)?);
            }
            _ => {}
        }
        if stable.len() >= limit_stable && two.len() >= limit_two && tri.len() >= limit_tri {
            break;
        }
    }

    Ok((stable, two, tri))
}

fn sample_amounts(balance: U256, decimals: u8) -> Vec<U256> {
    let base = U256::from(10).pow(U256::from(decimals));
    let a1 = std::cmp::min(balance / U256::from(1000u64), base * U256::from(10u64));
    let a2 = std::cmp::min(balance / U256::from(100u64), base * U256::from(100u64));
    let mut out = Vec::new();
    out.push(if a1.is_zero() { U256::from(1u64) } else { a1 });
    let a2v = if a2.is_zero() { U256::from(1u64) } else { a2 };
    if a2v != out[0] {
        out.push(a2v);
    }
    out
}

async fn find_min_dx_onchain<F, Fut>(mut get_dy: F, target_out: U256, hi: U256) -> Result<U256>
where
    F: FnMut(U256) -> Fut,
    Fut: Future<Output = Result<U256>>,
{
    if hi.is_zero() {
        return Ok(U256::ZERO);
    }
    let mut lo = U256::ZERO;
    let mut hi = hi;
    while lo < hi {
        let mid = (lo + hi) >> 1;
        let out = get_dy(mid).await?;
        if out >= target_out {
            hi = mid;
        } else {
            lo = mid + U256::from(1u8);
        }
    }
    Ok(lo)
}

#[tokio::test]
async fn test_ng_stableswap_simulation() -> Result<()> {
    let rpc_url = match get_provider_url() {
        Some(url) => url,
        None => {
            println!("Skipping test: ETHEREUM_PROVIDER/ETHEREUM_RPC_URL not set");
            return Ok(());
        }
    };
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let block = provider.get_block_number().await?;
    let block_id = alloy::eips::BlockId::number(block);

    let (stable_pools, _, _) = load_curve_ng_pools(12, 0, 0)?;
    assert!(!stable_pools.is_empty(), "No StableSwapNG pools found");

    for pool_addr in stable_pools {
        println!("Testing StableSwap NG Pool: {:?}", pool_addr);

        let mut pool = CurveNGPool::new(pool_addr, CurveNGPoolType::StableSwap);
        pool = pool.init(block_id, provider.clone()).await?;

        println!("Pool Initialized");
        println!("Coins: {:?}", pool.coins);

        let contract = ICurveStablePoolNG::new(pool_addr, provider.clone());
        if pool.coins.len() < 2 {
            continue;
        }

        let pairs = vec![(0usize, 1usize), (1usize, 0usize)];
        for (i, j) in pairs {
            let amounts = sample_amounts(pool.balances[i], pool.decimals[i]);
            for amount_in in amounts {
                let amount_out_sim =
                    pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;

                let amount_out_chain = contract
                    .get_dy(i as i128, j as i128, amount_in)
                    .block(block_id)
                    .call()
                    .await?;

                println!(
                    "{}->{} In: {}, Sim Out: {}, Chain Out: {}",
                    i, j, amount_in, amount_out_sim, amount_out_chain
                );

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
async fn test_ng_stableswap_exact_out_simulation() -> Result<()> {
    let rpc_url = match get_provider_url() {
        Some(url) => url,
        None => {
            println!("Skipping test: ETHEREUM_PROVIDER/ETHEREUM_RPC_URL not set");
            return Ok(());
        }
    };
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let block = provider.get_block_number().await?;
    let block_id = alloy::eips::BlockId::number(block);

    let (stable_pools, _, _) = load_curve_ng_pools(8, 0, 0)?;
    assert!(!stable_pools.is_empty(), "No StableSwapNG pools found");

    for pool_addr in stable_pools {
        println!("Testing StableSwap NG Exact-Out Pool: {:?}", pool_addr);

        let mut pool = CurveNGPool::new(pool_addr, CurveNGPoolType::StableSwap);
        pool = pool.init(block_id, provider.clone()).await?;

        let contract = ICurveStablePoolNG::new(pool_addr, provider.clone());
        if pool.coins.len() < 2 {
            continue;
        }

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
async fn test_ng_twocrypto_simulation() -> Result<()> {
    // NOTE: TwoCryptoNG bit-exactness is still being aligned with official Vyper math.
    // We keep strict equality to surface any remaining rounding drift.
    let rpc_url = match get_provider_url() {
        Some(url) => url,
        None => {
            println!("Skipping test: ETHEREUM_PROVIDER/ETHEREUM_RPC_URL not set");
            return Ok(());
        }
    };
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let block = provider.get_block_number().await?;
    let block_id = alloy::eips::BlockId::number(block);
    let block_ts = provider
        .get_block(block_id)
        .await?
        .map(|b| b.header.timestamp)
        .unwrap_or_default();

    let (_, two_pools, _) = load_curve_ng_pools(0, 3, 0)?;
    assert!(!two_pools.is_empty(), "No TwoCryptoNG pools found");

    for pool_address in two_pools {
        println!("Testing TwoCrypto NG Pool: {:?}", pool_address);

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
        println!("Pool Initialized");
        println!("Coins: {:?}", pool.coins);

        let contract = ICurveCryptoPoolNG::new(pool_address, provider.clone());
        if pool.coins.len() < 2 {
            continue;
        }

        let pairs = vec![(0usize, 1usize), (1usize, 0usize)];
        for (i, j) in pairs {
            let amounts = sample_amounts(pool.balances[i], pool.decimals[i]);
            for amount_in in amounts {
                let amount_out_sim =
                    pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;

                let amount_out_chain = contract
                    .get_dy(U256::from(i), U256::from(j), amount_in)
                    .block(block_id)
                    .call()
                    .await?;

                println!(
                    "{}->{} In: {}, Sim Out: {}, Chain Out: {}",
                    i, j, amount_in, amount_out_sim, amount_out_chain
                );

                assert_eq!(
                    amount_out_sim, amount_out_chain,
                    "TwoCrypto exact-in mismatch pool {:?} {}->{} sim={} chain={}",
                    pool_address, i, j, amount_out_sim, amount_out_chain
                );
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn test_ng_tricrypto_simulation() -> Result<()> {
    // NOTE: TriCryptoNG bit-exactness is still being aligned with official Vyper math.
    // We keep strict equality to surface any remaining rounding drift.
    let rpc_url = match get_provider_url() {
        Some(url) => url,
        None => {
            println!("Skipping test: ETHEREUM_PROVIDER/ETHEREUM_RPC_URL not set");
            return Ok(());
        }
    };
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let block = provider.get_block_number().await?;
    let block_id = alloy::eips::BlockId::number(block);
    let block_ts = provider
        .get_block(block_id)
        .await?
        .map(|b| b.header.timestamp)
        .unwrap_or_default();

    let (_, _, tri_pools) = load_curve_ng_pools(0, 0, 10)?;
    assert!(!tri_pools.is_empty(), "No TriCryptoNG pools found");

    for pool_address in tri_pools {
        println!("Testing Tricrypto NG Pool: {:?}", pool_address);

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
        println!("Pool Initialized");
        println!("Coins: {:?}", pool.coins);

        let contract = ICurveCryptoPoolNG::new(pool_address, provider.clone());
        if pool.coins.len() < 3 {
            continue;
        }

        let pairs = vec![(0usize, 1usize), (1usize, 0usize), (2usize, 0usize)];
        for (i, j) in pairs {
            let amounts = sample_amounts(pool.balances[i], pool.decimals[i]);
            for amount_in in amounts {
                let amount_out_sim =
                    pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;

                let amount_out_chain = contract
                    .get_dy(U256::from(i), U256::from(j), amount_in)
                    .block(block_id)
                    .call()
                    .await?;

                println!(
                    "{}->{} In: {}, Sim Out: {}, Chain Out: {}",
                    i, j, amount_in, amount_out_sim, amount_out_chain
                );

                assert_eq!(
                    amount_out_sim, amount_out_chain,
                    "TriCrypto exact-in mismatch pool {:?} {}->{} sim={} chain={}",
                    pool_address, i, j, amount_out_sim, amount_out_chain
                );
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn test_ng_twocrypto_exact_out_simulation() -> Result<()> {
    let rpc_url = match get_provider_url() {
        Some(url) => url,
        None => {
            println!("Skipping test: ETHEREUM_PROVIDER/ETHEREUM_RPC_URL not set");
            return Ok(());
        }
    };
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let block = provider.get_block_number().await?;
    let block_id = alloy::eips::BlockId::number(block);
    let block_ts = provider
        .get_block(block_id)
        .await?
        .map(|b| b.header.timestamp)
        .unwrap_or_default();

    let (_, two_pools, _) = load_curve_ng_pools(0, 3, 0)?;
    assert!(!two_pools.is_empty(), "No TwoCryptoNG pools found");

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

        let contract = ICurveCryptoPoolNG::new(pool_address, provider.clone());
        if pool.coins.len() < 2 {
            continue;
        }

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
        let hi = if local_dx > amount_in { local_dx } else { amount_in };
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
        assert_eq!(
            local_dx, onchain_dx,
            "TwoCrypto exact-out mismatch pool {:?} local_dx={} onchain_dx={} target_out={}",
            pool_address, local_dx, onchain_dx, target_out
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
    let rpc_url = match get_provider_url() {
        Some(url) => url,
        None => {
            println!("Skipping test: ETHEREUM_PROVIDER/ETHEREUM_RPC_URL not set");
            return Ok(());
        }
    };
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let block = provider.get_block_number().await?;
    let block_id = alloy::eips::BlockId::number(block);
    let block_ts = provider
        .get_block(block_id)
        .await?
        .map(|b| b.header.timestamp)
        .unwrap_or_default();

    let (_, _, tri_pools) = load_curve_ng_pools(0, 0, 10)?;
    assert!(!tri_pools.is_empty(), "No TriCryptoNG pools found");

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

        let contract = ICurveCryptoPoolNG::new(pool_address, provider.clone());
        if pool.coins.len() < 3 {
            continue;
        }

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
        let hi = if local_dx > amount_in { local_dx } else { amount_in };
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
        assert_eq!(
            local_dx, onchain_dx,
            "TriCrypto exact-out mismatch pool {:?} local_dx={} onchain_dx={} target_out={}",
            pool_address, local_dx, onchain_dx, target_out
        );

        let out_at_local = contract
            .get_dy(U256::from(2), U256::from(0), local_dx)
            .block(block_id)
            .call()
            .await?;
        assert!(
            out_at_local >= target_out,
            "Exact-out failed to reach target: target={target_out} out_at_local={out_at_local}"
        );

        if local_dx > U256::ZERO {
            let out_prev = contract
                .get_dy(U256::from(2), U256::from(0), local_dx - U256::from(1u8))
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

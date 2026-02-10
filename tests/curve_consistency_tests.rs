use alloy::{
    network::Ethereum,
    primitives::{address, U256},
    providers::ProviderBuilder,
    sol,
};
use amms::amms::{
    amm::AutomatedMarketMaker,
    curve_legacy::types::{CurveLegacyPool, CurveLegacyPoolType},
    curve_ng::{types::CurveNGPoolType, CurveNGPool},
};
use eyre::Result;
use std::env;

sol! {
    #[sol(rpc)]
    interface ICurveStableV1 {
        function get_dy(int128 i, int128 j, uint256 dx) external view returns (uint256);
    }

    #[sol(rpc)]
    interface ICurveCryptoV2 {
        function get_dy(uint256 i, uint256 j, uint256 dx) external view returns (uint256);
    }

    #[sol(rpc)]
    interface ICurveNG {
        function get_dy(uint256 i, uint256 j, uint256 dx) external view returns (uint256);
    }
}

async fn setup_provider() -> Result<impl alloy::providers::Provider<Ethereum> + Clone> {
    let rpc_url = env::var("ETHEREUM_RPC_URL").unwrap_or("https://eth.merkle.io".to_string());
    Ok(ProviderBuilder::new().connect_http(rpc_url.parse()?))
}

#[tokio::test]
async fn test_consistency_3pool_legacy_stable() -> Result<()> {
    let provider = setup_provider().await?;
    let pool_addr = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7"); // 3pool

    // 1. Init Local
    let mut pool = CurveLegacyPool::new(pool_addr, CurveLegacyPoolType::StableSwap);
    pool = pool
        .init(alloy::eips::BlockId::latest(), provider.clone())
        .await?;

    // 2. Setup Params
    // DAI (0) -> USDC (1)
    let i = 0;
    let j = 1;
    let amount_in = U256::from(10000) * U256::from(10).pow(U256::from(18)); // 10,000 DAI

    // 3. Local Sim
    let local_out = pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;

    // 4. Chain Sim
    // 3pool is old StableSwap, uses int128
    let contract = ICurveStableV1::new(pool_addr, provider.clone());
    // Use simple cast to i128
    let chain_out = contract
        .get_dy(i as i128, j as i128, amount_in)
        .call()
        .await?;

    println!(
        "3pool (DAI->USDC): Local={}, Chain={}",
        local_out, chain_out
    );

    verify_diff(local_out, chain_out, 1000); // 1000 ppm
    Ok(())
}

#[tokio::test]
async fn test_consistency_steth_legacy_stable() -> Result<()> {
    let provider = setup_provider().await?;
    let pool_addr = address!("DC24316b9AE028F1497c275EB9192a3Ea0f67022"); // stETH

    let mut pool = CurveLegacyPool::new(pool_addr, CurveLegacyPoolType::StableSwap);
    pool = pool
        .init(alloy::eips::BlockId::latest(), provider.clone())
        .await?;

    println!("stETH Coins: {:?}", pool.coins);

    let i = 1;
    let j = 0;
    let amount_in = U256::from(10) * U256::from(10).pow(U256::from(18)); // 10 stETH (amount_in)

    // Check if coins are valid (might be ETH wrapper issue)
    // stETH pool coins: [ETH, stETH] usually.
    // If Simulate Swap uses coins[i], ensure coins[i] is valid.

    let local_out = pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;

    let contract = ICurveStableV1::new(pool_addr, provider.clone());
    let chain_out = contract
        .get_dy(i as i128, j as i128, amount_in)
        .call()
        .await?;

    println!(
        "stETH (stETH->ETH): Local={}, Chain={}",
        local_out, chain_out
    );

    verify_diff(local_out, chain_out, 100);
    Ok(())
}

#[tokio::test]
async fn test_consistency_tricrypto2_legacy_crypto() -> Result<()> {
    let provider = setup_provider().await?;
    let pool_addr = address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46"); // Tricrypto2

    let mut pool = CurveLegacyPool::new(pool_addr, CurveLegacyPoolType::CryptoSwap);
    pool = pool
        .init(alloy::eips::BlockId::latest(), provider.clone())
        .await?;

    // 验证必要参数是否已获取
    if pool.out_fee.is_none() || pool.mid_fee.is_none() || pool.fee_gamma.is_none() {
        println!(
            "Skipping Tricrypto2 test: missing fee params (out_fee={:?}, mid_fee={:?}, fee_gamma={:?})",
            pool.out_fee, pool.mid_fee, pool.fee_gamma
        );
        return Ok(());
    }

    // WBTC (1) -> USDT (0)
    let i = 1;
    let j = 0;
    let amount_in = U256::from(1) * U256::from(10).pow(U256::from(8)); // 1 WBTC

    let local_out = pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;

    let contract = ICurveCryptoV2::new(pool_addr, provider.clone());
    let chain_out = contract
        .get_dy(U256::from(i), U256::from(j), amount_in)
        .call()
        .await?;

    println!(
        "Tricrypto2 Params: A={:?}, Gamma={:?}, D={:?}, Fee={:?}",
        pool.amp, pool.gamma, pool.d, pool.fee
    );
    println!("Tricrypto2 Balances: {:?}", pool.balances);

    println!(
        "Tricrypto2 (WBTC->USDT): Local={}, Chain={}",
        local_out, chain_out
    );

    verify_diff(local_out, chain_out, 2000); // 2000 ppm (0.2%) threshold
    Ok(())
}

#[tokio::test]
async fn test_consistency_stableswap_ng() -> Result<()> {
    let provider = setup_provider().await?;
    // stETH-ng-f (stETH/ETH) StableSwap-NG 池 (2-coin plain pool)
    let pool_addr = address!("21e27a5e5513D6e65C4f830167390997aA84843a");

    let mut pool = CurveNGPool::new(pool_addr, CurveNGPoolType::StableSwap);
    pool = pool
        .init(alloy::eips::BlockId::latest(), provider.clone())
        .await?;

    println!("StableSwap-NG Pool initialized:");
    println!("  n_coins: {}", pool.n_coins);
    println!("  coins: {:?}", pool.coins);
    println!("  amp: {:?}", pool.amp);

    // 验证初始化成功
    assert!(
        pool.n_coins >= 2,
        "StableSwap-NG pool should have at least 2 coins"
    );
    assert!(pool.amp.is_some(), "StableSwap-NG pool should have amp");

    // 交换第 0 -> 1 个代币
    let i = 0;
    let j = 1;
    // 使用对应精度的金额
    let decimals_i = pool.decimals[i] as u32;
    let amount_in = U256::from(100) * U256::from(10).pow(U256::from(decimals_i)); // 100 tokens

    let local_out = pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;

    // StableSwap-NG 使用 int128 接口
    let contract = ICurveStableV1::new(pool_addr, provider.clone());
    let chain_out = contract
        .get_dy(i as i128, j as i128, amount_in)
        .call()
        .await?;

    println!(
        "StableSwap-NG (coin0->coin1): Local={}, Chain={}",
        local_out, chain_out
    );

    verify_diff(local_out, chain_out, 100); // 100 ppm = 0.01%
    Ok(())
}

#[tokio::test]
async fn test_consistency_tricrypto_ng() -> Result<()> {
    let provider = setup_provider().await?;
    // TricryptoUSDC (factory-tricrypto-2): USDC/WBTC/WETH
    let pool_addr = address!("7F86Bf177Dd4F3494b841a37e810A34dD56c829B");

    let mut pool = CurveNGPool::new(pool_addr, CurveNGPoolType::TriCrypto);
    pool = pool
        .init(alloy::eips::BlockId::latest(), provider.clone())
        .await?;

    println!("TriCrypto-NG Pool initialized:");
    println!("  n_coins: {}", pool.n_coins);
    println!("  coins: {:?}", pool.coins);
    println!("  gamma: {:?}", pool.gamma);
    println!("  price_scale: {:?}", pool.price_scale);

    // 验证初始化成功
    assert!(
        pool.n_coins >= 3,
        "TriCrypto-NG pool should have at least 3 coins, got {}",
        pool.n_coins
    );
    assert!(
        pool.gamma.is_some(),
        "TriCrypto-NG pool should have gamma parameter"
    );

    // 交换 coin0 -> coin1 (使用池中实际代币)
    let i = 0;
    let j = 1;
    let decimals_i = pool.decimals[i] as u32;
    let amount_in = U256::from(100) * U256::from(10).pow(U256::from(decimals_i)); // 100 tokens

    let local_out = pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;

    let contract = ICurveNG::new(pool_addr, provider.clone());
    let chain_out = contract
        .get_dy(U256::from(i), U256::from(j), amount_in)
        .call()
        .await?;

    println!(
        "TriCrypto-NG (coin0->coin1): Local={}, Chain={}",
        local_out, chain_out
    );

    verify_diff(local_out, chain_out, 500); // 500 ppm = 0.05%
    Ok(())
}

#[tokio::test]
async fn test_consistency_twocrypto_ng() -> Result<()> {
    let provider = setup_provider().await?;
    // TwoCrypto-NG 池: UwU/WETH (factory-twocrypto-19, 已在内置测试中验证)
    let pool_addr = address!("77146B0a1d08B6844376dF6d9da99bA7F1b19e71");

    let mut pool = CurveNGPool::new(pool_addr, CurveNGPoolType::TwoCrypto);
    pool = pool
        .init(alloy::eips::BlockId::latest(), provider.clone())
        .await?;

    println!("TwoCrypto-NG Pool initialized:");
    println!("  n_coins: {}", pool.n_coins);
    println!("  coins: {:?}", pool.coins);
    println!("  gamma: {:?}", pool.gamma);
    println!("  price_scale: {:?}", pool.price_scale);

    // 验证初始化成功
    assert!(
        pool.n_coins >= 2,
        "TwoCrypto-NG pool should have at least 2 coins, got {}",
        pool.n_coins
    );
    assert!(
        pool.gamma.is_some(),
        "TwoCrypto-NG pool should have gamma parameter"
    );

    // 交换 coin0 -> coin1
    let i = 0;
    let j = 1;
    let decimals_i = pool.decimals[i] as u32;
    let amount_in = U256::from(100) * U256::from(10).pow(U256::from(decimals_i)); // 100 tokens

    let local_out = pool.simulate_swap(pool.coins[i], pool.coins[j], amount_in)?;

    let contract = ICurveNG::new(pool_addr, provider.clone());
    let chain_out = contract
        .get_dy(U256::from(i), U256::from(j), amount_in)
        .call()
        .await?;

    println!(
        "TwoCrypto-NG (coin0->coin1): Local={}, Chain={}",
        local_out, chain_out
    );

    verify_diff(local_out, chain_out, 500); // 500 ppm = 0.05%
    Ok(())
}

fn verify_diff(local: U256, chain: U256, threshold_ppm: u64) {
    if local == chain {
        return;
    }

    let diff = if local > chain {
        local - chain
    } else {
        chain - local
    };
    // diff / chain * 1e6 <= threshold

    let ratio = diff * U256::from(1_000_000) / chain;
    println!(
        "Diff: {}, Ratio: {} ppm (Threshold: {})",
        diff, ratio, threshold_ppm
    );

    assert!(ratio <= U256::from(threshold_ppm), "Deviation too high!");
}

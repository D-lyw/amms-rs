use std::env;

use alloy::{
    eips::BlockId,
    primitives::{address, Address, U256},
    providers::{Provider, ProviderBuilder},
    sol,
};
use amms::amms::{
    amm::{AutomatedMarketMaker, AMM},
    curve_legacy::{CurveLegacyPool, CurveLegacyPoolType},
};
use amms::state_space::StateSpaceBuilder;
use eyre::{eyre, Result};

use crate::common::quotes::assert_diff_within_ppm;

const SUSDE_META_POOL: Address = address!("5a6a4d54456819380173272a5e8e9b9904bdf41b");
const GUSD_META_POOL: Address = address!("4f062658EaAF2C1ccf8C8e36D6824CDf41167956");
const SUSDE: Address = address!("99d8a9c45b2eca8864373a26d1459e3dff1e17f3");
const GUSD: Address = address!("056fd409e1d7a124bd7017459dfea2f387b6d5cd");
const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
const THREE_POOL: Address = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");
const THREE_CRV: Address = address!("6c3F90f043a72FA612cbac8115EE7e52BDe6E490");

const CVX_META_POOL: Address = address!("bec570d92afb7ffc553bdd9d4b4638121000b10d");
const CVX_META_ZAP: Address = address!("5de4ef4879f4fe3bbadf2227d2ac5d0e2d76c895");
const CVX: Address = address!("4e3FBD56CD56c3e72C1403e103b45Db9da5B9D2B");
const FRAX: Address = address!("853d955aCEf822Db058eb8505911ED77F175b99e");
const BASE_FRAX_USDC: Address = address!("DcEF968d416a41Cdac0ED8702fAC8128A64241A2");

sol! {
    #[sol(rpc)]
    interface ICurveStableMetaGetDyUnderlying {
        function get_dy_underlying(int128 i, int128 j, uint256 dx) external view returns (uint256);
    }

    #[sol(rpc)]
    interface ICurveCryptoGetDy {
        function get_dy(uint256 i, uint256 j, uint256 dx) external view returns (uint256);
    }
}

fn ethereum_provider_url() -> Option<String> {
    dotenv::dotenv().ok();
    env::var("ETHEREUM_PROVIDER")
        .or_else(|_| env::var("ETH_RPC_URL"))
        .or_else(|_| env::var("MAINNET_RPC_URL"))
        .ok()
        .or_else(|| Some("https://ethereum.publicnode.com".to_string()))
}

async fn resolve_fork_block<P: Provider>(provider: &P) -> Result<u64> {
    if let Ok(raw) = env::var("CURVE_LEGACY_ETH_META_FORK_BLOCK") {
        return Ok(raw.parse::<u64>()?);
    }
    Ok(provider.get_block_number().await?)
}

#[tokio::test]
async fn test_legacy_stable_meta_detection_via_registry_fallback_on_ethereum_fork() -> Result<()> {
    let Some(rpc_url) = ethereum_provider_url() else {
        println!("Skipping: ETHEREUM_PROVIDER/ETH_RPC_URL not set");
        return Ok(());
    };

    let upstream = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let fork_block = resolve_fork_block(&upstream).await?;
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    let susde_pool = CurveLegacyPool::new(SUSDE_META_POOL, CurveLegacyPoolType::StableSwap)
        .init(BlockId::number(fork_block), provider.clone())
        .await?;
    let gusd_pool = CurveLegacyPool::new(GUSD_META_POOL, CurveLegacyPoolType::StableSwap)
        .init(BlockId::number(fork_block), provider)
        .await?;

    for (label, pool, expected_meta_coin) in
        [("sUSDe", susde_pool, SUSDE), ("GUSD", gusd_pool, GUSD)]
    {
        assert!(pool.is_meta_pool(), "{} should be detected as meta", label);
        assert_eq!(pool.base_pool_address, Some(THREE_POOL));
        assert_eq!(pool.base_lp_token, Some(THREE_CRV));
        assert_eq!(pool.base_token_index, Some(1));
        assert_eq!(pool.coins.len(), 2);
        assert_eq!(
            pool.underlying_coins,
            vec![expected_meta_coin, DAI, USDC, USDT]
        );
        assert_eq!(pool.tokens(), vec![expected_meta_coin, DAI, USDC, USDT]);
        assert!(
            pool.base_pool_view.is_some(),
            "{} should materialize base_pool_view",
            label
        );
    }

    Ok(())
}

#[tokio::test]
async fn test_legacy_stable_meta_underlying_quote_parity_on_ethereum_fork() -> Result<()> {
    let Some(rpc_url) = ethereum_provider_url() else {
        println!("Skipping: ETHEREUM_PROVIDER/ETH_RPC_URL not set");
        return Ok(());
    };

    let upstream = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let fork_block = resolve_fork_block(&upstream).await?;
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let block_id = BlockId::number(fork_block);

    let pool = CurveLegacyPool::new(SUSDE_META_POOL, CurveLegacyPoolType::StableSwap)
        .init(block_id, provider.clone())
        .await?;
    let chain = ICurveStableMetaGetDyUnderlying::new(SUSDE_META_POOL, provider.clone());

    assert!(pool.is_meta_pool());
    assert_eq!(pool.base_pool_address, Some(THREE_POOL));
    assert_eq!(pool.underlying_coins, vec![SUSDE, DAI, USDC, USDT]);

    let cases = [
        (
            "sUSDe -> DAI",
            0i128,
            1i128,
            U256::from(10_000000000000000000u128),
        ),
        (
            "sUSDe -> USDC",
            0i128,
            2i128,
            U256::from(10_000000000000000000u128),
        ),
        (
            "DAI -> sUSDe",
            1i128,
            0i128,
            U256::from(10_000000000000000000u128),
        ),
        ("USDC -> sUSDe", 2i128, 0i128, U256::from(10_000000u64)),
        ("USDC -> USDT", 2i128, 3i128, U256::from(10_000000u64)),
    ];

    for (label, i, j, amount_in) in cases {
        let token_in = pool.underlying_coins[i as usize];
        let token_out = pool.underlying_coins[j as usize];
        let local_out = pool.simulate_swap(token_in, token_out, amount_in)?;
        let chain_out = chain
            .get_dy_underlying(i, j, amount_in)
            .block(block_id)
            .call()
            .await?;

        println!(
            "quote parity [stableMeta]: {} amount_in={} local={} chain={}",
            label, amount_in, local_out, chain_out
        );
        assert_diff_within_ppm(local_out, chain_out, 20);
    }

    Ok(())
}

#[tokio::test]
async fn test_legacy_stable_meta_state_space_adds_top_level_base_pool_on_ethereum_fork(
) -> Result<()> {
    let Some(rpc_url) = ethereum_provider_url() else {
        println!("Skipping: ETHEREUM_PROVIDER/ETH_RPC_URL not set");
        return Ok(());
    };

    let upstream = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let fork_block = resolve_fork_block(&upstream).await?;
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    let manager = StateSpaceBuilder::new(provider.clone())
        .block(fork_block)
        .with_amms(vec![AMM::CurveLegacyPool(CurveLegacyPool::new(
            SUSDE_META_POOL,
            CurveLegacyPoolType::StableSwap,
        ))])
        .sync()
        .await?;

    let state = manager.state.read().await;
    let meta_pool = match state.get(&SUSDE_META_POOL) {
        Some(AMM::CurveLegacyPool(pool)) => pool,
        other => return Err(eyre!("expected sUSDe meta pool, got {:?}", other)),
    };
    let base_pool = match state.get(&THREE_POOL) {
        Some(AMM::CurveLegacyPool(pool)) => pool,
        other => return Err(eyre!("expected 3pool base dependency, got {:?}", other)),
    };

    assert!(meta_pool.is_meta_pool());
    assert_eq!(meta_pool.base_pool_address, Some(THREE_POOL));
    assert_eq!(meta_pool.underlying_coins, vec![SUSDE, DAI, USDC, USDT]);
    assert!(!base_pool.is_meta_pool());
    assert_eq!(base_pool.address, THREE_POOL);
    assert_eq!(base_pool.tokens(), vec![DAI, USDC, USDT]);

    Ok(())
}

#[tokio::test]
async fn test_legacy_crypto_meta_detection_and_zap_on_ethereum_fork() -> Result<()> {
    let Some(rpc_url) = ethereum_provider_url() else {
        println!("Skipping: ETHEREUM_PROVIDER/ETH_RPC_URL not set");
        return Ok(());
    };

    let upstream = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let fork_block = resolve_fork_block(&upstream).await?;
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    let mut pool = CurveLegacyPool::new(CVX_META_POOL, CurveLegacyPoolType::CryptoSwap);
    pool.zap_address = Some(CVX_META_ZAP);
    let pool = pool.init(BlockId::number(fork_block), provider).await?;

    assert!(pool.is_meta_pool());
    assert_eq!(pool.tokens(), vec![CVX, FRAX, USDC]);
    assert_eq!(pool.base_pool_address, Some(BASE_FRAX_USDC));

    Ok(())
}

#[tokio::test]
async fn test_legacy_crypto_meta_quote_parity_on_ethereum_fork() -> Result<()> {
    let Some(rpc_url) = ethereum_provider_url() else {
        println!("Skipping: ETHEREUM_PROVIDER/ETH_RPC_URL not set");
        return Ok(());
    };

    let upstream = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let fork_block = resolve_fork_block(&upstream).await?;
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let block_id = BlockId::number(fork_block);

    let mut pool = CurveLegacyPool::new(CVX_META_POOL, CurveLegacyPoolType::CryptoSwap);
    pool.zap_address = Some(CVX_META_ZAP);
    let pool = pool.init(block_id, provider.clone()).await?;

    assert!(pool.is_meta_pool());

    let meta_contract = ICurveCryptoGetDy::new(CVX_META_POOL, provider.clone());
    let cases = [
        (
            "CVX -> LP",
            0usize,
            1usize,
            U256::from(10_000000000000000000u128),
        ),
        (
            "LP -> CVX",
            1usize,
            0usize,
            U256::from(10_000000000000000000u128),
        ),
    ];

    for (label, i, j, amount_in) in &cases {
        let local_out = pool.simulate_swap(pool.coins[*i], pool.coins[*j], *amount_in)?;
        let chain_out = meta_contract
            .get_dy(U256::from(*i), U256::from(*j), *amount_in)
            .block(block_id)
            .call()
            .await?;

        println!(
            "quote parity [cryptoMeta]: {} amount_in={} local={} chain={}",
            label, amount_in, local_out, chain_out
        );
        assert_diff_within_ppm(local_out, chain_out, 10);
    }

    Ok(())
}

#[tokio::test]
async fn test_legacy_crypto_meta_state_space_adds_top_level_base_pool_on_ethereum_fork(
) -> Result<()> {
    let Some(rpc_url) = ethereum_provider_url() else {
        println!("Skipping: ETHEREUM_PROVIDER/ETH_RPC_URL not set");
        return Ok(());
    };

    let upstream = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let fork_block = resolve_fork_block(&upstream).await?;
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    let mut seed = CurveLegacyPool::new(CVX_META_POOL, CurveLegacyPoolType::CryptoSwap);
    seed.zap_address = Some(CVX_META_ZAP);

    let manager = StateSpaceBuilder::new(provider.clone())
        .block(fork_block)
        .with_amms(vec![AMM::CurveLegacyPool(seed)])
        .sync()
        .await?;

    let state = manager.state.read().await;
    let meta_pool = match state.get(&CVX_META_POOL) {
        Some(AMM::CurveLegacyPool(pool)) => pool,
        other => return Err(eyre!("expected CVX meta pool, got {:?}", other)),
    };
    let base_pool = match state.get(&BASE_FRAX_USDC) {
        Some(AMM::CurveLegacyPool(pool)) => pool,
        other => return Err(eyre!("expected FRAX/USDC base dependency, got {:?}", other)),
    };

    assert!(meta_pool.is_meta_pool());
    assert_eq!(meta_pool.base_pool_address, Some(BASE_FRAX_USDC));
    assert_eq!(meta_pool.tokens(), vec![CVX, FRAX, USDC]);
    assert!(!base_pool.is_meta_pool());
    assert_eq!(base_pool.address, BASE_FRAX_USDC);

    Ok(())
}

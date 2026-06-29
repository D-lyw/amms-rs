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

const DEFAULT_META_FORK_BLOCK: u64 = 478_251_736;

const EURS_2CRV_META_POOL: Address = address!("a827a652ead76c6b0b3d19dba05452e06e25c27e");
const EURS_2CRV_ZAP: Address = address!("25e2e8d104bc1a70492e2be32da7c1f8367f9d2c");
const BASE_POOL_2CRV: Address = address!("7f90122BF0700F9E7e1F688fe926940E8839F353");
const EURS: Address = address!("D22a58f79e9481D1a88e00c343885A588b34b68B");
const USDC_E: Address = address!("FF970A61A04b1cA14834A43f5dE4533eBDDB5CC8");
const USDT: Address = address!("Fd086bC7CD5C481DCC9C85ebE478A1C0b69FCbb9");

sol! {
    #[sol(rpc)]
    interface ICurveMetaPoolZap {
        function get_dy_underlying(uint256 i, uint256 j, uint256 dx) external view returns (uint256);
    }
}

fn arbitrum_provider_url() -> Option<String> {
    dotenv::dotenv().ok();
    env::var("ARBITRUM_PROVIDER")
        .or_else(|_| env::var("ARBITRUM_RPC_URL"))
        .ok()
        .or_else(|| Some("https://arb1.arbitrum.io/rpc".to_string()))
}

async fn resolve_fork_block<P: Provider>(provider: &P) -> Result<u64> {
    match env::var("CURVE_LEGACY_META_FORK_BLOCK") {
        Ok(raw) => Ok(raw.parse::<u64>()?),
        Err(_) => {
            let _ = provider;
            Ok(DEFAULT_META_FORK_BLOCK)
        }
    }
}

#[tokio::test]
async fn test_curve_legacy_meta_pool_detection_on_arbitrum_fork() -> Result<()> {
    let Some(rpc_url) = arbitrum_provider_url() else {
        println!(
            "Skipping meta pool detection fork test: ARBITRUM_PROVIDER/ARBITRUM_RPC_URL not set"
        );
        return Ok(());
    };

    let upstream = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let fork_block = resolve_fork_block(&upstream).await?;
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let manager = StateSpaceBuilder::new(provider.clone())
        .block(fork_block)
        .with_amms({
            let mut pool = CurveLegacyPool::new(EURS_2CRV_META_POOL, CurveLegacyPoolType::CryptoSwap);
            pool.zap_address = Some(EURS_2CRV_ZAP);
            vec![AMM::CurveLegacyPool(pool)]
        })
        .sync()
        .await?;

    let state = manager.state.read().await;
    let pool = match state.get(&EURS_2CRV_META_POOL) {
        Some(AMM::CurveLegacyPool(pool)) => pool,
        other => return Err(eyre!("expected CurveLegacyPool in state, got {:?}", other)),
    };
    let base_pool = match state.get(&BASE_POOL_2CRV) {
        Some(AMM::CurveLegacyPool(pool)) => pool,
        other => {
            return Err(eyre!(
                "expected base CurveLegacyPool in state, got {:?}",
                other
            ))
        }
    };

    assert!(
        pool.is_meta_pool(),
        "expected EURS-2Crv to be detected as meta pool"
    );
    assert_eq!(pool.base_pool_address, Some(BASE_POOL_2CRV));
    assert_eq!(pool.base_token_index, Some(1));
    assert_eq!(pool.underlying_coins, vec![EURS, USDC_E, USDT]);
    assert!(
        pool.base_pool_view.is_some(),
        "expected attached base pool view"
    );
    assert_eq!(pool.tokens(), vec![EURS, USDC_E, USDT]);
    assert!(
        !base_pool.is_meta_pool(),
        "base pool should remain a top-level non-meta pool"
    );
    assert_eq!(base_pool.address, BASE_POOL_2CRV);
    assert_eq!(base_pool.tokens(), vec![USDC_E, USDT]);

    Ok(())
}

#[tokio::test]
async fn test_curve_legacy_meta_pool_underlying_quote_parity_on_arbitrum_fork() -> Result<()> {
    let Some(rpc_url) = arbitrum_provider_url() else {
        println!("Skipping meta pool fork test: ARBITRUM_PROVIDER/ARBITRUM_RPC_URL not set");
        return Ok(());
    };

    let upstream = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let fork_block = resolve_fork_block(&upstream).await?;
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let block_id = BlockId::number(fork_block);
    let manager = StateSpaceBuilder::new(provider.clone())
        .block(fork_block)
        .with_amms({
            let mut pool = CurveLegacyPool::new(EURS_2CRV_META_POOL, CurveLegacyPoolType::CryptoSwap);
            pool.zap_address = Some(EURS_2CRV_ZAP);
            vec![AMM::CurveLegacyPool(pool)]
        })
        .sync()
        .await?;

    let state = manager.state.read().await;
    let pool = match state.get(&EURS_2CRV_META_POOL) {
        Some(AMM::CurveLegacyPool(pool)) => pool,
        other => return Err(eyre!("expected CurveLegacyPool in state, got {:?}", other)),
    };
    if !matches!(state.get(&BASE_POOL_2CRV), Some(AMM::CurveLegacyPool(_))) {
        return Err(eyre!(
            "expected synthesized base pool to be present in state"
        ));
    }
    let zap = ICurveMetaPoolZap::new(EURS_2CRV_ZAP, provider.clone());

    assert!(
        pool.is_meta_pool(),
        "expected EURS-2Crv to be detected as meta pool"
    );
    assert_eq!(pool.base_pool_address, Some(BASE_POOL_2CRV));
    assert_eq!(pool.underlying_coins, vec![EURS, USDC_E, USDT]);
    assert_eq!(pool.tokens(), vec![EURS, USDC_E, USDT]);

    let cases = [
        ("EURS -> USDC.e", 0usize, 1usize, U256::from(100u64)),
        ("EURS -> USDT", 0usize, 2usize, U256::from(100u64)),
        ("USDC.e -> EURS", 1usize, 0usize, U256::from(1_000_000u64)),
        ("USDT -> EURS", 2usize, 0usize, U256::from(1_000_000u64)),
        ("USDC.e -> USDT", 1usize, 2usize, U256::from(1_000_000u64)),
    ];

    for (label, i, j, amount_in) in cases {
        let token_in = pool.underlying_coins[i];
        let token_out = pool.underlying_coins[j];
        let local_out = pool.simulate_swap(token_in, token_out, amount_in)?;
        let chain_out = zap
            .get_dy_underlying(U256::from(i), U256::from(j), amount_in)
            .block(block_id)
            .call()
            .await?;

        println!(
            "quote parity: {} amount_in={} local={} chain={}",
            label, amount_in, local_out, chain_out
        );

        assert_diff_within_ppm(local_out, chain_out, 10);
    }

    Ok(())
}

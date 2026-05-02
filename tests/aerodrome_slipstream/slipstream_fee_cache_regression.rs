use alloy::{
    eips::BlockId,
    primitives::{address, Address, TxHash},
    providers::{Provider, ProviderBuilder},
    rpc::{client::ClientBuilder, types::Filter},
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::amms::{
    aerodrome_slipstream::pool::{AerodromeSlipstreamPool, ICLPool},
    amm::{AutomatedMarketMaker, AMM},
};
use std::{env, str::FromStr, sync::Arc};

/// Targeted regression test for the Slipstream swap-sync fee cache bug.
///
/// Background:
/// - We observed a mismatch where local cached `pool.fee` stayed at 100 while
///   chain `fee()` for the same pool/block was 2450.
/// - Root cause was in swap sync ordering (fee cache update timing relative to
///   post-swap state application).
///
/// This test locks that behavior by replaying one known tx at one known block
/// and asserting both:
/// 1) `pool.fee` (cached after sync) == on-chain `fee()`
/// 2) `compute_fee(sync_ts)`         == on-chain `fee()`
const POOL: Address = address!("17f707CF3EDBbd5d9251D4bCDF9Ad70a247D7B84");
const INIT_BLOCK: u64 = 45_463_469;
const SYNC_BLOCK: u64 = 45_463_470;
const TARGET_TX: &str = "0x3c6ee5a3bf7acecd0fc8a45c3e2bead9580a0b4a5d3be9285283dbebfae7a373";
const EXPECTED_FEE: u32 = 2450;

fn base_provider_url() -> Option<String> {
    dotenv::dotenv().ok();
    env::var("BASE_PROVIDER")
        .or_else(|_| env::var("BASE_RPC_URL"))
        .ok()
}

#[tokio::test]
async fn test_slipstream_fee_cache_matches_onchain_after_swap_sync() -> eyre::Result<()> {
    // Network-bound integration test. Skip gracefully when no RPC endpoint is configured.
    let rpc_endpoint = match base_provider_url() {
        Some(url) => url,
        None => {
            println!("Skipping test: BASE_PROVIDER or BASE_RPC_URL not set");
            return Ok(());
        }
    };

    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(300))
        .layer(RetryBackoffLayer::new(5, 200, 350))
        .http(rpc_endpoint.parse()?);
    let provider = Arc::new(ProviderBuilder::new().connect_client(client));

    // Initialize exact historical pre-state.
    let mut pool = AerodromeSlipstreamPool::new(POOL)
        .init::<_, _>(BlockId::from(INIT_BLOCK), provider.clone())
        .await?;

    let sync_block = provider
        .get_block(BlockId::from(SYNC_BLOCK))
        .await?
        .ok_or_else(|| eyre::eyre!("block {SYNC_BLOCK} not found"))?;
    let sync_ts = sync_block.header.timestamp as u32;

    let sync_topics = {
        let amm = AMM::AerodromeSlipstreamPool(pool.clone());
        amm.sync_events()
    };

    // Narrow scope to one tx so this test remains stable and actionable.
    let target_tx = TxHash::from_str(TARGET_TX)?;
    let mut logs = provider
        .get_logs(
            &Filter::new()
                .address(POOL)
                .event_signature(sync_topics)
                .from_block(SYNC_BLOCK)
                .to_block(SYNC_BLOCK),
        )
        .await?
        .into_iter()
        .filter(|l| l.transaction_hash == Some(target_tx))
        .collect::<Vec<_>>();

    logs.sort_by(|a, b| {
        let a_tx = a.transaction_index.unwrap_or(0);
        let b_tx = b.transaction_index.unwrap_or(0);
        if a_tx != b_tx {
            return a_tx.cmp(&b_tx);
        }
        a.log_index.unwrap_or(0).cmp(&b.log_index.unwrap_or(0))
    });

    assert_eq!(
        logs.len(),
        1,
        "expected exactly 1 pool log in target tx at block {SYNC_BLOCK}"
    );

    // Replay the exact log sequence for this tx into local pool state.
    for log in &logs {
        let mut amm = AMM::AerodromeSlipstreamPool(pool.clone());
        amm.sync(log)?;
        if let AMM::AerodromeSlipstreamPool(updated) = amm {
            pool = updated;
        }
    }

    // Chain baseline at the same block.
    let onchain_fee = ICLPool::new(POOL, provider.clone())
        .fee()
        .block(BlockId::from(SYNC_BLOCK))
        .call()
        .await?
        .to::<u32>();

    // Ensure test vectors are still anchored to the known incident.
    assert_eq!(onchain_fee, EXPECTED_FEE, "unexpected chain fee baseline");
    // Regression assertion #1: sync-updated cached fee must match chain fee.
    assert_eq!(
        pool.fee, onchain_fee,
        "cached pool.fee must match on-chain fee after sync"
    );
    // Regression assertion #2: direct recomputation must also match chain fee.
    assert_eq!(
        pool.compute_fee(sync_ts),
        onchain_fee,
        "compute_fee(sync_ts) must match on-chain fee"
    );

    Ok(())
}

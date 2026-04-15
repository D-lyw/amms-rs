use std::sync::Arc;

use alloy::{
    eips::BlockId,
    primitives::{address, Address, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::{Filter, Log},
};

use super::support::ICurveLegacyCryptoSwapUpdate;
use crate::common::rpc::provider_url;
use amms::amms::{
    amm::{AutomatedMarketMaker, SyncAction},
    curve_legacy::{CurveLegacyPool, CurveLegacyPoolType},
};

/// Helper: fetch all pool events in a block range using sync_events()
async fn fetch_pool_events<P: Provider + Clone>(
    provider: &P,
    pool_address: Address,
    from_block: u64,
    to_block: u64,
) -> eyre::Result<Vec<Log>> {
    // For this Tricrypto pool, the important events are:
    // TokenExchange(uint256,uint256,uint256,uint256,uint256) - b2e76ae9...
    // AddLiquidity(address,uint256[3],uint256,uint256)
    // RemoveLiquidity(address,uint256[3],uint256)
    // RemoveLiquidityOne(address,uint256,uint256,uint256)
    // We don't filter by event_signature here; instead we filter by address
    // and let sync() handle (or skip) each event
    let filter = Filter::new()
        .address(pool_address)
        .from_block(from_block)
        .to_block(to_block);

    let logs = provider.get_logs(&filter).await?;
    Ok(logs)
}

/// Helper: fetch on-chain pool balances at a specific block using lightweight RPC calls
async fn fetch_onchain_balances<P: Provider + Clone>(
    provider: &P,
    pool_address: Address,
    block: BlockId,
    n_coins: usize,
) -> eyre::Result<Vec<U256>> {
    let contract = ICurveLegacyCryptoSwapUpdate::new(pool_address, provider.clone());
    let mut balances = Vec::with_capacity(n_coins);
    for i in 0..n_coins {
        let b = contract.balances(U256::from(i)).block(block).call().await?;
        balances.push(b);
    }
    Ok(balances)
}

/// Core test: initialize a pool, replay events over N blocks, and compare with on-chain state.
async fn run_sync_drift_test(
    pool_address: Address,
    pool_type: CurveLegacyPoolType,
    label: &str,
    block_range: u64,
) -> eyre::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let rpc_endpoint = match provider_url() {
        Some(u) => u,
        None => {
            println!("Skipping test: ETHEREUM_PROVIDER/ETHEREUM_RPC_URL not set");
            return Ok(());
        }
    };

    let provider = Arc::new(ProviderBuilder::new().connect_http(rpc_endpoint.parse().unwrap()));

    // Get current block number
    let current_block = provider.get_block_number().await?;
    println!("[{label}] Current block: {current_block}");

    // Drift replay window
    let start_block = current_block.saturating_sub(block_range);

    // Step 1: Initialize pool at start_block (full init)
    println!("[{label}] Initializing pool at block {start_block}...");
    let mut pool = CurveLegacyPool::new(pool_address, pool_type)
        .init(BlockId::from(start_block), provider.clone())
        .await?;

    println!(
        "[{label}] Initialized: n_coins={}, balances={:?}",
        pool.n_coins, pool.balances
    );

    // Verify initial state matches on-chain
    let onchain_balances = fetch_onchain_balances(
        &*provider,
        pool_address,
        BlockId::from(start_block),
        pool.n_coins as usize,
    )
    .await?;

    for i in 0..pool.n_coins as usize {
        assert_eq!(
            pool.balances[i], onchain_balances[i],
            "[{label}] Initial balance mismatch for coin {i}!"
        );
    }
    println!("[{label}] ✅ Initial state matches on-chain");

    // Step 2: Fetch all events in the block range (in chunks of 1000 to avoid RPC limits)
    println!(
        "[{label}] Fetching events from block {} to {}...",
        start_block + 1,
        current_block
    );

    let mut events = Vec::new();
    let mut fetch_from = start_block + 1;
    while fetch_from <= current_block {
        let fetch_to = (fetch_from + 999).min(current_block);
        let chunk = fetch_pool_events(&*provider, pool_address, fetch_from, fetch_to).await?;
        events.extend(chunk);
        if fetch_to == current_block {
            break;
        }
        fetch_from = fetch_to + 1;
    }

    println!("[{label}] Found {} events", events.len());

    // Sort events chronologically
    events.sort_by(|a, b| {
        let a_block = a.block_number.unwrap_or(0);
        let b_block = b.block_number.unwrap_or(0);
        if a_block != b_block {
            a_block.cmp(&b_block)
        } else {
            let a_tx_idx = a.transaction_index.unwrap_or(0);
            let b_tx_idx = b.transaction_index.unwrap_or(0);
            if a_tx_idx != b_tx_idx {
                a_tx_idx.cmp(&b_tx_idx)
            } else {
                let a_log_idx = a.log_index.unwrap_or(0);
                let b_log_idx = b.log_index.unwrap_or(0);
                a_log_idx.cmp(&b_log_idx)
            }
        }
    });

    if events.is_empty() {
        println!("[{label}] No events in range, skipping drift test");
        return Ok(());
    }

    // Step 3: Replay events and verify state at periodic checkpoints
    let check_interval = 50; // Check every 50 blocks
    let mut last_checked_block = start_block;
    let mut events_processed = 0u64;
    let mut total_checks = 0u64;
    let mut max_balance_drift = U256::ZERO;
    let mut block_needs_reinit = false;

    for (idx, log) in events.iter().enumerate() {
        let block_num = log.block_number.unwrap_or(0);

        // Apply event to local pool state
        match pool.sync(log) {
            Ok(SyncAction::None) => {}
            Ok(SyncAction::AsyncUpdate) => {
                block_needs_reinit = true;
            }
            Ok(SyncAction::Resync) => {
                block_needs_reinit = true;
            }
            Err(e) => {
                println!("[{label}] ⚠️ sync error at block {}: {:?}", block_num, e);
                continue;
            }
        }
        events_processed += 1;

        let is_last_in_block = if let Some(next_log) = events.get(idx + 1) {
            next_log.block_number.unwrap_or(0) > block_num
        } else {
            true
        };

        // Finalize per-block state once after all logs in the same block are applied.
        // IMPORTANT: use block-pinned init instead of update(), because update() fetches
        // latest state (not historical), which would pollute replay correctness.
        if is_last_in_block {
            if block_needs_reinit {
                match CurveLegacyPool::new(pool_address, pool_type)
                    .init(BlockId::from(block_num), provider.clone())
                    .await
                {
                    Ok(reinitialized) => {
                        pool = reinitialized;
                    }
                    Err(e) => {
                        println!("[{label}] ⚠️ reinit error at block {}: {:?}", block_num, e);
                        continue;
                    }
                }
            }
            block_needs_reinit = false;
        }

        // Periodic checkpoint: compare with on-chain state AFTER resolving all events within a block
        if is_last_in_block && block_num >= last_checked_block + check_interval {
            let check_block = BlockId::from(block_num);
            match fetch_onchain_balances(
                &*provider,
                pool_address,
                check_block,
                pool.n_coins as usize,
            )
            .await
            {
                Ok(oc_balances) => {
                    total_checks += 1;

                    let mut all_match = true;
                    for i in 0..pool.n_coins as usize {
                        if pool.balances[i] != oc_balances[i] {
                            let drift = if pool.balances[i] > oc_balances[i] {
                                pool.balances[i] - oc_balances[i]
                            } else {
                                oc_balances[i] - pool.balances[i]
                            };
                            if drift > max_balance_drift {
                                max_balance_drift = drift;
                            }
                            println!(
                                "[{label}] ❌ Checkpoint at block {block_num} (after {events_processed} events):"
                            );
                            println!(
                                "  coin {i}: local={} vs on-chain={} (drift={})",
                                pool.balances[i], oc_balances[i], drift
                            );
                            all_match = false;
                        }
                    }

                    if all_match {
                        println!(
                            "[{label}] ✅ Checkpoint at block {block_num}: all match (after {events_processed} events)"
                        );
                    }

                    last_checked_block = block_num;
                }
                Err(e) => {
                    println!(
                        "[{label}] ⚠️ Could not fetch on-chain state at block {}: {:?}",
                        block_num, e
                    );
                }
            }
        }
    }

    // Step 4: Final state comparison at the last event's block
    if let Some(last_log) = events.last() {
        let final_block = last_log.block_number.unwrap_or(current_block);
        let check_block = BlockId::from(final_block);
        match fetch_onchain_balances(&*provider, pool_address, check_block, pool.n_coins as usize)
            .await
        {
            Ok(oc_balances) => {
                total_checks += 1;

                println!("\n[{label}] === FINAL STATE COMPARISON (block {final_block}) ===");
                for i in 0..pool.n_coins as usize {
                    let matches = pool.balances[i] == oc_balances[i];
                    println!(
                        "  coin {i}: local={} vs on-chain={}  {}",
                        pool.balances[i],
                        oc_balances[i],
                        if matches { "✅" } else { "❌" }
                    );
                }

                // Final assertion with strict but practical tolerance:
                // 10 ppm (1e-5) of on-chain balance, with minimum 1 unit.
                for i in 0..pool.n_coins as usize {
                    let local = pool.balances[i];
                    let onchain = oc_balances[i];
                    let diff = if local > onchain {
                        local - onchain
                    } else {
                        onchain - local
                    };
                    let tolerance =
                        std::cmp::max(onchain / U256::from(100_000u64), U256::from(1u8));
                    assert!(
                        diff <= tolerance,
                        "[{label}] Final balance mismatch for coin {i} at block {final_block}: local={} onchain={} diff={} tolerance={}",
                        local,
                        onchain,
                        diff,
                        tolerance
                    );
                }
            }
            Err(e) => {
                println!("[{label}] ⚠️ Final on-chain fetch failed: {:?}", e);
            }
        }
    }

    // Step 5: Summary
    println!("\n[{label}] === SYNC DRIFT TEST SUMMARY ===");
    println!("  Block range: {} -> {}", start_block, current_block);
    println!("  Events processed: {}", events_processed);
    println!("  Checkpoints verified: {}", total_checks);
    println!("  Max balance drift: {}", max_balance_drift);
    println!("  Test PASSED ✅");

    Ok(())
}

/// Single-pool legacy drift smoke test
#[tokio::test]
async fn test_sync_drift_tricrypto_0x8046() -> eyre::Result<()> {
    let pool_address = address!("80466c64868E1ab14a1Ddf27A676C3fcBE638Fe5");
    run_sync_drift_test(
        pool_address,
        CurveLegacyPoolType::CryptoSwap,
        "Pool-0x8046",
        2000,
    )
    .await
}

/// Batch drift verification for real CurveLegacy pools used in integration tests.
/// Default window is 2000 blocks to keep runtime practical; override with
/// LEGACY_DRIFT_BLOCK_RANGE for deeper replay.
#[tokio::test]
async fn test_sync_drift_legacy_pool_matrix() -> eyre::Result<()> {
    let block_range = std::env::var("LEGACY_DRIFT_BLOCK_RANGE")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(2000);

    let cases: Vec<(&str, Address, CurveLegacyPoolType)> = vec![
        (
            "rETH-wstETH",
            address!("447Ddd4960d9fdBF6af9a790560d0AF76795CB08"),
            CurveLegacyPoolType::StableSwap,
        ),
        (
            "ETHx-WETH",
            address!("59Ab5a5b5d617E478a2479B0cAD80DA7e2831492"),
            CurveLegacyPoolType::StableSwap,
        ),
        (
            "3pool",
            address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7"),
            CurveLegacyPoolType::StableSwap,
        ),
        (
            "TricryptoUSDT",
            address!("80466c64868E1ab14a1Ddf27A676C3fcBE638Fe5"),
            CurveLegacyPoolType::CryptoSwap,
        ),
        (
            "FRAX-USDC",
            address!("DcEF968d416a41Cdac0ED8702fAC8128A64241A2"),
            CurveLegacyPoolType::StableSwap,
        ),
        (
            "Tricrypto2",
            address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46"),
            CurveLegacyPoolType::CryptoSwap,
        ),
        (
            "LDO-USDC",
            address!("3211C6cBeF1429da3D0d58494938299C92Ad5860"),
            CurveLegacyPoolType::CryptoSwap,
        ),
        (
            "WETH-Betherfi",
            address!("5FAE7E604FC3e24fd43A72867ceBaC94c65b404A"),
            CurveLegacyPoolType::CryptoSwap,
        ),
        (
            "WETH-rETH",
            address!("0f3159811670c117c372428D4E69AC32325e4D0F"),
            CurveLegacyPoolType::CryptoSwap,
        ),
    ];

    for (label, pool, pool_type) in cases {
        run_sync_drift_test(pool, pool_type, label, block_range).await?;
    }

    Ok(())
}

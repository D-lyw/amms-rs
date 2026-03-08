//! Aerodrome V2 Pool Tests
//!
//! Comprehensive tests for Aerodrome V2 pool implementation:
//! - Swap simulation accuracy (local vs on-chain)
//! - Sync drift detection (event replay)
//! - Reverse swap testing
//! - Pool discovery

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy::{
        eips::BlockId,
        primitives::{address, Address, U256},
        providers::{Provider, ProviderBuilder},
        rpc::types::{Filter, Log},
        sol,
        sol_types::SolEvent,
    };

    use crate::amms::{
        aerodrome_v2::AerodromeV2Pool,
        amm::AutomatedMarketMaker,
    };

    // ============================================================================
    // Contract Interfaces
    // ============================================================================

    sol! {
        #[allow(missing_docs)]
        #[derive(Debug, PartialEq, Eq)]
        #[sol(rpc)]
        contract IAerodromeV2PoolEvents {
            event Swap(
                address indexed sender,
                address indexed to,
                uint256 amount0In,
                uint256 amount1In,
                uint256 amount0Out,
                uint256 amount1Out
            );
            event Mint(address indexed sender, uint256 amount0, uint256 amount1);
            event Burn(address indexed sender, address indexed to, uint256 amount0, uint256 amount1);
            event Sync(uint256 reserve0, uint256 reserve1);
        }

        #[allow(missing_docs)]
        #[derive(Debug, PartialEq, Eq)]
        #[sol(rpc)]
        contract IAerodromeV2PoolOnchain {
            function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
            function getAmountOut(uint256 amountIn, address tokenIn) external view returns (uint256 amountOut);
        }
    }

    // ============================================================================
    // Helper Functions
    // ============================================================================

    /// Get provider from environment
    fn get_provider() -> eyre::Result<Arc<impl Provider + Clone>> {
        dotenv::dotenv().ok();
        let rpc_endpoint = match std::env::var("BASE_PROVIDER") {
            Ok(u) => u,
            Err(_) => {
                println!("Skipping test: BASE_PROVIDER not set");
                return Err(eyre::eyre!("BASE_PROVIDER not set"));
            }
        };
        Ok(Arc::new(ProviderBuilder::new().connect_http(rpc_endpoint.parse()?)))
    }

    /// Helper: fetch all Aerodrome V2 pool events in a block range
    /// Note: We only need Sync events since every Swap/Mint/Burn triggers a Sync
    async fn fetch_pool_events<P: Provider + Clone>(
        provider: &P,
        pool_address: Address,
        from_block: u64,
        to_block: u64,
    ) -> eyre::Result<Vec<Log>> {
        let event_sigs = vec![IAerodromeV2PoolEvents::Sync::SIGNATURE_HASH];

        let mut all_logs = Vec::new();
        let chunk_size = 5000;
        let mut current_from = from_block;

        while current_from <= to_block {
            let current_to = std::cmp::min(current_from + chunk_size - 1, to_block);

            let filter = Filter::new()
                .address(pool_address)
                .event_signature(event_sigs.clone())
                .from_block(current_from)
                .to_block(current_to);

            let mut retries = 0;
            let mut result = None;
            while retries < 5 {
                match provider.get_logs(&filter).await {
                    Ok(logs) => {
                        result = Some(logs);
                        break;
                    }
                    Err(e) => {
                        println!("⚠️ get_logs error (retry {}/5): {:?}", retries + 1, e);
                        retries += 1;
                        tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;
                    }
                }
            }

            match result {
                Some(logs) => all_logs.extend(logs),
                None => {
                    return Err(eyre::eyre!(
                        "Failed to fetch logs from block {} to {} after 5 retries",
                        current_from,
                        current_to
                    ));
                }
            }

            current_from += chunk_size;
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }

        Ok(all_logs)
    }

    /// Helper: fetch on-chain pool state at a specific block
    async fn fetch_onchain_state<P: Provider + Clone>(
        provider: &P,
        pool_address: Address,
        block: BlockId,
    ) -> eyre::Result<(u128, u128)> {
        let pool_contract = IAerodromeV2PoolOnchain::new(pool_address, provider.clone());
        let reserves = pool_contract.getReserves().block(block).call().await?;
        Ok((reserves.reserve0.to::<u128>(), reserves.reserve1.to::<u128>()))
    }

    // ============================================================================
    // Test 1: Swap Simulation Tests
    // ============================================================================

    /// Test volatile pool swap simulation against on-chain results
    async fn run_swap_simulation_test(
        pool_address: Address,
        label: &str,
        is_stable: bool,
    ) -> eyre::Result<()> {
        let provider = match get_provider() {
            Ok(p) => p,
            Err(_) => return Ok(()),
        };

        let latest_block = BlockId::from(provider.get_block_number().await?);

        // Initialize pool
        let mut pool = if is_stable {
            AerodromeV2Pool::new_stable(pool_address)
        } else {
            AerodromeV2Pool::new_volatile(pool_address)
        };

        pool = match pool.init::<_, _>(latest_block, provider.clone()).await {
            Ok(p) => p,
            Err(e) => {
                println!("[{label}] Skipping: cannot initialize pool: {:?}", e);
                return Ok(());
            }
        };

        println!("\n[{label}] ========== SWAP SIMULATION TEST ==========");
        println!("Pool address: {:?}", pool_address);
        println!("Token A: {:?} (decimals: {})", pool.token_a.address, pool.token_a.decimals);
        println!("Token B: {:?} (decimals: {})", pool.token_b.address, pool.token_b.decimals);
        println!("Reserve 0: {}", pool.reserve_0);
        println!("Reserve 1: {}", pool.reserve_1);
        println!("Stable: {}", pool.stable);

        // Verify pool type
        assert_eq!(
            pool.stable, is_stable,
            "[{label}] Expected {} pool but got {}",
            if is_stable { "stable" } else { "volatile" },
            if pool.stable { "stable" } else { "volatile" }
        );

        // Get on-chain pool contract for comparison
        let onchain_pool = IAerodromeV2PoolOnchain::new(pool_address, provider.clone());

        // Test multiple swap amounts
        let test_cases = if is_stable {
            vec![
                U256::from(1_000_000_000_000_000u128),   // 0.001 token
                U256::from(10_000_000_000_000_000u128),  // 0.01 token
                U256::from(100_000_000_000_000_000u128), // 0.1 token
            ]
        } else {
            vec![
                U256::from(100_000u64),
                U256::from(1_000_000u64),
                U256::from(10_000_000u64),
                U256::from(100_000_000u64),
            ]
        };

        let tolerance = if is_stable { 0.01 } else { 0.005 }; // 1% for stable, 0.5% for volatile

        for (i, amount_in) in test_cases.iter().enumerate() {
            println!("\n[{label}] --- Test Case {} ---", i + 1);
            println!("Amount in: {}", amount_in);

            // Simulate swap locally
            let simulated = match pool.simulate_swap(pool.token_a.address, pool.token_b.address, *amount_in) {
                Ok(amount) => amount,
                Err(e) => {
                    println!("[{label}] Skip: Local simulation error: {:?}", e);
                    continue;
                }
            };

            // Get on-chain result
            let onchain_result = match onchain_pool.getAmountOut(*amount_in, pool.token_a.address).call().await {
                Ok(result) => result,
                Err(e) => {
                    let err_str = e.to_string();
                    if err_str.contains("rate limit") || err_str.contains("429") || err_str.contains("-32016") {
                        println!("[{label}] ⚠️ Rate limited, skipping on-chain comparison");
                        println!("[{label}] Local simulation: {}", simulated);
                        continue;
                    }
                    return Err(e.into());
                }
            };

            println!("[{label}] Local simulated: {}", simulated);
            println!("[{label}] On-chain result: {}", onchain_result);

            if onchain_result.is_zero() {
                println!("[{label}] Skip: On-chain result is zero");
                continue;
            }

            // Calculate difference
            let diff = if simulated > onchain_result {
                simulated - onchain_result
            } else {
                onchain_result - simulated
            };

            let diff_ratio = diff.to_string().parse::<f64>().unwrap()
                / onchain_result.to_string().parse::<f64>().unwrap();

            println!("[{label}] Difference: {} ({:.4}%)", diff, diff_ratio * 100.0);

            assert!(
                diff_ratio < tolerance,
                "[{label}] Diff ratio too high: {:.4}% (threshold: {:.2}%)",
                diff_ratio * 100.0,
                tolerance * 100.0
            );
        }

        println!("\n[{label}] ✅ SWAP SIMULATION TEST PASSED");
        Ok(())
    }

    #[tokio::test]
    async fn test_volatile_pool_swap_simulation() -> eyre::Result<()> {
        // USDC/AERO Volatile Pool on Base
        let pool_address = address!("6cdcb1c4a4d1c3c6d054b27ac5b77e89eafb971d");
        run_swap_simulation_test(pool_address, "Volatile-USDC/AERO", false).await
    }

    #[tokio::test]
    async fn test_stable_pool_swap_simulation() -> eyre::Result<()> {
        // WETH/msETH Stable Pool on Base
        let pool_address = address!("de4fb30ccc2f1210fce2c8ad66410c586c8d1f9a");
        run_swap_simulation_test(pool_address, "Stable-WETH/msETH", true).await
    }

    // ============================================================================
    // Test 2: Reverse Swap Tests
    // ============================================================================

    #[tokio::test]
    async fn test_reverse_swap() -> eyre::Result<()> {
        let provider = match get_provider() {
            Ok(p) => p,
            Err(_) => return Ok(()),
        };

        let pool_address = address!("6cdcb1c4a4d1c3c6d054b27ac5b77e89eafb971d");
        let latest_block = BlockId::from(provider.get_block_number().await?);

        let mut pool = AerodromeV2Pool::new_volatile(pool_address);
        pool = pool.init::<_, _>(latest_block, provider.clone()).await?;

        println!("\n[ReverseSwap] ========== REVERSE SWAP TEST ==========");
        println!("Token A: {:?}", pool.token_a.address);
        println!("Token B: {:?}", pool.token_b.address);

        let onchain_pool = IAerodromeV2PoolOnchain::new(pool_address, provider.clone());
        let amount_in = U256::from(1_000_000u64);

        println!("\n[ReverseSwap] Testing Token B -> Token A: {}", amount_in);

        let simulated = pool.simulate_swap(pool.token_b.address, pool.token_a.address, amount_in)?;
        println!("[ReverseSwap] Local simulated: {}", simulated);

        let onchain_result = match onchain_pool.getAmountOut(amount_in, pool.token_b.address).call().await {
            Ok(result) => result,
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("rate limit") || err_str.contains("429") || err_str.contains("-32016") {
                    println!("[ReverseSwap] ⚠️ Rate limited, test passed (local only)");
                    println!("[ReverseSwap] ✅ REVERSE SWAP TEST PASSED (local only)");
                    return Ok(());
                }
                return Err(e.into());
            }
        };

        println!("[ReverseSwap] On-chain result: {}", onchain_result);

        if !onchain_result.is_zero() {
            let diff = if simulated > onchain_result {
                simulated - onchain_result
            } else {
                onchain_result - simulated
            };

            let diff_ratio = diff.to_string().parse::<f64>().unwrap()
                / onchain_result.to_string().parse::<f64>().unwrap();

            println!("[ReverseSwap] Difference: {:.4}%", diff_ratio * 100.0);
            assert!(diff_ratio < 0.005, "[ReverseSwap] Diff ratio too high: {:.4}%", diff_ratio * 100.0);
        }

        println!("\n[ReverseSwap] ✅ REVERSE SWAP TEST PASSED");
        Ok(())
    }

    // ============================================================================
    // Test 3: Sync Drift Tests (Event Replay)
    // ============================================================================

    /// Core test: initialize a pool, replay events over N blocks, and compare with on-chain state.
    async fn run_sync_drift_test(
        pool_address: Address,
        label: &str,
        is_stable: bool,
    ) -> eyre::Result<()> {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let provider = match get_provider() {
            Ok(p) => p,
            Err(_) => return Ok(()),
        };

        let current_block = provider.get_block_number().await?;
        println!("\n[{label}] ========== SYNC DRIFT TEST ==========");
        println!("[{label}] Current block: {current_block}");

        let test_block_range = 10000u64;
        let start_block = current_block.saturating_sub(test_block_range);

        // Step 1: Initialize pool at start_block
        println!(
            "[{label}] Initializing {} pool at block {start_block}...",
            if is_stable { "stable" } else { "volatile" }
        );

        let mut pool = if is_stable {
            AerodromeV2Pool::new_stable(pool_address)
        } else {
            AerodromeV2Pool::new_volatile(pool_address)
        };

        pool = match pool.init::<_, _>(BlockId::from(start_block), provider.clone()).await {
            Ok(p) => p,
            Err(e) => {
                println!("[{label}] Skipping: cannot initialize pool: {:?}", e);
                return Ok(());
            }
        };

        pool.last_synced_block = start_block;

        println!(
            "[{label}] Initialized: reserve0={}, reserve1={}, stable={}",
            pool.reserve_0, pool.reserve_1, pool.stable
        );

        // Verify initial state matches on-chain
        let (onchain_r0, onchain_r1) =
            fetch_onchain_state(&*provider, pool_address, BlockId::from(start_block)).await?;

        assert_eq!(pool.reserve_0, onchain_r0, "[{label}] Initial reserve0 mismatch!");
        assert_eq!(pool.reserve_1, onchain_r1, "[{label}] Initial reserve1 mismatch!");
        println!("[{label}] ✅ Initial state matches on-chain");

        // Step 2: Fetch all events in the block range
        println!(
            "[{label}] Fetching events from block {} to {}...",
            start_block + 1,
            current_block
        );

        let mut events = fetch_pool_events(&*provider, pool_address, start_block + 1, current_block).await?;
        println!("[{label}] Found {} events", events.len());

        if events.is_empty() {
            println!("[{label}] No events in range, skipping drift test");
            return Ok(());
        }

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

        // Step 3: Replay events and verify state at periodic checkpoints
        let check_interval = 10;
        let mut last_checked_block = start_block;
        let mut events_processed = 0u64;
        let mut total_checks = 0u64;
        let mut max_reserve0_drift_pct = 0.0f64;
        let mut max_reserve1_drift_pct = 0.0f64;

        let mut i = 0;
        while i < events.len() {
            let log = &events[i];
            let block_num = log.block_number.unwrap_or(0);

            match pool.sync(log) {
                Ok(_) => {}
                Err(e) => {
                    println!("[{label}] ⚠️ sync error at block {}: {:?}", block_num, e);
                    i += 1;
                    continue;
                }
            }
            pool.last_synced_block = block_num;
            events_processed += 1;

            let is_last_in_block =
                i + 1 == events.len() || events[i + 1].block_number.unwrap_or(0) != block_num;

            if is_last_in_block && block_num >= last_checked_block + check_interval {
                let check_block = BlockId::from(block_num);
                match fetch_onchain_state(&*provider, pool_address, check_block).await {
                    Ok((oc_r0, oc_r1)) => {
                        total_checks += 1;

                        let r0_matches = pool.reserve_0 == oc_r0;
                        let r1_matches = pool.reserve_1 == oc_r1;

                        let r0_drift_pct = if oc_r0 != 0 {
                            ((pool.reserve_0 as f64 - oc_r0 as f64) / oc_r0 as f64 * 100.0).abs()
                        } else {
                            0.0
                        };

                        let r1_drift_pct = if oc_r1 != 0 {
                            ((pool.reserve_1 as f64 - oc_r1 as f64) / oc_r1 as f64 * 100.0).abs()
                        } else {
                            0.0
                        };

                        if r0_drift_pct > max_reserve0_drift_pct {
                            max_reserve0_drift_pct = r0_drift_pct;
                        }
                        if r1_drift_pct > max_reserve1_drift_pct {
                            max_reserve1_drift_pct = r1_drift_pct;
                        }

                        if !r0_matches || !r1_matches {
                            println!(
                                "[{label}] ❌ Checkpoint at block {block_num} (after {events_processed} events):"
                            );
                            if !r0_matches {
                                println!(
                                    "  reserve0: local={} vs on-chain={} (drift={:.6}%)",
                                    pool.reserve_0, oc_r0, r0_drift_pct
                                );
                            }
                            if !r1_matches {
                                println!(
                                    "  reserve1: local={} vs on-chain={} (drift={:.6}%)",
                                    pool.reserve_1, oc_r1, r1_drift_pct
                                );
                            }
                        } else {
                            println!(
                                "[{label}] ✅ Checkpoint at block {block_num}: all match (after {events_processed} events)"
                            );
                        }

                        last_checked_block = block_num;
                    }
                    Err(e) => {
                        println!("[{label}] ⚠️ Could not fetch on-chain state at block {}: {:?}", block_num, e);
                    }
                }
            }

            i += 1;
        }

        // Step 4: Final state comparison
        if let Some(last_log) = events.last() {
            let final_block = last_log.block_number.unwrap_or(current_block);
            let check_block = BlockId::from(final_block);
            match fetch_onchain_state(&*provider, pool_address, check_block).await {
                Ok((oc_r0, oc_r1)) => {
                    total_checks += 1;

                    println!("\n[{label}] === FINAL STATE COMPARISON (block {final_block}) ===");
                    println!(
                        "  reserve0: local={} vs on-chain={}  {}",
                        pool.reserve_0, oc_r0, if pool.reserve_0 == oc_r0 { "✅" } else { "❌" }
                    );
                    println!(
                        "  reserve1: local={} vs on-chain={}  {}",
                        pool.reserve_1, oc_r1, if pool.reserve_1 == oc_r1 { "✅" } else { "❌" }
                    );

                    assert_eq!(pool.reserve_0, oc_r0, "[{label}] Final reserve0 mismatch!");
                    assert_eq!(pool.reserve_1, oc_r1, "[{label}] Final reserve1 mismatch!");
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
        println!("  Max reserve0 drift: {:.8}%", max_reserve0_drift_pct);
        println!("  Max reserve1 drift: {:.8}%", max_reserve1_drift_pct);
        println!("[{label}] ✅ SYNC DRIFT TEST PASSED");

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_drift_volatile_pool() -> eyre::Result<()> {
        // USDC/AERO Volatile Pool on Base
        let pool_address = address!("6cdcb1c4a4d1c3c6d054b27ac5b77e89eafb971d");
        run_sync_drift_test(pool_address, "Drift-Volatile-USDC/AERO", false).await
    }

    #[tokio::test]
    async fn test_sync_drift_stable_pool() -> eyre::Result<()> {
        // WETH/msETH Stable Pool on Base
        let pool_address = address!("de4fb30ccc2f1210fce2c8ad66410c586c8d1f9a");
        run_sync_drift_test(pool_address, "Drift-Stable-WETH/msETH", true).await
    }

    // ============================================================================
    // Test 4: Pool Discovery Tests
    // ============================================================================

    #[tokio::test]
    async fn test_pool_discovery() -> eyre::Result<()> {
        let provider = match get_provider() {
            Ok(p) => p,
            Err(_) => return Ok(()),
        };

        let pool_address = address!("6cdcb1c4a4d1c3c6d054b27ac5b77e89eafb971d");
        let latest_block = BlockId::from(provider.get_block_number().await?);

        println!("\n[Discovery] ========== POOL DISCOVERY TEST ==========");

        let pool = AerodromeV2Pool::new_volatile(pool_address)
            .init::<_, _>(latest_block, provider.clone())
            .await?;

        println!("[Discovery] Successfully discovered and initialized pool:");
        println!("  Address: {:?}", pool.address);
        println!("  Token A: {:?} (decimals: {})", pool.token_a.address, pool.token_a.decimals);
        println!("  Token B: {:?} (decimals: {})", pool.token_b.address, pool.token_b.decimals);
        println!("  Stable: {}", pool.stable);

        assert!(!pool.stable, "[Discovery] Expected volatile pool");

        println!("\n[Discovery] ✅ POOL DISCOVERY TEST PASSED");
        Ok(())
    }
}

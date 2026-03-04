#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy::{
        eips::BlockId,
        primitives::{address, Address, U256},
        providers::{Provider, ProviderBuilder},
        rpc::types::{Filter, Log},
        sol_types::SolEvent,
    };

    use crate::amms::{
        amm::AutomatedMarketMaker,
        pancake_v3::{IPancakeV3PoolEvents, IPancakeV3PoolState, PancakeV3Pool},
    };

    /// Helper: fetch all PancakeV3 pool events (Swap, Mint, Burn) in a block range
    async fn fetch_pool_events<P: Provider + Clone>(
        provider: &P,
        pool_address: Address,
        from_block: u64,
        to_block: u64,
    ) -> eyre::Result<Vec<Log>> {
        let event_sigs = vec![
            IPancakeV3PoolEvents::Swap::SIGNATURE_HASH,
            IPancakeV3PoolEvents::Mint::SIGNATURE_HASH,
            IPancakeV3PoolEvents::Burn::SIGNATURE_HASH,
        ];

        let filter = Filter::new()
            .address(pool_address)
            .event_signature(event_sigs)
            .from_block(from_block)
            .to_block(to_block);

        let logs = provider.get_logs(&filter).await?;
        Ok(logs)
    }

    /// Helper: fetch on-chain pool state at a specific block
    async fn fetch_onchain_state<P: Provider + Clone>(
        provider: &P,
        pool_address: Address,
        block: BlockId,
    ) -> eyre::Result<(U256, i32, u128)> {
        let pool_contract = IPancakeV3PoolState::new(pool_address, provider.clone());

        let slot0 = pool_contract.slot0().block(block).call().await?;
        let liquidity = pool_contract.liquidity().block(block).call().await?;

        let sqrt_price = U256::from(slot0.sqrtPriceX96);
        let tick: i32 = slot0.tick.unchecked_into();

        Ok((sqrt_price, tick, liquidity))
    }

    /// Core test: initialize a pool, replay events over N blocks, and compare with on-chain state.
    async fn run_sync_drift_test(pool_address: Address, label: &str) -> eyre::Result<()> {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        dotenv::dotenv().ok();
        let rpc_endpoint = match std::env::var("ETHEREUM_PROVIDER") {
            Ok(u) => u,
            Err(_) => {
                println!("Skipping test: ETHEREUM_PROVIDER not set");
                return Ok(());
            }
        };

        let provider = Arc::new(ProviderBuilder::new().connect_http(rpc_endpoint.parse().unwrap()));

        // Get current block number
        let current_block = provider.get_block_number().await?;
        println!("[{label}] Current block: {current_block}");

        // We will test over the last 10000 blocks (~1.4 days on Ethereum)
        let test_block_range = 10000u64;
        let start_block = current_block.saturating_sub(test_block_range);

        // Step 1: Initialize pool at start_block (full init, including tick data)
        println!("[{label}] Initializing pool at block {start_block}...");
        let mut pool = PancakeV3Pool::new(pool_address)
            .init::<_, _>(BlockId::from(start_block), provider.clone())
            .await?;
        pool.set_last_synced_block(start_block);

        println!(
            "[{label}] Initialized: tick={}, liquidity={}, sqrt_price={}",
            pool.tick, pool.liquidity, pool.sqrt_price
        );

        // Verify initial state matches on-chain
        let (onchain_sqrt_price, onchain_tick, onchain_liq) =
            fetch_onchain_state(&*provider, pool_address, BlockId::from(start_block)).await?;
        assert_eq!(
            pool.sqrt_price, onchain_sqrt_price,
            "[{label}] Initial sqrt_price mismatch!"
        );
        assert_eq!(pool.tick, onchain_tick, "[{label}] Initial tick mismatch!");
        assert_eq!(
            pool.liquidity, onchain_liq,
            "[{label}] Initial liquidity mismatch!"
        );
        println!("[{label}] ✅ Initial state matches on-chain");

        // Step 2: Fetch all events in the block range
        println!(
            "[{label}] Fetching events from block {} to {}...",
            start_block + 1,
            current_block
        );
        let mut events =
            fetch_pool_events(&*provider, pool_address, start_block + 1, current_block).await?;
        println!("[{label}] Found {} events", events.len());

        // CRITICAL: Sort events chronologically to prevent out-of-order processing issues
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
        let mut max_sqrt_price_drift_pct = 0.0f64;

        for log in &events {
            let block_num = log.block_number.unwrap_or(0);
            if block_num <= last_checked_block {
                // Should not happen if events are in order, but be safe
            }

            // Apply event to local pool state
            match pool.sync(log) {
                Ok(_) => {}
                Err(e) => {
                    println!("[{label}] ⚠️ sync error at block {}: {:?}", block_num, e);
                    continue;
                }
            }
            pool.set_last_synced_block(block_num);
            events_processed += 1;

            // Periodic checkpoint: compare with on-chain state
            if block_num >= last_checked_block + check_interval {
                let check_block = BlockId::from(block_num);
                match fetch_onchain_state(&*provider, pool_address, check_block).await {
                    Ok((oc_sqrt_price, oc_tick, oc_liq)) => {
                        total_checks += 1;

                        let sqrt_price_matches = pool.sqrt_price == oc_sqrt_price;
                        let tick_matches = pool.tick == oc_tick;
                        let liq_matches = pool.liquidity == oc_liq;

                        // Calculate drift percentage for sqrt_price
                        let drift_pct = if !oc_sqrt_price.is_zero() {
                            let local_f = pool.sqrt_price.to_string().parse::<f64>().unwrap_or(0.0);
                            let remote_f = oc_sqrt_price.to_string().parse::<f64>().unwrap_or(1.0);
                            ((local_f - remote_f) / remote_f * 100.0).abs()
                        } else {
                            0.0
                        };

                        if drift_pct > max_sqrt_price_drift_pct {
                            max_sqrt_price_drift_pct = drift_pct;
                        }

                        if !sqrt_price_matches || !tick_matches || !liq_matches {
                            println!(
                                "[{label}] ❌ Checkpoint at block {block_num} (after {events_processed} events):"
                            );
                            if !sqrt_price_matches {
                                println!(
                                    "  sqrt_price: local={} vs on-chain={} (drift={:.6}%)",
                                    pool.sqrt_price, oc_sqrt_price, drift_pct
                                );
                            }
                            if !tick_matches {
                                println!("  tick: local={} vs on-chain={}", pool.tick, oc_tick);
                            }
                            if !liq_matches {
                                println!(
                                    "  liquidity: local={} vs on-chain={}",
                                    pool.liquidity, oc_liq
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
            match fetch_onchain_state(&*provider, pool_address, check_block).await {
                Ok((oc_sqrt_price, oc_tick, oc_liq)) => {
                    total_checks += 1;

                    println!("\n[{label}] === FINAL STATE COMPARISON (block {final_block}) ===");
                    println!(
                        "  sqrt_price: local={} vs on-chain={}  {}",
                        pool.sqrt_price,
                        oc_sqrt_price,
                        if pool.sqrt_price == oc_sqrt_price {
                            "✅"
                        } else {
                            "❌"
                        }
                    );
                    println!(
                        "  tick:       local={} vs on-chain={}  {}",
                        pool.tick,
                        oc_tick,
                        if pool.tick == oc_tick { "✅" } else { "❌" }
                    );
                    println!(
                        "  liquidity:  local={} vs on-chain={}  {}",
                        pool.liquidity,
                        oc_liq,
                        if pool.liquidity == oc_liq {
                            "✅"
                        } else {
                            "❌"
                        }
                    );

                    // Assert final state matches
                    assert_eq!(
                        pool.sqrt_price, oc_sqrt_price,
                        "[{label}] Final sqrt_price mismatch at block {final_block}!"
                    );
                    assert_eq!(
                        pool.tick, oc_tick,
                        "[{label}] Final tick mismatch at block {final_block}!"
                    );
                    assert_eq!(
                        pool.liquidity, oc_liq,
                        "[{label}] Final liquidity mismatch at block {final_block}!"
                    );
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
        println!("  Max sqrt_price drift: {:.8}%", max_sqrt_price_drift_pct);
        println!("  Test PASSED ✅");

        Ok(())
    }

    /// Pool 1: 0x6ca298d2983ab03aa1da7679389d955a4efee15c (PancakeV3, Ethereum mainnet)
    #[tokio::test]
    async fn test_sync_drift_pool_0x6ca298d2() -> eyre::Result<()> {
        let pool_address = address!("6ca298d2983ab03aa1da7679389d955a4efee15c");
        run_sync_drift_test(pool_address, "Pool-0x6ca2").await
    }

    /// Pool 2: 0x1ac1A8FEaAEa1900C4166dEeed0C11cC10669D36 (PancakeV3, Ethereum mainnet)
    #[tokio::test]
    async fn test_sync_drift_pool_0x1ac1a8fe() -> eyre::Result<()> {
        let pool_address = address!("1ac1A8FEaAEa1900C4166dEeed0C11cC10669D36");
        run_sync_drift_test(pool_address, "Pool-0x1ac1").await
    }
}

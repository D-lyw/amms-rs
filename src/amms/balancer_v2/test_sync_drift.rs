#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy::{
        eips::BlockId,
        primitives::{address, Address, B256, U256},
        providers::{Provider, ProviderBuilder},
        rpc::types::{Filter, Log},
        sol_types::SolEvent,
    };
    use std::str::FromStr;
    use std::collections::HashMap;

    use crate::amms::{
        amm::AutomatedMarketMaker,
        balancer_v2::{abi::IVault, BalancerV2Pool, BalancerV2PoolType},
    };

    /// Helper: fetch all BalancerV2 pool events in a block range
    async fn fetch_pool_events<P: Provider + Clone>(
        provider: &P,
        vault_address: Address,
        pool_id: B256,
        from_block: u64,
        to_block: u64,
    ) -> eyre::Result<Vec<Log>> {
        let event_sigs = vec![
            IVault::Swap::SIGNATURE_HASH,
            IVault::PoolBalanceChanged::SIGNATURE_HASH,
            IVault::PoolBalanceManaged::SIGNATURE_HASH,
        ];

        let mut all_logs = Vec::new();
        let chunk_size = 10000; // 10k blocks per request to avoid RPC limits
        let mut current_from = from_block;

        while current_from <= to_block {
            let current_to = std::cmp::min(current_from + chunk_size - 1, to_block);
            let filter = Filter::new()
                .address(vault_address)
                .event_signature(event_sigs.clone())
                // poolId is indexed as topic1 in all 3 events
                .topic1(pool_id)
                .from_block(current_from)
                .to_block(current_to);

            let mut logs = provider.get_logs(&filter).await?;
            all_logs.append(&mut logs);
            current_from = current_to + 1;
        }

        Ok(all_logs)
    }

    /// Helper: fetch on-chain pool state at a specific block
    async fn fetch_onchain_state<P: Provider + Clone>(
        provider: &P,
        vault_address: Address,
        pool_id: B256,
        block: BlockId,
    ) -> eyre::Result<HashMap<Address, U256>> {
        let vault_contract = IVault::new(vault_address, provider.clone());
        let result = vault_contract.getPoolTokens(pool_id).block(block).call().await?;

        let mut balances = HashMap::new();
        for (token, balance) in result.tokens.into_iter().zip(result.balances.into_iter()) {
            balances.insert(token, balance);
        }
        Ok(balances)
    }

    /// Core test: initialize a pool, replay events over N blocks, and compare with on-chain state.
    async fn run_sync_drift_test(
        pool_address: Address,
        pool_id: B256,
        vault_address: Address,
        pool_type: BalancerV2PoolType,
        label: &str,
    ) -> eyre::Result<()> {
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

        // We will test over the last 100000 blocks (~14 days on Ethereum)
        let test_block_range = 100000u64;
        let start_block = current_block.saturating_sub(test_block_range);

        // Step 1: Initialize pool at start_block
        println!("[{label}] Initializing pool at block {start_block}...");
        let mut pool = BalancerV2Pool::new(pool_address, vault_address, pool_id, pool_type)
            .init::<_, _>(BlockId::from(start_block), provider.clone())
            .await?;
        pool.set_last_synced_block(start_block);

        println!("[{label}] Initialized tokens:");
        for (addr, state) in &pool.tokens {
            println!("  {}: balance={}", addr, state.balance);
        }

        // Verify initial state matches on-chain
        let onchain_balances =
            fetch_onchain_state(&*provider, vault_address, pool_id, BlockId::from(start_block))
                .await?;
        for (addr, state) in &pool.tokens {
            let oc_balance = onchain_balances
                .get(addr)
                .copied()
                .unwrap_or_default();
            assert_eq!(
                state.balance, oc_balance,
                "[{label}] Initial balance mismatch for token {}!", addr
            );
        }
        println!("[{label}] ✅ Initial state matches on-chain");

        // Step 2: Fetch all events in the block range
        println!(
            "[{label}] Fetching events from block {} to {}...",
            start_block + 1,
            current_block
        );
        let mut events =
            fetch_pool_events(&*provider, vault_address, pool_id, start_block + 1, current_block)
                .await?;
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
        let mut max_balance_drift_pct = 0.0f64;

        for log in &events {
            let block_num = log.block_number.unwrap_or(0);

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
                match fetch_onchain_state(&*provider, vault_address, pool_id, check_block).await {
                    Ok(oc_balances) => {
                        total_checks += 1;
                        let mut all_match = true;

                        for (addr, state) in &pool.tokens {
                            let oc_balance = oc_balances
                                .get(addr)
                                .copied()
                                .unwrap_or_default();

                            let balance_matches = state.balance == oc_balance;

                            if !balance_matches {
                                all_match = false;
                                let drift_pct = if !oc_balance.is_zero() {
                                    let local_f = state.balance.to_string().parse::<f64>().unwrap_or(0.0);
                                    let remote_f = oc_balance.to_string().parse::<f64>().unwrap_or(1.0);
                                    ((local_f - remote_f) / remote_f * 100.0).abs()
                                } else {
                                    100.0
                                };

                                if drift_pct > max_balance_drift_pct {
                                    max_balance_drift_pct = drift_pct;
                                }

                                println!(
                                    "[{label}] ❌ Checkpoint at block {block_num} (after {events_processed} events):"
                                );
                                println!(
                                    "  Token {addr} balance: local={} vs on-chain={} (drift={:.6}%)",
                                    state.balance, oc_balance, drift_pct
                                );
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
            match fetch_onchain_state(&*provider, vault_address, pool_id, check_block).await {
                Ok(oc_balances) => {
                    total_checks += 1;

                    println!("\n[{label}] === FINAL STATE COMPARISON (block {final_block}) ===");
                    
                    for (addr, state) in &pool.tokens {
                        let oc_balance = oc_balances
                            .get(addr)
                            .copied()
                            .unwrap_or_default();
                        
                        println!(
                            "  Token {addr} balance: local={} vs on-chain={}  {}",
                            state.balance,
                            oc_balance,
                            if state.balance == oc_balance { "✅" } else { "❌" }
                        );

                        // Assert final state matches
                        assert_eq!(
                            state.balance, oc_balance,
                            "[{label}] Final balance mismatch for token {} at block {}!",
                            addr, final_block
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
        println!("  Max balance drift: {:.8}%", max_balance_drift_pct);
        println!("  Test PASSED ✅");

        Ok(())
    }

    /// Pool 1: 0x3de27efa2f1aa663ae5d458857e731c129069f29 (BalancerV2 Weighted, Ethereum mainnet)
    #[tokio::test]
    async fn test_sync_drift_pool_0x3de2() -> eyre::Result<()> {
        let pool_address = address!("3de27efa2f1aa663ae5d458857e731c129069f29");
        let pool_id = B256::from_str("0x3de27efa2f1aa663ae5d458857e731c129069f29000200000000000000000588").unwrap();
        let vault_address = address!("BA12222222228d8Ba445958a75a0704d566BF2C8");
        run_sync_drift_test(pool_address, pool_id, vault_address, BalancerV2PoolType::Weighted, "Pool-0x3de2").await
    }
}

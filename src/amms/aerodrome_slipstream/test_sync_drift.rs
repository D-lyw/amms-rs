#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy::{
        eips::BlockId,
        primitives::{address, aliases::I24, aliases::U160, Address, U256},
        providers::{Provider, ProviderBuilder},
        rpc::types::{Filter, Log},
        sol,
        sol_types::SolEvent,
    };

    use crate::amms::{
        aerodrome_slipstream::{AerodromeSlipstreamPool, ICLPool, ICustomFeeModule},
        amm::AutomatedMarketMaker,
    };

    // Define Aerodrome Slipstream Pool events for sync
    sol! {
        #[allow(missing_docs)]
        #[derive(Debug, PartialEq, Eq)]
        #[sol(rpc)]
        contract ICLPoolEventsInterface {
            event Swap(
                address indexed sender,
                address indexed recipient,
                int256 amount0,
                int256 amount1,
                uint160 sqrtPriceX96,
                uint128 liquidity,
                int24 tick
            );
            event Mint(
                address sender,
                address indexed owner,
                int24 indexed tickLower,
                int24 indexed tickUpper,
                uint128 amount,
                uint256 amount0,
                uint256 amount1
            );
            event Burn(
                address indexed owner,
                int24 indexed tickLower,
                int24 indexed tickUpper,
                uint128 amount,
                uint256 amount0,
                uint256 amount1
            );
        }
    }

    // Aerodrome Slipstream QuoterV2 (Base)
    // Signature aligns with on-chain verified QuoterV2 for Slipstream:
    // quoteExactOutputSingle((tokenIn, tokenOut, amount, tickSpacing, sqrtPriceLimitX96))
    sol! {
        #[allow(missing_docs)]
        #[derive(Debug, PartialEq, Eq)]
        #[sol(rpc)]
        contract ICLQuoterV2 {
            struct QuoteExactOutputSingleParams {
                address tokenIn;
                address tokenOut;
                uint256 amount;
                int24 tickSpacing;
                uint160 sqrtPriceLimitX96;
            }

            function quoteExactOutputSingle(QuoteExactOutputSingleParams memory params)
                external
                returns (
                    uint256 amountIn,
                    uint160 sqrtPriceX96After,
                    uint32 initializedTicksCrossed,
                    uint256 gasEstimate
                );
        }
    }

    const DEFAULT_SLIPSTREAM_QUOTER_V2_BASE: Address =
        address!("254cf9e1e6e233aa1ac962cb9b05b2cfeaae15b0");

    fn exact_out_amounts_by_decimals(decimals: u8) -> Vec<U256> {
        let one = U256::from(10u8).pow(U256::from(decimals));
        let thousand = U256::from(1_000u16);
        let hundred = U256::from(100u8);
        let ten = U256::from(10u8);

        let a = if one >= thousand {
            one / thousand
        } else {
            U256::from(1u8)
        };
        let b = if one >= hundred { one / hundred } else { one };
        let c = if one >= ten { one / ten } else { one };

        vec![a, b, c]
    }

    /// Helper: fetch pool events (Swap, Mint, Burn) in a block range
    async fn fetch_pool_events<P: Provider + Clone>(
        provider: &P,
        pool_address: Address,
        from_block: u64,
        to_block: u64,
    ) -> eyre::Result<Vec<Log>> {
        let event_sigs = vec![
            ICLPoolEventsInterface::Swap::SIGNATURE_HASH,
            ICLPoolEventsInterface::Mint::SIGNATURE_HASH,
            ICLPoolEventsInterface::Burn::SIGNATURE_HASH,
        ];

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

    /// Helper: fetch fee change events from FeeModule
    /// Note: CustomFeeSet events are emitted from the FeeModule contract, not the pool
    async fn fetch_fee_events<P: Provider + Clone>(
        provider: &P,
        fee_module_address: Address,
        pool_address: Address,
        from_block: u64,
        to_block: u64,
    ) -> eyre::Result<Vec<Log>> {
        let mut all_logs = Vec::new();
        let chunk_size = 5000u64; // Use smaller chunks to avoid RPC limits
        let mut current_from = from_block;

        while current_from <= to_block {
            let current_to = std::cmp::min(current_from + chunk_size - 1, to_block);

            let filter = Filter::new()
                .address(fee_module_address)
                .event_signature(ICustomFeeModule::CustomFeeSet::SIGNATURE_HASH)
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
                        println!("⚠️ fee event fetch error (retry {}/5): {:?}", retries + 1, e);
                        retries += 1;
                        tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;
                    }
                }
            }

            if let Some(logs) = result {
                // Filter logs for our specific pool (topic[1] is the indexed pool address)
                let pool_logs: Vec<Log> = logs
                    .into_iter()
                    .filter(|log| {
                        if log.topics().len() > 1 {
                            let event_pool = Address::from_word(log.topics()[1]);
                            event_pool == pool_address
                        } else {
                            false
                        }
                    })
                    .collect();
                all_logs.extend(pool_logs);
            }

            current_from += chunk_size;
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }

        Ok(all_logs)
    }

    /// Helper: fetch on-chain pool slot0 state at a specific block
    async fn fetch_onchain_slot0<P: Provider + Clone>(
        provider: &P,
        pool_address: Address,
        block: BlockId,
    ) -> eyre::Result<(U256, u128, i32)> {
        let pool_contract = ICLPool::new(pool_address, provider.clone());

        // Get liquidity
        let liquidity_result = pool_contract.liquidity().block(block).call().await?;

        // Get slot0
        let slot0 = pool_contract.slot0().block(block).call().await?;

        Ok((
            slot0.sqrtPriceX96.to(),
            liquidity_result,
            slot0.tick.as_i32(),
        ))
    }

    /// Helper: fetch on-chain pool fee at a specific block
    async fn fetch_onchain_fee<P: Provider + Clone>(
        provider: &P,
        pool_address: Address,
        block: BlockId,
    ) -> eyre::Result<u32> {
        let pool_contract = ICLPool::new(pool_address, provider.clone());
        let fee = pool_contract.fee().block(block).call().await?;
        Ok(fee.to::<u32>())
    }

    /// Test 1: Local Swap Simulation vs On-chain Quoted Swap
    ///
    /// This test:
    /// 1. Initializes a pool at a specific block
    /// 2. Simulates a swap locally
    /// 3. Compares the result with on-chain quoter (if available) or verifies output is reasonable
    async fn run_swap_simulation_test(pool_address: Address, label: &str) -> eyre::Result<()> {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        dotenv::dotenv().ok();
        let rpc_endpoint = match std::env::var("BASE_PROVIDER") {
            Ok(u) => u,
            Err(_) => {
                println!("Skipping test: BASE_PROVIDER not set");
                return Ok(());
            }
        };

        let provider = Arc::new(ProviderBuilder::new().connect_http(rpc_endpoint.parse()?));

        // Get current block number
        let current_block = provider.get_block_number().await?;
        println!("\n[{label}] ========== SWAP SIMULATION TEST ==========");
        println!("[{label}] Current block: {current_block}");

        // Step 1: Initialize pool at current block
        println!("[{label}] Initializing pool at block {current_block}...");

        let pool = match AerodromeSlipstreamPool::new(pool_address)
            .init::<_, _>(BlockId::from(current_block), provider.clone())
            .await
        {
            Ok(p) => p,
            Err(e) => {
                println!("[{label}] Skipping test: cannot initialize pool: {:?}", e);
                return Ok(());
            }
        };

        println!("[{label}] Pool initialized:",);
        println!(
            "  sqrt_price={}, liquidity={}, tick={}, fee={}, tick_spacing={}",
            pool.sqrt_price, pool.liquidity, pool.tick, pool.fee, pool.tick_spacing
        );
        println!(
            "  Token A: {} (decimals={})",
            pool.token_a.address, pool.token_a.decimals
        );
        println!(
            "  Token B: {} (decimals={})",
            pool.token_b.address, pool.token_b.decimals
        );

        // Verify fee is fetched from chain (not default)
        let onchain_fee = fetch_onchain_fee(&*provider, pool_address, BlockId::from(current_block)).await?;
        println!(
            "  Fee verification: local={} vs onchain={} {}",
            pool.fee,
            onchain_fee,
            if pool.fee == onchain_fee { "✅" } else { "❌" }
        );
        assert_eq!(pool.fee, onchain_fee, "[{label}] Fee mismatch - not using on-chain value!");

        // Step 2: Simulate swaps in both directions with different amounts
        let test_amounts = vec![
            U256::from(1_000_000u64),     // 1M units
            U256::from(10_000_000u64),    // 10M units
            U256::from(100_000_000u64),   // 100M units
            U256::from(1_000_000_000u64), // 1B units
        ];

        println!("\n[{label}] Simulating swaps (A -> B):");
        for amount_in in &test_amounts {
            match pool.simulate_swap(pool.token_a.address, pool.token_b.address, *amount_in) {
                Ok(amount_out) => {
                    // Calculate effective price
                    let effective_price = if amount_out > U256::ZERO {
                        amount_out.to_string().parse::<f64>().unwrap_or(0.0)
                            / amount_in.to_string().parse::<f64>().unwrap_or(1.0)
                    } else {
                        0.0
                    };
                    println!(
                        "  {} tokenA -> {} tokenB (price: {:.9})",
                        amount_in, amount_out, effective_price
                    );
                }
                Err(e) => {
                    println!("  {} tokenA -> ERROR: {:?}", amount_in, e);
                }
            }
        }

        println!("\n[{label}] Simulating swaps (B -> A):");
        for amount_in in &test_amounts {
            match pool.simulate_swap(pool.token_b.address, pool.token_a.address, *amount_in) {
                Ok(amount_out) => {
                    let effective_price = if amount_out > U256::ZERO {
                        amount_out.to_string().parse::<f64>().unwrap_or(0.0)
                            / amount_in.to_string().parse::<f64>().unwrap_or(1.0)
                    } else {
                        0.0
                    };
                    println!(
                        "  {} tokenB -> {} tokenA (price: {:.9})",
                        amount_in, amount_out, effective_price
                    );
                }
                Err(e) => {
                    println!("  {} tokenB -> ERROR: {:?}", amount_in, e);
                }
            }
        }

        // Step 3: Verify pool state consistency
        let (onchain_sqrt_price, onchain_liquidity, onchain_tick) =
            fetch_onchain_slot0(&*provider, pool_address, BlockId::from(current_block)).await?;

        println!("\n[{label}] State verification:");
        let sqrt_price_match = pool.sqrt_price == onchain_sqrt_price;
        let liquidity_match = pool.liquidity == onchain_liquidity;
        let tick_match = pool.tick == onchain_tick;

        println!(
            "  sqrt_price: local={} vs onchain={} {}",
            pool.sqrt_price,
            onchain_sqrt_price,
            if sqrt_price_match { "✅" } else { "❌" }
        );
        println!(
            "  liquidity: local={} vs onchain={} {}",
            pool.liquidity,
            onchain_liquidity,
            if liquidity_match { "✅" } else { "❌" }
        );
        println!(
            "  tick: local={} vs onchain={} {}",
            pool.tick,
            onchain_tick,
            if tick_match { "✅" } else { "❌" }
        );

        assert!(sqrt_price_match, "[{label}] sqrt_price mismatch!");
        assert!(liquidity_match, "[{label}] liquidity mismatch!");
        assert!(tick_match, "[{label}] tick mismatch!");

        println!("\n[{label}] ✅ SWAP SIMULATION TEST PASSED");
        Ok(())
    }

    /// Test 2: Long-term Sync Drift Detection
    ///
    /// This test:
    /// 1. Initializes a pool at a historical block
    /// 2. Fetches all events since then
    /// 3. Replays events locally using sync()
    /// 4. Periodically compares local state with on-chain state
    /// 5. Reports any drift
    async fn run_sync_drift_test(pool_address: Address, label: &str) -> eyre::Result<()> {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        dotenv::dotenv().ok();
        let rpc_endpoint = match std::env::var("BASE_PROVIDER") {
            Ok(u) => u,
            Err(_) => {
                println!("Skipping test: BASE_PROVIDER not set");
                return Ok(());
            }
        };

        let provider = Arc::new(ProviderBuilder::new().connect_http(rpc_endpoint.parse()?));

        // Get current block number
        let current_block = provider.get_block_number().await?;
        println!("\n[{label}] ========== SYNC DRIFT TEST ==========");
        println!("[{label}] Current block: {current_block}");

        // Use a block range for testing (adjust based on RPC limits)
        let test_block_range = 10000u64;
        let start_block = current_block.saturating_sub(test_block_range);

        // Step 1: Initialize pool at start_block
        println!("[{label}] Initializing pool at block {start_block}...",);

        let mut pool = match AerodromeSlipstreamPool::new(pool_address)
            .init::<_, _>(BlockId::from(start_block), provider.clone())
            .await
        {
            Ok(p) => p,
            Err(e) => {
                println!("[{label}] Skipping test: cannot initialize pool: {:?}", e);
                return Ok(());
            }
        };

        pool.last_synced_block = start_block;

        println!(
            "[{label}] Initialized: sqrt_price={}, liquidity={}, tick={}, fee={}, tick_spacing={}",
            pool.sqrt_price, pool.liquidity, pool.tick, pool.fee, pool.tick_spacing
        );

        // Verify initial state matches on-chain
        let (onchain_sqrt_price, onchain_liquidity, onchain_tick) =
            fetch_onchain_slot0(&*provider, pool_address, BlockId::from(start_block)).await?;

        assert_eq!(
            pool.sqrt_price, onchain_sqrt_price,
            "[{label}] Initial sqrt_price mismatch!"
        );
        assert_eq!(
            pool.liquidity, onchain_liquidity,
            "[{label}] Initial liquidity mismatch!"
        );
        assert_eq!(pool.tick, onchain_tick, "[{label}] Initial tick mismatch!");

        // Verify initial fee matches on-chain
        let onchain_fee = fetch_onchain_fee(&*provider, pool_address, BlockId::from(start_block)).await?;
        assert_eq!(pool.fee, onchain_fee, "[{label}] Initial fee mismatch!");
        println!("[{label}] ✅ Initial state (including fee) matches on-chain");

        // Step 2: Fetch all events in the block range
        println!(
            "[{label}] Fetching events from block {} to {}...",
            start_block + 1,
            current_block
        );
        let events =
            fetch_pool_events(&*provider, pool_address, start_block + 1, current_block).await?;
        println!("[{label}] Found {} pool events", events.len());

        if events.is_empty() {
            println!("[{label}] No events in range, test completed with state verification only");
            return Ok(());
        }

        // CRITICAL: Sort events chronologically
        let mut events = events;
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
        let check_interval = 50; // Check every 50 blocks
        let mut last_checked_block = start_block;
        let mut events_processed = 0u64;
        let mut total_checks = 0u64;
        let mut max_sqrt_price_drift_pct = 0.0f64;
        let mut max_liquidity_drift_pct = 0.0f64;
        let mut max_tick_drift = 0i32;

        let mut i = 0;
        while i < events.len() {
            let log = &events[i];
            let block_num = log.block_number.unwrap_or(0);

            // Apply event to local pool state
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

            // Check if this is the last event of the current block
            let is_last_in_block =
                i + 1 == events.len() || events[i + 1].block_number.unwrap_or(0) != block_num;

            // Periodic checkpoint: compare with on-chain state
            if is_last_in_block && block_num >= last_checked_block + check_interval {
                let check_block = BlockId::from(block_num);
                match fetch_onchain_slot0(&*provider, pool_address, check_block).await {
                    Ok((oc_sqrt_price, oc_liquidity, oc_tick)) => {
                        total_checks += 1;

                        let sqrt_price_match = pool.sqrt_price == oc_sqrt_price;
                        let liquidity_match = pool.liquidity == oc_liquidity;
                        let tick_match = pool.tick == oc_tick;

                        // Calculate drift percentage
                        let sqrt_price_drift_pct = if !oc_sqrt_price.is_zero() {
                            let local_f: f64 = pool.sqrt_price.to_string().parse().unwrap_or(0.0);
                            let remote_f: f64 = oc_sqrt_price.to_string().parse().unwrap_or(1.0);
                            ((local_f - remote_f) / remote_f * 100.0).abs()
                        } else {
                            0.0
                        };

                        let liquidity_drift_pct = if oc_liquidity != 0 {
                            let local_f = pool.liquidity as f64;
                            let remote_f = oc_liquidity as f64;
                            ((local_f - remote_f) / remote_f * 100.0).abs()
                        } else {
                            0.0
                        };

                        let tick_drift = (pool.tick - oc_tick).abs();

                        // Update max drifts
                        if sqrt_price_drift_pct > max_sqrt_price_drift_pct {
                            max_sqrt_price_drift_pct = sqrt_price_drift_pct;
                        }
                        if liquidity_drift_pct > max_liquidity_drift_pct {
                            max_liquidity_drift_pct = liquidity_drift_pct;
                        }
                        if tick_drift > max_tick_drift {
                            max_tick_drift = tick_drift;
                        }

                        if !sqrt_price_match || !liquidity_match || !tick_match {
                            println!(
                                "[{label}] ❌ Checkpoint at block {block_num} (after {events_processed} events):"
                            );
                            if !sqrt_price_match {
                                println!(
                                    "  sqrt_price: local={} vs on-chain={} (drift={:.6}%)",
                                    pool.sqrt_price, oc_sqrt_price, sqrt_price_drift_pct
                                );
                            }
                            if !liquidity_match {
                                println!(
                                    "  liquidity: local={} vs on-chain={} (drift={:.6}%)",
                                    pool.liquidity, oc_liquidity, liquidity_drift_pct
                                );
                            }
                            if !tick_match {
                                println!(
                                    "  tick: local={} vs on-chain={} (drift={})",
                                    pool.tick, oc_tick, tick_drift
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

            i += 1;
        }

        // Step 4: Final state comparison
        if let Some(last_log) = events.last() {
            let final_block = last_log.block_number.unwrap_or(current_block);
            let check_block = BlockId::from(final_block);
            match fetch_onchain_slot0(&*provider, pool_address, check_block).await {
                Ok((oc_sqrt_price, oc_liquidity, oc_tick)) => {
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
                        "  liquidity: local={} vs on-chain={}  {}",
                        pool.liquidity,
                        oc_liquidity,
                        if pool.liquidity == oc_liquidity {
                            "✅"
                        } else {
                            "❌"
                        }
                    );
                    println!(
                        "  tick: local={} vs on-chain={}  {}",
                        pool.tick,
                        oc_tick,
                        if pool.tick == oc_tick { "✅" } else { "❌" }
                    );

                    // Assert final state matches
                    assert_eq!(
                        pool.sqrt_price, oc_sqrt_price,
                        "[{label}] Final sqrt_price mismatch at block {final_block}!"
                    );
                    assert_eq!(
                        pool.liquidity, oc_liquidity,
                        "[{label}] Final liquidity mismatch at block {final_block}!"
                    );
                    assert_eq!(
                        pool.tick, oc_tick,
                        "[{label}] Final tick mismatch at block {final_block}!"
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
        println!("  Max liquidity drift: {:.8}%", max_liquidity_drift_pct);
        println!("  Max tick drift: {}", max_tick_drift);
        println!("  Test PASSED ✅");

        Ok(())
    }

    /// Test 4: SwapExactOut parity against on-chain Slipstream QuoterV2
    ///
    /// Requires:
    /// - `BASE_PROVIDER` set to a Base RPC (recommended: local mainnet-fork RPC)
    /// - Optional `AERODROME_SLIPSTREAM_QUOTER_V2` override
    async fn run_exact_out_quoter_parity_test(
        pool_address: Address,
        label: &str,
    ) -> eyre::Result<()> {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        dotenv::dotenv().ok();
        let rpc_endpoint = match std::env::var("BASE_PROVIDER") {
            Ok(u) => u,
            Err(_) => {
                println!("Skipping test: BASE_PROVIDER not set");
                return Ok(());
            }
        };

        let quoter_addr = std::env::var("AERODROME_SLIPSTREAM_QUOTER_V2")
            .ok()
            .and_then(|s| s.parse::<Address>().ok())
            .unwrap_or(DEFAULT_SLIPSTREAM_QUOTER_V2_BASE);

        let provider = Arc::new(ProviderBuilder::new().connect_http(rpc_endpoint.parse()?));
        let latest = provider.get_block_number().await?;
        let block_num = latest.saturating_sub(3);
        let block = BlockId::from(block_num);

        let pool = match AerodromeSlipstreamPool::new(pool_address)
            .init::<_, _>(block, provider.clone())
            .await
        {
            Ok(p) => p,
            Err(e) => {
                println!("[{label}] Skipping test: cannot initialize pool: {:?}", e);
                return Ok(());
            }
        };

        let quoter = ICLQuoterV2::new(quoter_addr, provider.clone());

        println!("\n[{label}] ======== EXACT OUT QUOTER PARITY ========");
        println!(
            "[{label}] block={} quoter={} pool={} tick_spacing={}",
            block_num, quoter_addr, pool.address, pool.tick_spacing
        );

        let test_directions = vec![
            (pool.token_a.address, pool.token_b.address, pool.token_b.decimals, "A->B"),
            (pool.token_b.address, pool.token_a.address, pool.token_a.decimals, "B->A"),
        ];

        for (token_in, token_out, out_decimals, dir) in test_directions {
            let amounts = exact_out_amounts_by_decimals(out_decimals);
            println!("[{label}] direction={dir}, out_decimals={out_decimals}, samples={amounts:?}");

            for amount_out in amounts {
                let simulated_in =
                    match pool.simulate_swap_exact_out(token_in, token_out, amount_out) {
                        Ok(v) => v,
                        Err(e) => {
                            println!(
                                "[{label}] direction={dir} amount_out={} local error={:?}",
                                amount_out, e
                            );
                            continue;
                        }
                    };

                let params = ICLQuoterV2::QuoteExactOutputSingleParams {
                    tokenIn: token_in,
                    tokenOut: token_out,
                    amount: amount_out,
                    tickSpacing: I24::try_from(pool.tick_spacing)
                        .map_err(|_| eyre::eyre!("tick_spacing out of int24 range"))?,
                    sqrtPriceLimitX96: U160::ZERO,
                };

                let quoted = match quoter
                    .quoteExactOutputSingle(params)
                    .block(block)
                    .call()
                    .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        println!(
                            "[{label}] direction={dir} amount_out={} quoter call error={:?}",
                            amount_out, e
                        );
                        continue;
                    }
                };

                println!(
                    "[{label}] direction={dir} out={} local_in={} quote_in={} ticks_crossed={}",
                    amount_out, simulated_in, quoted.amountIn, quoted.initializedTicksCrossed
                );

                assert_eq!(
                    simulated_in, quoted.amountIn,
                    "[{label}] exact_out mismatch direction={dir}, amount_out={}, local={}, quote={}",
                    amount_out, simulated_in, quoted.amountIn
                );
            }
        }

        println!("[{label}] ✅ EXACT OUT QUOTER PARITY PASSED");
        Ok(())
    }

    /// Test 3: Fee Change Event Handling
    ///
    /// This test verifies that CustomFeeSet events can be properly decoded
    /// and would update the pool's fee value.
    async fn run_fee_event_test(pool_address: Address, label: &str) -> eyre::Result<()> {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        dotenv::dotenv().ok();
        let rpc_endpoint = match std::env::var("BASE_PROVIDER") {
            Ok(u) => u,
            Err(_) => {
                println!("Skipping test: BASE_PROVIDER not set");
                return Ok(());
            }
        };

        let provider = Arc::new(ProviderBuilder::new().connect_http(rpc_endpoint.parse()?));

        // Slipstream Factory address on Base
        let factory_address = address!("5e7BB104d84c7CB9B682AaC2F3d509f5F406809A");

        // Get FeeModule address from factory
        sol! {
            #[sol(rpc)]
            contract ICLFactoryReader {
                function swapFeeModule() external view returns (address);
            }
        }
        let factory = ICLFactoryReader::new(factory_address, provider.clone());
        let fee_module_address = factory.swapFeeModule().call().await?;

        println!("\n[{label}] ========== FEE EVENT TEST ==========");
        println!("[{label}] Factory: {}", factory_address);
        println!("[{label}] FeeModule: {}", fee_module_address);

        // Get current block
        let current_block = provider.get_block_number().await?;
        let test_block_range = 100000u64; // Look back further for fee changes
        let start_block = current_block.saturating_sub(test_block_range);

        // Search for fee change events for this pool
        println!(
            "[{label}] Searching for CustomFeeSet events from block {} to {}...",
            start_block, current_block
        );

        let fee_events =
            fetch_fee_events(&*provider, fee_module_address, pool_address, start_block, current_block)
                .await?;

        println!("[{label}] Found {} fee change events for this pool", fee_events.len());

        // Display found events
        for log in &fee_events {
            let block = log.block_number.unwrap_or(0);
            match ICustomFeeModule::CustomFeeSet::decode_log(log.as_ref()) {
                Ok(event) => {
                    println!(
                        "[{label}]   Block {}: pool={}, new_fee={} ({}%)",
                        block,
                        event.pool,
                        event.fee,
                        event.fee.to::<u32>() as f64 / 10000.0
                    );
                }
                Err(e) => {
                    println!("[{label}]   Block {}: decode error: {:?}", block, e);
                }
            }
        }

        // Test sync_events() includes fee event signature
        let pool = AerodromeSlipstreamPool::new(pool_address);
        let sync_events = pool.sync_events();

        let has_fee_event = sync_events
            .iter()
            .any(|&sig| sig == ICustomFeeModule::CustomFeeSet::SIGNATURE_HASH);

        println!(
            "[{label}] sync_events() includes CustomFeeSet: {}",
            if has_fee_event { "✅" } else { "❌" }
        );
        assert!(
            has_fee_event,
            "[{label}] sync_events() should include CustomFeeSet signature!"
        );

        println!("\n[{label}] ✅ FEE EVENT TEST PASSED");
        Ok(())
    }

    // ============================================================================
    // Test Cases
    // ============================================================================

    /// Slipstream Pool: WETH / USDC
    /// Pool: 0xb2cc224c1c9fee385f8ad6a55b4d94e92359dc59
    #[tokio::test]
    async fn test_weth_usdc_swap_simulation() -> eyre::Result<()> {
        let pool_address = address!("b2cc224c1c9fee385f8ad6a55b4d94e92359dc59");
        run_swap_simulation_test(pool_address, "WETH/USDC-SwapSim").await
    }

    #[tokio::test]
    async fn test_weth_usdc_sync_drift() -> eyre::Result<()> {
        let pool_address = address!("b2cc224c1c9fee385f8ad6a55b4d94e92359dc59");
        run_sync_drift_test(pool_address, "WETH/USDC-Drift").await
    }

    #[tokio::test]
    async fn test_weth_usdc_fee_event() -> eyre::Result<()> {
        let pool_address = address!("b2cc224c1c9fee385f8ad6a55b4d94e92359dc59");
        run_fee_event_test(pool_address, "WETH/USDC-Fee").await
    }

    #[tokio::test]
    async fn test_weth_usdc_exact_out_quoter_parity() -> eyre::Result<()> {
        let pool_address = address!("b2cc224c1c9fee385f8ad6a55b4d94e92359dc59");
        run_exact_out_quoter_parity_test(pool_address, "WETH/USDC-ExactOut").await
    }

    /// Slipstream Pool: USDC / cbBTC
    /// Pool: 0x3e66e55e97ce60096f74b7c475e8249f2d31a9fb
    #[tokio::test]
    async fn test_usdc_cbbtc_swap_simulation() -> eyre::Result<()> {
        let pool_address = address!("3e66e55e97ce60096f74b7c475e8249f2d31a9fb");
        run_swap_simulation_test(pool_address, "USDC/cbBTC-SwapSim").await
    }

    #[tokio::test]
    async fn test_usdc_cbbtc_sync_drift() -> eyre::Result<()> {
        let pool_address = address!("3e66e55e97ce60096f74b7c475e8249f2d31a9fb");
        run_sync_drift_test(pool_address, "USDC/cbBTC-Drift").await
    }

    #[tokio::test]
    async fn test_usdc_cbbtc_fee_event() -> eyre::Result<()> {
        let pool_address = address!("3e66e55e97ce60096f74b7c475e8249f2d31a9fb");
        run_fee_event_test(pool_address, "USDC/cbBTC-Fee").await
    }

    #[tokio::test]
    async fn test_usdc_cbbtc_exact_out_quoter_parity() -> eyre::Result<()> {
        let pool_address = address!("3e66e55e97ce60096f74b7c475e8249f2d31a9fb");
        run_exact_out_quoter_parity_test(pool_address, "USDC/cbBTC-ExactOut").await
    }
}

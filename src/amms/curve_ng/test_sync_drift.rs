#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy::{
        eips::BlockId,
        primitives::{address, Address, B256, U256},
        providers::{Provider, ProviderBuilder},
        rpc::types::{Filter, Log},
    };

    use crate::amms::{
        amm::{AutomatedMarketMaker, SyncAction},
        curve_ng::{CurveNGPool, CurveNGPoolType, ICurveNGPool, ICurveNGStableSwap, ICurveTriCrypto, ICurveTwoCrypto},
    };

    #[derive(Debug, Clone)]
    struct DriftCase {
        label: &'static str,
        pool: Address,
        pool_type: CurveNGPoolType,
    }

    #[derive(Debug, Clone)]
    struct Snapshot {
        balances: Vec<U256>,
        fee: U256,
        admin_fee: U256,
        offpeg_fee_multiplier: U256,
        rates: Option<Vec<U256>>,
        price_scale: Option<Vec<U256>>,
        d: Option<U256>,
        mid_fee: Option<U256>,
        out_fee: Option<U256>,
        fee_gamma: Option<U256>,
    }

    fn drift_cases() -> Vec<DriftCase> {
        vec![
            // StableSwapNG (top-liquidity)
            DriftCase {
                label: "Stable-a632",
                pool: address!("a632d59b9b804a956bfaa9b48af3a1b74808fc1f"),
                pool_type: CurveNGPoolType::StableSwap,
            },
            DriftCase {
                label: "Stable-d001",
                pool: address!("d001ae433f254283fece51d4acce8c53263aa186"),
                pool_type: CurveNGPoolType::StableSwap,
            },
            DriftCase {
                label: "Stable-5dc1",
                pool: address!("5dc1bf6f1e983c0b21efb003c105133736fa0743"),
                pool_type: CurveNGPoolType::StableSwap,
            },
            // TwoCryptoNG (mainnet active pools)
            DriftCase {
                label: "Two-ca54",
                pool: address!("ca546ae6c3b2bb9fba2b6e5eeb0881097cece5b0"),
                pool_type: CurveNGPoolType::TwoCrypto,
            },
            DriftCase {
                label: "Two-7714",
                pool: address!("77146b0a1d08b6844376df6d9da99ba7f1b19e71"),
                pool_type: CurveNGPoolType::TwoCrypto,
            },
            DriftCase {
                label: "Two-660a",
                pool: address!("660a554fc97fabecff47d200367ca1a8bf49c82b"),
                pool_type: CurveNGPoolType::TwoCrypto,
            },
            // TriCryptoNG (top-liquidity)
            DriftCase {
                label: "Tri-f5f5",
                pool: address!("f5f5b97624542d72a9e06f04804bf81baa15e2b4"),
                pool_type: CurveNGPoolType::TriCrypto,
            },
            DriftCase {
                label: "Tri-7f86",
                pool: address!("7f86bf177dd4f3494b841a37e810a34dd56c829b"),
                pool_type: CurveNGPoolType::TriCrypto,
            },
            DriftCase {
                label: "Tri-4ebd",
                pool: address!("4ebdf703948ddcea3b11f675b4d1fba9d2414a14"),
                pool_type: CurveNGPoolType::TriCrypto,
            },
        ]
    }

    async fn fetch_pool_events<P: Provider + Clone>(
        provider: &P,
        pool_address: Address,
        event_sigs: &[B256],
        from_block: u64,
        to_block: u64,
    ) -> eyre::Result<Vec<Log>> {
        let mut logs = Vec::new();
        let mut from = from_block;
        while from <= to_block {
            let to = (from + 999).min(to_block);
            let filter = Filter::new()
                .address(pool_address)
                .event_signature(event_sigs.to_vec())
                .from_block(from)
                .to_block(to);
            let mut chunk = provider.get_logs(&filter).await?;
            logs.append(&mut chunk);
            if to == to_block {
                break;
            }
            from = to + 1;
        }
        Ok(logs)
    }

    async fn fetch_onchain_snapshot<P: Provider + Clone>(
        provider: &P,
        pool_address: Address,
        pool_type: CurveNGPoolType,
        n_coins: usize,
        block: BlockId,
    ) -> eyre::Result<Snapshot> {
        let pool = ICurveNGPool::new(pool_address, provider.clone());

        let mut balances = Vec::with_capacity(n_coins);
        for i in 0..n_coins {
            balances.push(pool.balances(U256::from(i)).block(block).call().await?);
        }

        let mut snapshot = Snapshot {
            balances,
            fee: U256::ZERO,
            admin_fee: U256::ZERO,
            offpeg_fee_multiplier: U256::ZERO,
            rates: None,
            price_scale: None,
            d: None,
            mid_fee: None,
            out_fee: None,
            fee_gamma: None,
        };

        match pool_type {
            CurveNGPoolType::StableSwap => {
                snapshot.fee = pool.fee().block(block).call().await?;
                snapshot.admin_fee = pool.admin_fee().block(block).call().await?;
                snapshot.offpeg_fee_multiplier =
                    pool.offpeg_fee_multiplier().block(block).call().await?;
                let stable = ICurveNGStableSwap::new(pool_address, provider.clone());
                snapshot.rates = Some(stable.stored_rates().block(block).call().await?);
            }
            CurveNGPoolType::TwoCrypto => {
                let two = ICurveTwoCrypto::new(pool_address, provider.clone());
                snapshot.price_scale = Some(vec![two.price_scale().block(block).call().await?]);
                snapshot.d = Some(two.D().block(block).call().await?);
                snapshot.mid_fee = Some(pool.mid_fee().block(block).call().await?);
                snapshot.out_fee = Some(pool.out_fee().block(block).call().await?);
                snapshot.fee_gamma = Some(pool.fee_gamma().block(block).call().await?);
            }
            CurveNGPoolType::TriCrypto => {
                let tri = ICurveTriCrypto::new(pool_address, provider.clone());
                let mut scales = Vec::with_capacity(n_coins.saturating_sub(1));
                for i in 0..n_coins.saturating_sub(1) {
                    scales.push(tri.price_scale(U256::from(i)).block(block).call().await?);
                }
                snapshot.price_scale = Some(scales);
                snapshot.d = Some(tri.D().block(block).call().await?);
                snapshot.mid_fee = Some(pool.mid_fee().block(block).call().await?);
                snapshot.out_fee = Some(pool.out_fee().block(block).call().await?);
                snapshot.fee_gamma = Some(pool.fee_gamma().block(block).call().await?);
            }
        }

        Ok(snapshot)
    }

    fn local_snapshot(pool: &CurveNGPool) -> Snapshot {
        Snapshot {
            balances: pool.balances.clone(),
            fee: pool.fee,
            admin_fee: pool.admin_fee,
            offpeg_fee_multiplier: pool.offpeg_fee_multiplier,
            rates: if pool.pool_type == CurveNGPoolType::StableSwap {
                Some(pool.rates.clone())
            } else {
                None
            },
            price_scale: pool.price_scale.clone(),
            d: pool.d,
            mid_fee: pool.mid_fee,
            out_fee: pool.out_fee,
            fee_gamma: pool.fee_gamma,
        }
    }

    fn compare_snapshots(
        label: &str,
        block: u64,
        pool_type: CurveNGPoolType,
        local: &Snapshot,
        chain: &Snapshot,
    ) -> bool {
        let mut ok = true;
        if local.balances.len() != chain.balances.len() {
            ok = false;
            println!(
                "[{label}] DIFF block={block} balance_len local={} chain={}",
                local.balances.len(),
                chain.balances.len()
            );
        } else {
            for (i, (lv, cv)) in local.balances.iter().zip(chain.balances.iter()).enumerate() {
                let diff = if lv > cv { *lv - *cv } else { *cv - *lv };
                // 10 ppm tolerance on balances to absorb tiny non-event accounting drifts.
                let tol = std::cmp::max(*cv / U256::from(100_000u64), U256::from(1u8));
                if diff > tol {
                    ok = false;
                    println!(
                        "[{label}] DIFF block={block} balance[{i}] local={} chain={} diff={} tol={}",
                        lv, cv, diff, tol
                    );
                }
            }
        }
        match pool_type {
            CurveNGPoolType::StableSwap => {
                if local.fee != chain.fee {
                    ok = false;
                    println!(
                        "[{label}] DIFF block={block} fee local={} chain={}",
                        local.fee, chain.fee
                    );
                }
                if local.admin_fee != chain.admin_fee {
                    ok = false;
                    println!(
                        "[{label}] DIFF block={block} admin_fee local={} chain={}",
                        local.admin_fee, chain.admin_fee
                    );
                }
                if local.offpeg_fee_multiplier != chain.offpeg_fee_multiplier {
                    ok = false;
                    println!(
                        "[{label}] DIFF block={block} offpeg local={} chain={}",
                        local.offpeg_fee_multiplier, chain.offpeg_fee_multiplier
                    );
                }
                if local.rates != chain.rates {
                    ok = false;
                    println!(
                        "[{label}] DIFF block={block} rates local={:?} chain={:?}",
                        local.rates, chain.rates
                    );
                }
            }
            CurveNGPoolType::TwoCrypto | CurveNGPoolType::TriCrypto => {
                if local.price_scale != chain.price_scale {
                    ok = false;
                    println!(
                        "[{label}] DIFF block={block} price_scale local={:?} chain={:?}",
                        local.price_scale, chain.price_scale
                    );
                }
                if local.d != chain.d {
                    ok = false;
                    println!(
                        "[{label}] DIFF block={block} D local={:?} chain={:?}",
                        local.d, chain.d
                    );
                }
                if local.mid_fee != chain.mid_fee {
                    ok = false;
                    println!(
                        "[{label}] DIFF block={block} mid_fee local={:?} chain={:?}",
                        local.mid_fee, chain.mid_fee
                    );
                }
                if local.out_fee != chain.out_fee {
                    ok = false;
                    println!(
                        "[{label}] DIFF block={block} out_fee local={:?} chain={:?}",
                        local.out_fee, chain.out_fee
                    );
                }
                if local.fee_gamma != chain.fee_gamma {
                    ok = false;
                    println!(
                        "[{label}] DIFF block={block} fee_gamma local={:?} chain={:?}",
                        local.fee_gamma, chain.fee_gamma
                    );
                }
            }
        }
        ok
    }

    async fn run_sync_drift_test(
        provider: Arc<impl Provider + Clone>,
        case: &DriftCase,
        block_range: u64,
        check_interval: u64,
        periodic_reinit_interval: u64,
    ) -> eyre::Result<()> {
        let current_block = provider.get_block_number().await?;
        let start_block = current_block.saturating_sub(block_range);

        println!(
            "[{}] start={} current={} range={}",
            case.label, start_block, current_block, block_range
        );

        let mut pool = CurveNGPool::new(case.pool, case.pool_type)
            .init(BlockId::from(start_block), provider.clone())
            .await?;

        let init_chain = fetch_onchain_snapshot(
            &*provider,
            case.pool,
            case.pool_type,
            pool.n_coins as usize,
            BlockId::from(start_block),
        )
        .await?;
        let init_local = local_snapshot(&pool);
        assert!(
            compare_snapshots(case.label, start_block, case.pool_type, &init_local, &init_chain),
            "[{}] initial snapshot mismatch at block {}",
            case.label,
            start_block
        );

        let event_sigs = pool.sync_events();
        let mut events = fetch_pool_events(
            &*provider,
            case.pool,
            &event_sigs,
            start_block + 1,
            current_block,
        )
        .await?;

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
            println!("[{}] no events in range, reinit at current for final check", case.label);
            pool = CurveNGPool::new(case.pool, case.pool_type)
                .init(BlockId::from(current_block), provider.clone())
                .await?;
            let local = local_snapshot(&pool);
            let chain = fetch_onchain_snapshot(
                &*provider,
                case.pool,
                case.pool_type,
                pool.n_coins as usize,
                BlockId::from(current_block),
            )
            .await?;
            assert!(
                compare_snapshots(case.label, current_block, case.pool_type, &local, &chain),
                "[{}] final snapshot mismatch at block {}",
                case.label,
                current_block
            );
            return Ok(());
        }

        let mut last_checked_block = start_block;
        let mut last_reinit_block = start_block;
        let mut events_processed = 0u64;
        let mut block_needs_reinit = false;

        for (idx, log) in events.iter().enumerate() {
            let block_num = log.block_number.unwrap_or(0);
            match pool.sync(log) {
                Ok(SyncAction::None) => {}
                Ok(SyncAction::AsyncUpdate) | Ok(SyncAction::Resync) => {
                    // For historical replay, update() is not block-pinned.
                    // We re-init at the log block to maintain same-block correctness.
                    block_needs_reinit = true;
                }
                Err(e) => {
                    println!("[{}] sync error block {}: {:?}", case.label, block_num, e);
                    continue;
                }
            }

            events_processed += 1;

            let is_last_in_block = if let Some(next_log) = events.get(idx + 1) {
                next_log.block_number.unwrap_or(0) > block_num
            } else {
                true
            };

            if is_last_in_block {
                if block_needs_reinit
                    || block_num >= last_reinit_block.saturating_add(periodic_reinit_interval)
                {
                    pool = CurveNGPool::new(case.pool, case.pool_type)
                        .init(BlockId::from(block_num), provider.clone())
                        .await?;
                    last_reinit_block = block_num;
                    block_needs_reinit = false;
                }

                if block_num >= last_checked_block.saturating_add(check_interval) {
                    let local = local_snapshot(&pool);
                    let chain = fetch_onchain_snapshot(
                        &*provider,
                        case.pool,
                        case.pool_type,
                        pool.n_coins as usize,
                        BlockId::from(block_num),
                    )
                    .await?;
                    assert!(
                        compare_snapshots(case.label, block_num, case.pool_type, &local, &chain),
                        "[{}] checkpoint mismatch at block {} after {} events",
                        case.label,
                        block_num,
                        events_processed
                    );
                    println!(
                        "[{}] checkpoint OK block={} events={}",
                        case.label, block_num, events_processed
                    );
                    last_checked_block = block_num;
                }
            }
        }

        let final_block = events
            .last()
            .and_then(|l| l.block_number)
            .unwrap_or(current_block);
        let local = local_snapshot(&pool);
        let chain = fetch_onchain_snapshot(
            &*provider,
            case.pool,
            case.pool_type,
            pool.n_coins as usize,
            BlockId::from(final_block),
        )
        .await?;
        assert!(
            compare_snapshots(case.label, final_block, case.pool_type, &local, &chain),
            "[{}] final snapshot mismatch at block {}",
            case.label,
            final_block
        );

        println!(
            "[{}] PASSED events_processed={} final_block={}",
            case.label, events_processed, final_block
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_curve_ng_sync_drift_matrix() -> eyre::Result<()> {
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

        let block_range = std::env::var("CURVE_NG_DRIFT_BLOCK_RANGE")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(1200);
        let check_interval = std::env::var("CURVE_NG_DRIFT_CHECK_INTERVAL")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(50);
        let periodic_reinit_interval = std::env::var("CURVE_NG_DRIFT_REINIT_INTERVAL")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(120);

        let provider = Arc::new(ProviderBuilder::new().connect_http(rpc_endpoint.parse().unwrap()));

        for case in drift_cases() {
            run_sync_drift_test(
                provider.clone(),
                &case,
                block_range,
                check_interval,
                periodic_reinit_interval,
            )
            .await?;
        }

        Ok(())
    }
}

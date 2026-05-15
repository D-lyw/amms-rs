use std::collections::HashMap;
use std::sync::Arc;

use alloy::{
    eips::BlockId,
    primitives::{aliases::U160, Address, U256},
    providers::{Provider, ProviderBuilder},
    sol,
};

use alloy::network::Ethereum;
use amms::amms::{
    algebra_integral::{AlgebraIntegralFactory, AlgebraIntegralPool},
    amm::{AutomatedMarketMaker, AMM},
};

use super::support::{
    algebra_cases, bps_diff, exact_in_amounts_by_decimals, exact_out_amounts_by_decimals,
    provider_url_for_base, ALGEBRA_COMPARE_BLOCK, HYDREX_BASE_QUOTER_V2, HYDREX_WETH_CBBTC_POOL,
    QUICKSWAP_BASE_QUOTER_V2, QUICKSWAP_V4_WETH_USDC_POOL,
};

sol! {
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IAlgebraQuoterV2 {
        struct QuoteExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            address deployer;
            uint256 amountIn;
            uint160 limitSqrtPrice;
        }

        struct QuoteExactOutputSingleParams {
            address tokenIn;
            address tokenOut;
            address deployer;
            uint256 amount;
            uint160 limitSqrtPrice;
        }

        function quoteExactInputSingle(QuoteExactInputSingleParams memory params)
            external
            returns (
                uint256 amountOut,
                uint256 amountIn,
                uint160 sqrtPriceX96After,
                uint32 initializedTicksCrossed,
                uint256 gasEstimate,
                uint16 fee
            );

        function quoteExactOutputSingle(QuoteExactOutputSingleParams memory params)
            external
            returns (
                uint256 amountOut,
                uint256 amountIn,
                uint160 sqrtPriceX96After,
                uint32 initializedTicksCrossed,
                uint256 gasEstimate,
                uint16 fee
            );
    }
}

// Minimal view-interface to the Algebra plugin contract.
sol! {
    #[sol(rpc)]
    interface IAlgebraFeeView {
        function getCurrentFee() external view returns (uint16 fee);
        function timepointIndex() external view returns (uint16);
        function lastTimepointTimestamp() external view returns (uint32);
    }
}

#[derive(Default, Debug)]
struct CompareStats {
    exact_in_samples: usize,
    exact_out_samples: usize,
    exact_in_max_drift_bps: U256,
    exact_out_max_drift_bps: U256,
    large_tick_samples: usize,
    max_ticks_crossed: u32,
}

fn allowed_drift_bps(ticks_crossed: u32) -> U256 {
    if ticks_crossed >= 20 {
        U256::from(350u64)
    } else {
        U256::from(150u64)
    }
}

fn abs_diff(lhs: U256, rhs: U256) -> U256 {
    if lhs >= rhs {
        lhs - rhs
    } else {
        rhs - lhs
    }
}

fn simulate_exact_in_with_fee(
    pool: &AlgebraIntegralPool,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    fee: u32,
) -> eyre::Result<U256> {
    let mut local = pool.clone();
    local.inner.fee = fee;
    local.last_fee = fee;
    local
        .simulate_swap(token_in, token_out, amount_in)
        .map_err(|e| eyre::eyre!(e.to_string()))
}

fn infer_effective_fee_exact_in(
    pool: &AlgebraIntegralPool,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    target_out: U256,
    low_fee: u32,
    high_fee: u32,
) -> eyre::Result<(u32, U256)> {
    let out_low = simulate_exact_in_with_fee(pool, token_in, token_out, amount_in, low_fee)?;
    let out_high = simulate_exact_in_with_fee(pool, token_in, token_out, amount_in, high_fee)?;

    if out_low <= out_high {
        return Err(eyre::eyre!(
            "unexpected non-monotonic output over fee range: out_low={} out_high={}",
            out_low,
            out_high
        ));
    }

    let mut lo = low_fee;
    let mut hi = high_fee;

    while hi.saturating_sub(lo) > 1 {
        let mid = lo + (hi - lo) / 2;
        let out_mid = simulate_exact_in_with_fee(pool, token_in, token_out, amount_in, mid)?;
        if out_mid > target_out {
            lo = mid;
        } else {
            hi = mid;
        }
    }

    let mut best_fee = low_fee;
    let mut best_out = out_low;
    let mut best_diff = abs_diff(out_low, target_out);

    for fee in [low_fee, lo, hi, high_fee] {
        let out = simulate_exact_in_with_fee(pool, token_in, token_out, amount_in, fee)?;
        let diff = abs_diff(out, target_out);
        if diff < best_diff {
            best_diff = diff;
            best_fee = fee;
            best_out = out;
        }
    }

    Ok((best_fee, best_out))
}

async fn compare_one_direction(
    label: &str,
    pool: &AlgebraIntegralPool,
    provider: Arc<impl alloy::providers::Provider<alloy::network::Ethereum> + Clone>,
    quoter_addr: Address,
    block: BlockId,
    token_in: Address,
    token_out: Address,
    deployer: Address,
    exact_in_amounts: &[U256],
    exact_out_amounts: &[U256],
    large_tick_threshold: u32,
) -> eyre::Result<CompareStats> {
    let quoter = IAlgebraQuoterV2::new(quoter_addr, provider);
    let mut stats = CompareStats::default();

    for amount_in in exact_in_amounts {
        let params = IAlgebraQuoterV2::QuoteExactInputSingleParams {
            tokenIn: token_in,
            tokenOut: token_out,
            deployer,
            amountIn: *amount_in,
            limitSqrtPrice: U160::ZERO,
        };

        let quoted = match quoter
            .quoteExactInputSingle(params)
            .block(block)
            .call()
            .await
        {
            Ok(v) => v,
            Err(_) => continue,
        };

        let local_out = pool.simulate_swap(token_in, token_out, *amount_in)?;
        let drift_bps = bps_diff(local_out, quoted.amountOut);

        if drift_bps > stats.exact_in_max_drift_bps {
            stats.exact_in_max_drift_bps = drift_bps;
        }
        if quoted.initializedTicksCrossed > stats.max_ticks_crossed {
            stats.max_ticks_crossed = quoted.initializedTicksCrossed;
        }
        if quoted.initializedTicksCrossed >= large_tick_threshold {
            stats.large_tick_samples += 1;
        }

        println!(
            "[{}] exact_in {}->{} amount_in={} local_out={} onchain_out={} ticks={} drift_bps={}",
            label,
            token_in,
            token_out,
            amount_in,
            local_out,
            quoted.amountOut,
            quoted.initializedTicksCrossed,
            drift_bps,
        );

        stats.exact_in_samples += 1;

        let max_allowed = allowed_drift_bps(quoted.initializedTicksCrossed);
        assert!(
            drift_bps <= max_allowed,
            "[{}] exact_in drift too high token_in={} token_out={} amount_in={} local_out={} onchain_out={} ticks={} drift_bps={} allowed={}",
            label,
            token_in,
            token_out,
            amount_in,
            local_out,
            quoted.amountOut,
            quoted.initializedTicksCrossed,
            drift_bps,
            max_allowed
        );
    }

    for amount_out in exact_out_amounts {
        let params = IAlgebraQuoterV2::QuoteExactOutputSingleParams {
            tokenIn: token_in,
            tokenOut: token_out,
            deployer,
            amount: *amount_out,
            limitSqrtPrice: U160::ZERO,
        };

        let quoted = match quoter
            .quoteExactOutputSingle(params)
            .block(block)
            .call()
            .await
        {
            Ok(v) => v,
            Err(_) => continue,
        };

        let local_in = pool.simulate_swap_exact_out(token_in, token_out, *amount_out)?;
        let drift_bps = bps_diff(local_in, quoted.amountIn);

        if drift_bps > stats.exact_out_max_drift_bps {
            stats.exact_out_max_drift_bps = drift_bps;
        }
        if quoted.initializedTicksCrossed > stats.max_ticks_crossed {
            stats.max_ticks_crossed = quoted.initializedTicksCrossed;
        }
        if quoted.initializedTicksCrossed >= large_tick_threshold {
            stats.large_tick_samples += 1;
        }

        println!(
            "[{}] exact_out {}->{} amount_out={} local_in={} onchain_in={} ticks={} drift_bps={}",
            label,
            token_in,
            token_out,
            amount_out,
            local_in,
            quoted.amountIn,
            quoted.initializedTicksCrossed,
            drift_bps,
        );

        stats.exact_out_samples += 1;

        let max_allowed = allowed_drift_bps(quoted.initializedTicksCrossed);
        assert!(
            drift_bps <= max_allowed,
            "[{}] exact_out drift too high token_in={} token_out={} amount_out={} local_in={} onchain_in={} ticks={} drift_bps={} allowed={}",
            label,
            token_in,
            token_out,
            amount_out,
            local_in,
            quoted.amountIn,
            quoted.initializedTicksCrossed,
            drift_bps,
            max_allowed
        );
    }

    Ok(stats)
}

#[tokio::test]
async fn test_root_cause_fee_variation_vs_init_data() -> eyre::Result<()> {
    let rpc = match provider_url_for_base() {
        Some(v) => v,
        None => {
            eprintln!("skip algebra root cause test: BASE_PROVIDER/ETHEREUM_PROVIDER not set");
            return Ok(());
        }
    };

    let provider = Arc::new(ProviderBuilder::new().connect_http(rpc.parse()?));
    println!("[root-cause-test] rpc={}", rpc);
    let block = BlockId::from(ALGEBRA_COMPARE_BLOCK);

    let root_cases = vec![
        (
            "QuickSwap V4 WETH-USDC (control)",
            QUICKSWAP_V4_WETH_USDC_POOL,
            QUICKSWAP_BASE_QUOTER_V2,
        ),
        (
            "Hydrex WETH-cbBTC (problematic)",
            HYDREX_WETH_CBBTC_POOL,
            HYDREX_BASE_QUOTER_V2,
        ),
    ];

    let mut hydrex_implied_fees = Vec::<u32>::new();
    let mut hydrex_implied_improves = false;

    for (label, pool_addr, quoter_addr) in root_cases {
        let mut pool_opt = None;
        let mut init_err = None;
        for _ in 0..4 {
            match AlgebraIntegralPool::new(pool_addr)
                .init::<alloy::network::Ethereum, _>(block, provider.clone())
                .await
            {
                Ok(p) => {
                    pool_opt = Some(p);
                    break;
                }
                Err(e) => {
                    init_err = Some(e.to_string());
                    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
                }
            }
        }
        let pool = match pool_opt {
            Some(v) => v,
            None => {
                return Err(eyre::eyre!(
                    "[{}] failed to init pool after retries: {:?}",
                    label,
                    init_err
                ))
            }
        };
        let quoter = IAlgebraQuoterV2::new(quoter_addr, provider.clone());

        println!(
            "[{}] block={} base_local_fee={} plugin={} plugin_config={} fee_mode={:?}",
            label,
            ALGEBRA_COMPARE_BLOCK,
            pool.inner.fee,
            pool.plugin,
            pool.plugin_config,
            pool.fee_mode
        );

        let mut quoted_fees = Vec::<u32>::new();

        let directions = vec![
            (
                pool.inner.token_a.address,
                pool.inner.token_b.address,
                pool.inner.token_a.decimals,
            ),
            (
                pool.inner.token_b.address,
                pool.inner.token_a.address,
                pool.inner.token_b.decimals,
            ),
        ];

        for (token_in, token_out, in_decimals) in directions {
            let one = U256::from(10u8).pow(U256::from(in_decimals));
            let test_amounts = [
                one * U256::from(20u64),
                one * U256::from(100u64),
                one * U256::from(500u64),
            ];
            println!("[{}] direction {} -> {}", label, token_in, token_out);

            for amount_in in test_amounts {
                let params = IAlgebraQuoterV2::QuoteExactInputSingleParams {
                    tokenIn: token_in,
                    tokenOut: token_out,
                    deployer: Address::ZERO,
                    amountIn: amount_in,
                    limitSqrtPrice: U160::ZERO,
                };
                let mut quoted_opt = None;
                let mut quote_err = None;
                for _ in 0..4 {
                    match quoter
                        .quoteExactInputSingle(params.clone())
                        .block(block)
                        .call()
                        .await
                    {
                        Ok(v) => {
                            quoted_opt = Some(v);
                            break;
                        }
                        Err(e) => {
                            quote_err = Some(e.to_string());
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                    }
                }
                let quoted = match quoted_opt {
                    Some(v) => v,
                    None => {
                        return Err(eyre::eyre!(
                            "[{}] quote failed after retries, amount_in={}: {:?}",
                            label,
                            amount_in,
                            quote_err
                        ))
                    }
                };

                let local_base = pool
                    .simulate_swap(token_in, token_out, amount_in)
                    .map_err(|e| eyre::eyre!(e.to_string()))?;
                let local_quote_fee = simulate_exact_in_with_fee(
                    &pool,
                    token_in,
                    token_out,
                    amount_in,
                    u32::from(quoted.fee),
                )?;
                let (implied_fee, implied_out) = infer_effective_fee_exact_in(
                    &pool,
                    token_in,
                    token_out,
                    amount_in,
                    quoted.amountOut,
                    1,
                    100_000,
                )?;
                let drift_base = bps_diff(local_base, quoted.amountOut);
                let drift_quote_fee = bps_diff(local_quote_fee, quoted.amountOut);
                let drift_implied = bps_diff(implied_out, quoted.amountOut);

                quoted_fees.push(u32::from(quoted.fee));
                if label.contains("Hydrex")
                    && token_in == pool.inner.token_a.address
                    && token_out == pool.inner.token_b.address
                {
                    hydrex_implied_fees.push(implied_fee);
                    if drift_implied < drift_base {
                        hydrex_implied_improves = true;
                    }
                }

                println!(
                    "[{}] amount_in={} quoted_out={} quoted_fee={} ticks={} | local_base_out={} drift_base_bps={} | local_quote_fee_out={} drift_quote_fee_bps={} | implied_fee={} implied_out={} drift_implied_bps={}",
                    label,
                    amount_in,
                    quoted.amountOut,
                    quoted.fee,
                    quoted.initializedTicksCrossed,
                    local_base,
                    drift_base,
                    local_quote_fee,
                    drift_quote_fee,
                    implied_fee,
                    implied_out,
                    drift_implied
                );
            }
        }

        let min_fee = quoted_fees.iter().min().copied().unwrap_or(0);
        let max_fee = quoted_fees.iter().max().copied().unwrap_or(0);
        let fee_span = max_fee.saturating_sub(min_fee);

        println!(
            "[{}] quoted_fee_range=[{}, {}], span={}",
            label, min_fee, max_fee, fee_span
        );
    }

    let hydrex_implied_min = hydrex_implied_fees.iter().min().copied().unwrap_or(0);
    let hydrex_implied_max = hydrex_implied_fees.iter().max().copied().unwrap_or(0);
    let hydrex_implied_span = hydrex_implied_max.saturating_sub(hydrex_implied_min);
    println!(
        "[Hydrex implied fee] range=[{}, {}], span={}",
        hydrex_implied_min, hydrex_implied_max, hydrex_implied_span
    );

    assert!(
        !hydrex_implied_fees.is_empty(),
        "Hydrex implied fee samples are empty"
    );
    assert!(
        hydrex_implied_span <= 5,
        "Hydrex implied fee changed too much for the sampled swaps: span={hydrex_implied_span}"
    );
    println!(
        "[Hydrex implied fee] drift_improved_vs_base={}",
        hydrex_implied_improves
    );

    Ok(())
}

#[tokio::test]
async fn test_swap_compare_exact_in_exact_out_and_large_cross_tick() -> eyre::Result<()> {
    let rpc = match provider_url_for_base() {
        Some(v) => v,
        None => {
            eprintln!("skip algebra swap compare: BASE_PROVIDER/ETHEREUM_PROVIDER not set");
            return Ok(());
        }
    };

    let provider = Arc::new(ProviderBuilder::new().connect_http(rpc.parse()?));
    let block = BlockId::from(ALGEBRA_COMPARE_BLOCK);

    let mut global_exact_in_samples = 0usize;
    let mut global_exact_out_samples = 0usize;
    let mut global_large_tick_samples = 0usize;
    let mut global_max_ticks_crossed = 0u32;

    for case in algebra_cases() {
        let mut case_done = None;
        let mut last_err = None;
        for _ in 0..3 {
            let pool = match AlgebraIntegralPool::new(case.pool)
                .init::<alloy::network::Ethereum, _>(block, provider.clone())
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    last_err = Some(eyre::eyre!(e.to_string()));
                    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
                    continue;
                }
            };

            let token0 = pool.inner.token_a.address;
            let token1 = pool.inner.token_b.address;

            let exact_in_0_to_1 = exact_in_amounts_by_decimals(pool.inner.token_a.decimals);
            let exact_out_0_to_1 = exact_out_amounts_by_decimals(pool.inner.token_b.decimals);

            let exact_in_1_to_0 = exact_in_amounts_by_decimals(pool.inner.token_b.decimals);
            let exact_out_1_to_0 = exact_out_amounts_by_decimals(pool.inner.token_a.decimals);

            let stats_0_to_1 = match compare_one_direction(
                case.label,
                &pool,
                provider.clone(),
                case.quoter,
                block,
                token0,
                token1,
                case.deployer,
                &exact_in_0_to_1,
                &exact_out_0_to_1,
                10,
            )
            .await
            {
                Ok(v) => v,
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
                    continue;
                }
            };

            let stats_1_to_0 = match compare_one_direction(
                case.label,
                &pool,
                provider.clone(),
                case.quoter,
                block,
                token1,
                token0,
                case.deployer,
                &exact_in_1_to_0,
                &exact_out_1_to_0,
                10,
            )
            .await
            {
                Ok(v) => v,
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
                    continue;
                }
            };
            case_done = Some((stats_0_to_1, stats_1_to_0));
            break;
        }

        let (stats_0_to_1, stats_1_to_0) = match case_done {
            Some(v) => v,
            None => {
                return Err(eyre::eyre!(
                    "[{}] compare failed after retries: {:?}",
                    case.label,
                    last_err
                ))
            }
        };

        let exact_in_samples = stats_0_to_1.exact_in_samples + stats_1_to_0.exact_in_samples;
        let exact_out_samples = stats_0_to_1.exact_out_samples + stats_1_to_0.exact_out_samples;

        println!(
            "[{}] summary exact_in_samples={} exact_out_samples={} max_ticks_crossed={} large_tick_samples={} exact_in_max_drift_bps={} exact_out_max_drift_bps={}",
            case.label,
            exact_in_samples,
            exact_out_samples,
            stats_0_to_1.max_ticks_crossed.max(stats_1_to_0.max_ticks_crossed),
            stats_0_to_1.large_tick_samples + stats_1_to_0.large_tick_samples,
            stats_0_to_1
                .exact_in_max_drift_bps
                .max(stats_1_to_0.exact_in_max_drift_bps),
            stats_0_to_1
                .exact_out_max_drift_bps
                .max(stats_1_to_0.exact_out_max_drift_bps),
        );

        assert!(
            exact_in_samples >= 4,
            "[{}] too few exact_in samples: {}",
            case.label,
            exact_in_samples
        );
        assert!(
            exact_out_samples >= 4,
            "[{}] too few exact_out samples: {}",
            case.label,
            exact_out_samples
        );

        global_exact_in_samples += exact_in_samples;
        global_exact_out_samples += exact_out_samples;

        let case_large_tick_samples =
            stats_0_to_1.large_tick_samples + stats_1_to_0.large_tick_samples;
        global_large_tick_samples += case_large_tick_samples;

        let case_max_ticks_crossed = stats_0_to_1
            .max_ticks_crossed
            .max(stats_1_to_0.max_ticks_crossed);
        if case_max_ticks_crossed > global_max_ticks_crossed {
            global_max_ticks_crossed = case_max_ticks_crossed;
        }
    }

    println!(
        "[global] exact_in_samples={} exact_out_samples={} large_tick_samples={} max_ticks_crossed={}",
        global_exact_in_samples,
        global_exact_out_samples,
        global_large_tick_samples,
        global_max_ticks_crossed,
    );

    assert!(
        global_exact_in_samples >= 30,
        "too few global exact_in samples"
    );
    assert!(
        global_exact_out_samples >= 30,
        "too few global exact_out samples"
    );
    assert!(
        global_large_tick_samples > 0 || global_max_ticks_crossed >= 10,
        "no large cross-tick sample captured"
    );

    Ok(())
}

/// Verify that the local compute_fee() matches the chain's plugin.getCurrentFee().
#[tokio::test]
async fn test_compute_fee_matches_chain() -> eyre::Result<()> {
    let rpc = match provider_url_for_base() {
        Some(v) => v,
        None => {
            eprintln!("skip compute_fee test: BASE_PROVIDER not set");
            return Ok(());
        }
    };

    let provider = Arc::new(ProviderBuilder::new().connect_http(rpc.parse()?));
    let block = BlockId::from(ALGEBRA_COMPARE_BLOCK);

    // Batch-init pools in small chunks to avoid batch-contract gas limits.
    let cases = algebra_cases();
    let batch: Vec<AMM> = cases.iter().map(|c| AMM::AlgebraIntegralPool(AlgebraIntegralPool::new(c.pool))).collect();
    const BATCH_CHUNK: usize = 5;
    let mut pools: HashMap<Address, AlgebraIntegralPool> = HashMap::new();
    for chunk in batch.chunks(BATCH_CHUNK) {
        let chunk_vec = chunk.to_vec();
        for attempt in 0..3 {
            match AlgebraIntegralFactory::init_batch::<Ethereum, _>(chunk_vec.clone(), block, provider.clone()).await {
                Ok(initialized) => {
                    for amm in initialized {
                        if let AMM::AlgebraIntegralPool(p) = amm {
                            pools.insert(p.inner.address, p);
                        }
                    }
                    break;
                }
                Err(e) => {
                    if attempt == 2 {
                        return Err(eyre::eyre!("batch init chunk failed after retries: {:?}", e));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                }
            }
        }
    }

    for case in &cases {
        let Some(pool) = pools.get(&case.pool) else {
            eprintln!("[{}] skip: not in batch init results", case.label);
            continue;
        };

        println!(
            "[{}] pool={} plugin={} fee_mode={:?} fee_config={} timepoints={}",
            case.label,
            case.pool,
            pool.plugin,
            pool.fee_mode,
            pool.fee_config.is_some(),
            pool.timepoints.is_some(),
        );

        if pool.plugin.is_zero() {
            eprintln!("[{}] skip: no plugin", case.label);
            continue;
        }

        // Debug: check what's in the timepoint cache.
        if let Some(ref tp) = pool.timepoints {
            let last = tp.last();
            let oi = tp.oldest_index();
            let cc = tp.cardinality;
            println!(
                "[{}] timepoints: has_last={} oldest_idx={:?} count={}",
                case.label,
                last.is_some(),
                oi,
                cc,
            );
            if let Some(ref l) = last {
                println!(
                    "[{}]   last_tp: ts={} tick={} vol_cum={} tick_cum={} win_start={}",
                    case.label,
                    l.block_timestamp,
                    l.tick,
                    l.volatility_cumulative,
                    l.tick_cumulative,
                    l.window_start_index,
                );
            }
        }

        // Read the chain fee via plugin.getCurrentFee() at the same block.
        let plugin_contract = IAlgebraFeeView::new(pool.plugin, provider.clone());
        let chain_fee = match plugin_contract.getCurrentFee().block(block).call().await {
            Ok(v) => v,
            Err(_) => {
                eprintln!("[{}] skip: plugin.getCurrentFee() failed", case.label);
                continue;
            }
        };

        // Get the block timestamp for the init block.
        let block_ts = match provider.get_block(block).await {
            Ok(Some(b)) => b.header.timestamp as u32,
            _ => {
                eprintln!("[{}] skip: could not fetch block header", case.label);
                continue;
            }
        };

        // Compute the fee locally.
        let Some(local_fee_u16) = pool.compute_fee(block_ts) else {
            return Err(eyre::eyre!(
                "[{}] compute_fee returned None (stale={}, fee_config={}, timepoints={})",
                case.label,
                pool.stale_fee_config,
                pool.fee_config.is_some(),
                pool.timepoints.is_some(),
            ));
        };
        let local_fee = u16::try_from(local_fee_u16).unwrap_or(u16::MAX);

        let drift = if local_fee >= chain_fee {
            local_fee - chain_fee
        } else {
            chain_fee - local_fee
        };

        println!(
            "[{}] block_ts={} tick={} chain_fee={} local_fee={} drift={}",
            case.label, block_ts, pool.inner.tick, chain_fee, local_fee, drift,
        );

        assert!(
            drift == 0,
            "[{}] compute_fee mismatch: chain={} local={} drift={}",
            case.label,
            chain_fee,
            local_fee,
            drift,
        );
    }

    println!("test_compute_fee_matches_chain: ALL PASSED");
    Ok(())
}

use std::sync::Arc;
use std::time::Duration;

use alloy::{
    eips::BlockId,
    primitives::{address, aliases::U24, Address, U160, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::{Filter, Log},
    sol_types::SolEvent,
};
use amms::amms::{
    amm::{AutomatedMarketMaker, SyncAction},
    pancake_v3::{IPancakeV3PoolEvents, IPancakeV3PoolState, IQuoterV2, PancakeV3Pool},
};
use eyre::{eyre, Result};
use tokio::time::sleep;

const TARGET_POOL: Address = address!("526d54cD4FAc2e6B2ddCb6bC98b9292603061f85");
const DEFAULT_QUOTER: Address = address!("B048Bbc1Ee6b733FFfCFb9e9CeF7375518e25997");
const FALLBACK_QUOTER: Address = address!("61fFE014bA17989E743c5F6cB21bF9697530B21e");

#[derive(Debug, Default)]
struct QuoteParityStats {
    exact_in_total: u64,
    exact_in_failed: u64,
    exact_out_total: u64,
    exact_out_failed: u64,
    quoter_errors: u64,
    max_exact_in_bps: f64,
    max_exact_out_bps: f64,
    worst_exact_in: String,
    worst_exact_out: String,
}

#[derive(Debug, Default)]
struct DriftStats {
    events_total: u64,
    swap_events: u64,
    mint_events: u64,
    burn_events: u64,
    sync_errors: u64,
    resyncs: u64,
    checkpoints: u64,
    mismatch_checkpoints: u64,
    first_mismatch_block: Option<u64>,
    max_sqrt_price_drift_bps: f64,
    max_liquidity_drift_bps: f64,
    max_tick_drift: i32,
}

fn abs_bps(a: U256, b: U256) -> f64 {
    if a == b {
        return 0.0;
    }
    if b.is_zero() {
        return f64::INFINITY;
    }
    let diff = if a > b { a - b } else { b - a };
    let diff_f = diff.to_string().parse::<f64>().unwrap_or(f64::INFINITY);
    let base_f = b.to_string().parse::<f64>().unwrap_or(0.0);
    if base_f == 0.0 {
        f64::INFINITY
    } else {
        (diff_f / base_f) * 10_000.0
    }
}

fn pow10(decimals: u8) -> U256 {
    U256::from(10).pow(U256::from(decimals as u64))
}

fn sample_amounts(decimals: u8) -> Vec<U256> {
    let unit = pow10(decimals);
    let mut values = vec![
        unit / U256::from(1_000_000u64),
        unit / U256::from(100_000u64),
        unit / U256::from(10_000u64),
        unit / U256::from(1_000u64),
        U256::from(1u64),
    ];
    for v in &mut values {
        if v.is_zero() {
            *v = U256::from(1u64);
        }
    }
    values.sort();
    values.dedup();
    values
}

async fn fetch_onchain_state<P: Provider + Clone>(
    provider: &P,
    pool_address: Address,
    block: BlockId,
) -> Result<(U256, i32, u128)> {
    let pool_contract = IPancakeV3PoolState::new(pool_address, provider.clone());
    let slot0 = pool_contract.slot0().block(block).call().await?;
    let liquidity = pool_contract.liquidity().block(block).call().await?;
    Ok((
        U256::from(slot0.sqrtPriceX96),
        slot0.tick.as_i32(),
        liquidity,
    ))
}

async fn fetch_pool_events_chunked<P: Provider + Clone>(
    provider: &P,
    pool_address: Address,
    from_block: u64,
    to_block: u64,
    chunk_size: u64,
) -> Result<Vec<Log>> {
    let event_sigs = vec![
        IPancakeV3PoolEvents::Swap::SIGNATURE_HASH,
        IPancakeV3PoolEvents::Mint::SIGNATURE_HASH,
        IPancakeV3PoolEvents::Burn::SIGNATURE_HASH,
    ];

    let mut all_logs = Vec::new();
    let mut current_from = from_block;

    while current_from <= to_block {
        let current_to = std::cmp::min(current_from + chunk_size - 1, to_block);
        let filter = Filter::new()
            .address(pool_address)
            .event_signature(event_sigs.clone())
            .from_block(current_from)
            .to_block(current_to);

        let mut got = None;
        for retry in 1..=5 {
            match provider.get_logs(&filter).await {
                Ok(logs) => {
                    got = Some(logs);
                    break;
                }
                Err(e) => {
                    println!(
                        "[drift] get_logs error {current_from}-{current_to}, retry={retry}/5, error={e:?}"
                    );
                    sleep(Duration::from_millis(1200)).await;
                }
            }
        }

        match got {
            Some(logs) => all_logs.extend(logs),
            None => {
                return Err(eyre!(
                    "failed to fetch logs in range {current_from}-{current_to}"
                ));
            }
        }

        current_from = current_to.saturating_add(1);
        sleep(Duration::from_millis(60)).await;
    }

    Ok(all_logs)
}

async fn detect_working_quoter<P: Provider + Clone>(
    provider: &P,
    pool: &PancakeV3Pool,
    block: BlockId,
) -> Option<Address> {
    let candidates = [DEFAULT_QUOTER, FALLBACK_QUOTER];
    let amount_in = sample_amounts(pool.token_a.decimals)
        .into_iter()
        .next()
        .unwrap_or(U256::from(1u64));

    for addr in candidates {
        let quoter = IQuoterV2::new(addr, provider.clone());
        let params = IQuoterV2::QuoteExactInputSingleParams {
            tokenIn: pool.token_a.address,
            tokenOut: pool.token_b.address,
            amountIn: amount_in,
            fee: U24::from(pool.fee),
            sqrtPriceLimitX96: U160::ZERO,
        };
        match quoter
            .quoteExactInputSingle(params)
            .block(block)
            .call()
            .await
        {
            Ok(_) => {
                println!("[quote] using quoter {addr}");
                return Some(addr);
            }
            Err(e) => {
                println!("[quote] quoter candidate {addr} unusable: {e:?}");
            }
        }
    }
    None
}

async fn run_quote_parity<P: Provider + Clone>(
    provider: &P,
    pool_address: Address,
    current_block: u64,
) -> Result<QuoteParityStats> {
    let mut stats = QuoteParityStats::default();

    let checkpoints = [
        current_block,
        current_block.saturating_sub(500),
        current_block.saturating_sub(5_000),
        current_block.saturating_sub(20_000),
    ];

    let reference_pool = PancakeV3Pool::new(pool_address)
        .init::<_, _>(BlockId::from(current_block), provider.clone())
        .await?;

    let quoter_address =
        detect_working_quoter(provider, &reference_pool, BlockId::from(current_block)).await;
    if quoter_address.is_none() {
        println!("[quote] no working quoter found, skipping quoter parity");
        return Ok(stats);
    }
    let quoter = IQuoterV2::new(quoter_address.unwrap(), provider.clone());

    for block in checkpoints {
        println!("\n[quote] checkpoint block={block}");
        let pool = PancakeV3Pool::new(pool_address)
            .init::<_, _>(BlockId::from(block), provider.clone())
            .await?;

        println!(
            "[quote] pool state: tick={}, liq={}, sqrt={}, fee={}, token_a={} ({}), token_b={} ({})",
            pool.tick,
            pool.liquidity,
            pool.sqrt_price,
            pool.fee,
            pool.token_a.address,
            pool.token_a.decimals,
            pool.token_b.address,
            pool.token_b.decimals
        );

        let amount_in_a = sample_amounts(pool.token_a.decimals);
        let amount_in_b = sample_amounts(pool.token_b.decimals);
        let amount_out_a = sample_amounts(pool.token_a.decimals);
        let amount_out_b = sample_amounts(pool.token_b.decimals);

        let directions = [
            (
                pool.token_a.address,
                pool.token_b.address,
                amount_in_a,
                amount_out_b,
            ),
            (
                pool.token_b.address,
                pool.token_a.address,
                amount_in_b,
                amount_out_a,
            ),
        ];

        for (token_in, token_out, in_amounts, out_amounts) in directions {
            for amount_in in in_amounts {
                stats.exact_in_total += 1;
                let local_out = match pool.simulate_swap(token_in, token_out, amount_in) {
                    Ok(v) => v,
                    Err(e) => {
                        stats.exact_in_failed += 1;
                        println!(
                            "[quote][exact-in] local failed block={block} token_in={token_in} token_out={token_out} amount_in={amount_in} error={e:?}"
                        );
                        continue;
                    }
                };

                let params = IQuoterV2::QuoteExactInputSingleParams {
                    tokenIn: token_in,
                    tokenOut: token_out,
                    amountIn: amount_in,
                    fee: U24::from(pool.fee),
                    sqrtPriceLimitX96: U160::ZERO,
                };

                let quote_out = match quoter
                    .quoteExactInputSingle(params)
                    .block(BlockId::from(block))
                    .call()
                    .await
                {
                    Ok(v) => v.amountOut,
                    Err(e) => {
                        stats.quoter_errors += 1;
                        println!(
                            "[quote][exact-in] quoter failed block={block} token_in={token_in} token_out={token_out} amount_in={amount_in} error={e:?}"
                        );
                        continue;
                    }
                };

                let bps = abs_bps(local_out, quote_out);
                if bps > stats.max_exact_in_bps {
                    stats.max_exact_in_bps = bps;
                    stats.worst_exact_in = format!(
                        "block={block}, in={token_in}, out={token_out}, amount_in={amount_in}, local={local_out}, quote={quote_out}, bps={bps:.6}"
                    );
                }
            }

            for amount_out in out_amounts {
                stats.exact_out_total += 1;
                let local_in = match pool.simulate_swap_exact_out(token_in, token_out, amount_out) {
                    Ok(v) => v,
                    Err(e) => {
                        stats.exact_out_failed += 1;
                        println!(
                            "[quote][exact-out] local failed block={block} token_in={token_in} token_out={token_out} amount_out={amount_out} error={e:?}"
                        );
                        continue;
                    }
                };

                let params = IQuoterV2::QuoteExactOutputSingleParams {
                    tokenIn: token_in,
                    tokenOut: token_out,
                    amountOut: amount_out,
                    fee: U24::from(pool.fee),
                    sqrtPriceLimitX96: U160::ZERO,
                };

                let quote_in = match quoter
                    .quoteExactOutputSingle(params)
                    .block(BlockId::from(block))
                    .call()
                    .await
                {
                    Ok(v) => v.amountIn,
                    Err(e) => {
                        stats.quoter_errors += 1;
                        println!(
                            "[quote][exact-out] quoter failed block={block} token_in={token_in} token_out={token_out} amount_out={amount_out} error={e:?}"
                        );
                        continue;
                    }
                };

                let bps = abs_bps(local_in, quote_in);
                if bps > stats.max_exact_out_bps {
                    stats.max_exact_out_bps = bps;
                    stats.worst_exact_out = format!(
                        "block={block}, in={token_in}, out={token_out}, amount_out={amount_out}, local={local_in}, quote={quote_in}, bps={bps:.6}"
                    );
                }
            }
        }
    }

    Ok(stats)
}

async fn run_drift_replay<P: Provider + Clone>(
    provider: &P,
    pool_address: Address,
    current_block: u64,
) -> Result<DriftStats> {
    let mut stats = DriftStats::default();

    let block_range: u64 = std::env::var("DRIFT_BLOCK_RANGE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120_000);
    let chunk_size: u64 = std::env::var("DRIFT_LOG_CHUNK")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5_000);
    let check_interval: u64 = std::env::var("DRIFT_CHECK_INTERVAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);

    let start_block = current_block.saturating_sub(block_range);
    println!(
        "\n[drift] start_block={start_block}, end_block={current_block}, range={block_range}, chunk_size={chunk_size}, check_interval={check_interval}"
    );

    let mut pool = PancakeV3Pool::new(pool_address)
        .init::<_, _>(BlockId::from(start_block), provider.clone())
        .await?;
    pool.set_last_synced_block(start_block);

    let (oc_sqrt_start, oc_tick_start, oc_liq_start) =
        fetch_onchain_state(provider, pool_address, BlockId::from(start_block)).await?;
    println!(
        "[drift] init local sqrt={},tick={},liq={} | chain sqrt={},tick={},liq={}",
        pool.sqrt_price, pool.tick, pool.liquidity, oc_sqrt_start, oc_tick_start, oc_liq_start
    );

    let mut events = fetch_pool_events_chunked(
        provider,
        pool_address,
        start_block.saturating_add(1),
        current_block,
        chunk_size,
    )
    .await?;

    events.sort_by(|a, b| {
        let a_block = a.block_number.unwrap_or(0);
        let b_block = b.block_number.unwrap_or(0);
        if a_block != b_block {
            return a_block.cmp(&b_block);
        }
        let a_tx = a.transaction_index.unwrap_or(0);
        let b_tx = b.transaction_index.unwrap_or(0);
        if a_tx != b_tx {
            return a_tx.cmp(&b_tx);
        }
        let a_log = a.log_index.unwrap_or(0);
        let b_log = b.log_index.unwrap_or(0);
        a_log.cmp(&b_log)
    });

    stats.events_total = events.len() as u64;
    println!("[drift] fetched events={}", stats.events_total);
    println!(
        "[drift] local swap topic0={}",
        IPancakeV3PoolEvents::Swap::SIGNATURE_HASH
    );

    if events.is_empty() {
        return Ok(stats);
    }

    let mut last_checked = start_block;
    let mut i = 0usize;
    while i < events.len() {
        let block = events[i].block_number.unwrap_or(0);

        // Replay all logs of the same block before checkpointing.
        while i < events.len() && events[i].block_number.unwrap_or(0) == block {
            let log = &events[i];
            let topic0 = log.topics()[0];
            if topic0 == IPancakeV3PoolEvents::Swap::SIGNATURE_HASH {
                stats.swap_events += 1;
            } else if topic0 == IPancakeV3PoolEvents::Mint::SIGNATURE_HASH {
                stats.mint_events += 1;
            } else if topic0 == IPancakeV3PoolEvents::Burn::SIGNATURE_HASH {
                stats.burn_events += 1;
            }

            match pool.sync(log) {
                Ok(action) => {
                    if matches!(action, SyncAction::Resync) {
                        stats.resyncs += 1;
                        println!("[drift] got Resync at block={block}, doing fresh init");
                        pool = PancakeV3Pool::new(pool_address)
                            .init::<_, _>(BlockId::from(block), provider.clone())
                            .await?;
                    }
                }
                Err(e) => {
                    stats.sync_errors += 1;
                    println!("[drift] sync error at block={block}: {e:?}");
                }
            }

            i += 1;
            if i % 2000 == 0 {
                println!(
                    "[drift] replay progress: {}/{} events processed",
                    i,
                    events.len()
                );
            }
        }

        pool.set_last_synced_block(block);

        if block >= last_checked.saturating_add(check_interval) {
            stats.checkpoints += 1;
            let (oc_sqrt, oc_tick, oc_liq) =
                fetch_onchain_state(provider, pool_address, BlockId::from(block)).await?;

            let sqrt_bps = abs_bps(pool.sqrt_price, oc_sqrt);
            let liq_bps = abs_bps(U256::from(pool.liquidity), U256::from(oc_liq));
            let tick_drift = (pool.tick - oc_tick).abs();

            if sqrt_bps > stats.max_sqrt_price_drift_bps {
                stats.max_sqrt_price_drift_bps = sqrt_bps;
            }
            if liq_bps > stats.max_liquidity_drift_bps {
                stats.max_liquidity_drift_bps = liq_bps;
            }
            if tick_drift > stats.max_tick_drift {
                stats.max_tick_drift = tick_drift;
            }

            let matched =
                pool.sqrt_price == oc_sqrt && pool.tick == oc_tick && pool.liquidity == oc_liq;
            if !matched {
                stats.mismatch_checkpoints += 1;
                if stats.first_mismatch_block.is_none() {
                    stats.first_mismatch_block = Some(block);
                }
                if stats.mismatch_checkpoints <= 12 || stats.mismatch_checkpoints % 50 == 0 {
                    println!(
                        "[drift] mismatch block={block} | local(sqrt={},tick={},liq={}) chain(sqrt={},tick={},liq={}) | sqrt_bps={:.6}, liq_bps={:.6}, tick_abs={}",
                        pool.sqrt_price,
                        pool.tick,
                        pool.liquidity,
                        oc_sqrt,
                        oc_tick,
                        oc_liq,
                        sqrt_bps,
                        liq_bps,
                        tick_drift
                    );
                }
            }

            last_checked = block;
            if stats.checkpoints % 100 == 0 {
                println!(
                    "[drift] checkpoint progress: {} checked, mismatches={}",
                    stats.checkpoints, stats.mismatch_checkpoints
                );
            }
        }
    }

    if let Some(last) = events.last() {
        let final_block = last.block_number.unwrap_or(current_block);
        let (oc_sqrt, oc_tick, oc_liq) =
            fetch_onchain_state(provider, pool_address, BlockId::from(final_block)).await?;
        println!(
            "[drift] final block={final_block} | local(sqrt={},tick={},liq={}) chain(sqrt={},tick={},liq={})",
            pool.sqrt_price, pool.tick, pool.liquidity, oc_sqrt, oc_tick, oc_liq
        );
    }

    Ok(stats)
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();

    let rpc = std::env::var("BASE_PROVIDER")
        .or_else(|_| std::env::var("ETHEREUM_PROVIDER"))
        .map_err(|_| eyre!("BASE_PROVIDER (or ETHEREUM_PROVIDER) is required"))?;

    let provider = Arc::new(ProviderBuilder::new().connect_http(rpc.parse()?));
    let current_block = provider.get_block_number().await?;

    println!("=== PancakeV3 Base Pool Diagnostics ===");
    println!("pool={TARGET_POOL}");
    println!("current_block={current_block}");

    let quote_stats = run_quote_parity(&*provider, TARGET_POOL, current_block).await?;
    println!("\n=== Quote Parity Summary ===");
    println!(
        "exact_in_total={}, exact_in_failed={}, exact_out_total={}, exact_out_failed={}, quoter_errors={}",
        quote_stats.exact_in_total,
        quote_stats.exact_in_failed,
        quote_stats.exact_out_total,
        quote_stats.exact_out_failed,
        quote_stats.quoter_errors
    );
    println!(
        "max_exact_in_bps={:.6}, max_exact_out_bps={:.6}",
        quote_stats.max_exact_in_bps, quote_stats.max_exact_out_bps
    );
    if !quote_stats.worst_exact_in.is_empty() {
        println!("worst_exact_in: {}", quote_stats.worst_exact_in);
    }
    if !quote_stats.worst_exact_out.is_empty() {
        println!("worst_exact_out: {}", quote_stats.worst_exact_out);
    }

    let drift_stats = run_drift_replay(&*provider, TARGET_POOL, current_block).await?;
    println!("\n=== Drift Replay Summary ===");
    println!(
        "events_total={}, swap={}, mint={}, burn={}, sync_errors={}, resyncs={}",
        drift_stats.events_total,
        drift_stats.swap_events,
        drift_stats.mint_events,
        drift_stats.burn_events,
        drift_stats.sync_errors,
        drift_stats.resyncs
    );
    println!(
        "checkpoints={}, mismatches={}, first_mismatch_block={:?}",
        drift_stats.checkpoints, drift_stats.mismatch_checkpoints, drift_stats.first_mismatch_block
    );
    println!(
        "max_sqrt_price_drift_bps={:.6}, max_liquidity_drift_bps={:.6}, max_tick_drift={}",
        drift_stats.max_sqrt_price_drift_bps,
        drift_stats.max_liquidity_drift_bps,
        drift_stats.max_tick_drift
    );

    Ok(())
}

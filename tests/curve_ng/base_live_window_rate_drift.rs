use alloy::{
    eips::BlockId,
    primitives::{address, Address, B256, U256},
    providers::{Provider, ProviderBuilder},
    rpc::client::ClientBuilder,
    rpc::types::{Filter, Log},
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::amms::{
    amm::{AutomatedMarketMaker, AMM},
    curve_ng::{CurveNGPool, CurveNGPoolType, ICurveNGPool, ICurveNGStableSwap},
};
use eyre::{eyre, Result};
use std::{collections::HashSet, env};

const START_BLOCK: u64 = 46_117_282;
const END_BLOCK: u64 = 46_117_291;
const POOL: Address = address!("ae07db17dEbde8391c6ea3a6990eBdD9E4939494");
const WETH: Address = address!("4200000000000000000000000000000000000006");
const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const QUOTE_DX_USDC: U256 = U256::from_limbs([20_000_000_000u64, 0, 0, 0]); // 20,000 USDC (6 decimals)

fn base_provider_url() -> Option<String> {
    dotenv::dotenv().ok();
    env::var("BASE_PROVIDER")
        .or_else(|_| env::var("BASE_RPC_URL"))
        .or_else(|_| env::var("BASE_MAINNET_RPC_URL"))
        .ok()
}

fn sort_logs(logs: &mut [Log]) {
    logs.sort_by_key(|l| {
        (
            l.block_number.unwrap_or_default(),
            l.transaction_index.unwrap_or_default(),
            l.log_index.unwrap_or_default(),
        )
    });
}

fn abs_diff(a: U256, b: U256) -> U256 {
    if a >= b {
        a - b
    } else {
        b - a
    }
}

async fn chain_balances_at_block<P: Provider + Clone>(
    provider: &P,
    block: u64,
) -> Result<[U256; 2]> {
    let c = ICurveNGPool::new(POOL, provider.clone());
    let b0 = c
        .balances(U256::from(0u8))
        .block(BlockId::from(block))
        .call()
        .await?;
    let b1 = c
        .balances(U256::from(1u8))
        .block(BlockId::from(block))
        .call()
        .await?;
    Ok([b0, b1])
}

#[tokio::test]
async fn test_curve_ng_base_live_window_rates_quote_drift() -> Result<()> {
    let rpc_url = match base_provider_url() {
        Some(url) => url,
        None => {
            println!("Skipping test: BASE_PROVIDER or BASE_RPC_URL not set");
            return Ok(());
        }
    };
    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(50))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(rpc_url.parse()?);
    let provider = ProviderBuilder::new().connect_client(client);

    let mut local = AMM::CurveNGPool(CurveNGPool::new(POOL, CurveNGPoolType::StableSwap))
        .init(BlockId::from(START_BLOCK), provider.clone())
        .await?;
    let stable = ICurveNGStableSwap::new(POOL, provider.clone());

    let sync_topics: HashSet<B256> = local.sync_events().into_iter().collect();
    let mut logs = provider
        .get_logs(
            &Filter::new()
                .address(POOL)
                .from_block(START_BLOCK + 1)
                .to_block(END_BLOCK),
        )
        .await?;
    sort_logs(&mut logs);

    let mut sync_logs = Vec::new();
    for l in logs {
        if let Some(t0) = l.topics().first() {
            if sync_topics.contains(t0) {
                sync_logs.push(l);
            }
        }
    }
    sort_logs(&mut sync_logs);

    let mut log_idx = 0usize;

    let mut max_bal_diff = [U256::ZERO, U256::ZERO];
    let mut max_rate_diff = [U256::ZERO, U256::ZERO];
    let mut max_quote_diff_stale = U256::ZERO;
    let mut max_quote_diff_fresh_rates = U256::ZERO;

    println!(
        "[AE07-live] replay window={}..{} sync_logs={}",
        START_BLOCK + 1,
        END_BLOCK,
        sync_logs.len()
    );

    for block in START_BLOCK..=END_BLOCK {
        while log_idx < sync_logs.len()
            && sync_logs[log_idx].block_number.unwrap_or_default() == block
        {
            local.sync(&sync_logs[log_idx])?;
            log_idx += 1;
        }

        let local_pool = match &local {
            AMM::CurveNGPool(p) => p,
            _ => return Err(eyre!("not CurveNG pool")),
        };
        if local_pool.rates.len() < 2 || local_pool.balances.len() < 2 {
            return Err(eyre!("invalid local pool vectors"));
        }

        let local_bal = [local_pool.balances[0], local_pool.balances[1]];
        let chain_bal = chain_balances_at_block(&provider, block).await?;
        let chain_rates = stable
            .stored_rates()
            .block(BlockId::from(block))
            .call()
            .await?;
        if chain_rates.len() < 2 {
            return Err(eyre!("invalid chain rates len"));
        }

        let local_rates = [local_pool.rates[0], local_pool.rates[1]];
        let rate_diff = [
            abs_diff(local_rates[0], chain_rates[0]),
            abs_diff(local_rates[1], chain_rates[1]),
        ];
        let bal_diff = [
            abs_diff(local_bal[0], chain_bal[0]),
            abs_diff(local_bal[1], chain_bal[1]),
        ];
        for i in 0..2 {
            if bal_diff[i] > max_bal_diff[i] {
                max_bal_diff[i] = bal_diff[i];
            }
            if rate_diff[i] > max_rate_diff[i] {
                max_rate_diff[i] = rate_diff[i];
            }
        }

        let local_out_stale = local.simulate_swap(USDC, WETH, QUOTE_DX_USDC)?;
        let chain_out = stable
            .get_dy(1i128, 0i128, QUOTE_DX_USDC)
            .block(BlockId::from(block))
            .call()
            .await?;
        let quote_diff_stale = abs_diff(local_out_stale, chain_out);
        if quote_diff_stale > max_quote_diff_stale {
            max_quote_diff_stale = quote_diff_stale;
        }

        let mut local_fresh = local.clone();
        if let AMM::CurveNGPool(pf) = &mut local_fresh {
            pf.rates = chain_rates.clone();
        }
        let local_out_fresh = local_fresh.simulate_swap(USDC, WETH, QUOTE_DX_USDC)?;
        let quote_diff_fresh = abs_diff(local_out_fresh, chain_out);
        if quote_diff_fresh > max_quote_diff_fresh_rates {
            max_quote_diff_fresh_rates = quote_diff_fresh;
        }

        println!(
            "[AE07-live] block={} bal_diff=[{}, {}] rate_diff=[{}, {}] quote_diff_stale={} quote_diff_fresh_rates={}",
            block,
            bal_diff[0],
            bal_diff[1],
            rate_diff[0],
            rate_diff[1],
            quote_diff_stale,
            quote_diff_fresh
        );
    }

    println!(
        "[AE07-live] summary max_bal_diff=[{}, {}] max_rate_diff=[{}, {}] max_quote_diff_stale={} max_quote_diff_fresh_rates={}",
        max_bal_diff[0],
        max_bal_diff[1],
        max_rate_diff[0],
        max_rate_diff[1],
        max_quote_diff_stale,
        max_quote_diff_fresh_rates
    );

    assert!(max_rate_diff[0] > U256::ZERO || max_rate_diff[1] > U256::ZERO);
    assert!(max_quote_diff_stale > U256::ZERO);
    Ok(())
}

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

const START_BLOCK: u64 = 46_072_150;

#[derive(Clone, Copy)]
struct Case {
    label: &'static str,
    pool: Address,
    end_block: u64,
}

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
    pool: Address,
    block: u64,
) -> Result<[U256; 2]> {
    let c = ICurveNGPool::new(pool, provider.clone());
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
async fn test_curve_ng_base_init_window_rates_observation() -> Result<()> {
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

    let cases = [
        Case {
            label: "AE07-WETH-USDC",
            pool: address!("ae07db17dEbde8391c6ea3a6990eBdD9E4939494"),
            end_block: 46_072_222,
        },
        Case {
            label: "A92B-USDC-cbBTC",
            pool: address!("A92b4Cc758C30EA7f734089BB0c0d0914f1241b4"),
            end_block: 46_072_219,
        },
    ];

    for case in cases {
        let mut local = AMM::CurveNGPool(CurveNGPool::new(case.pool, CurveNGPoolType::StableSwap))
            .init(BlockId::from(START_BLOCK), provider.clone())
            .await?;
        let stable = ICurveNGStableSwap::new(case.pool, provider.clone());

        let sync_topics: HashSet<B256> = local.sync_events().into_iter().collect();
        let mut all_logs = provider
            .get_logs(
                &Filter::new()
                    .address(case.pool)
                    .from_block(START_BLOCK + 1)
                    .to_block(case.end_block),
            )
            .await?;
        sort_logs(&mut all_logs);

        let mut sync_logs = Vec::new();
        for log in all_logs {
            if let Some(topic0) = log.topics().first() {
                if sync_topics.contains(topic0) {
                    sync_logs.push(log);
                }
            }
        }
        sort_logs(&mut sync_logs);

        let mut log_idx = 0usize;
        let mut max_bal_diff = [U256::ZERO, U256::ZERO];
        let mut max_bal_block = [START_BLOCK, START_BLOCK];
        let mut max_rate_diff = [U256::ZERO, U256::ZERO];
        let mut max_rate_block = [START_BLOCK, START_BLOCK];
        let mut first_chain_rates: Option<[U256; 2]> = None;
        let mut last_chain_rates: [U256; 2] = [U256::ZERO, U256::ZERO];
        let mut prev_chain_rates: Option<[U256; 2]> = None;
        let mut rate_change_count = [0u64, 0u64];
        let mut max_step_rate_change = [U256::ZERO, U256::ZERO];
        let mut monotonic_non_decreasing = [true, true];
        let mut monotonic_non_increasing = [true, true];

        for block in START_BLOCK..=case.end_block {
            while log_idx < sync_logs.len()
                && sync_logs[log_idx].block_number.unwrap_or_default() == block
            {
                local.sync(&sync_logs[log_idx])?;
                log_idx += 1;
            }

            let pool = match &local {
                AMM::CurveNGPool(p) => p,
                _ => return Err(eyre!("not CurveNG pool")),
            };
            if pool.balances.len() < 2 || pool.rates.len() < 2 {
                return Err(eyre!("invalid local vectors"));
            }

            let local_bal = [pool.balances[0], pool.balances[1]];
            let chain_bal = chain_balances_at_block(&provider, case.pool, block).await?;
            for i in 0..2 {
                let d = abs_diff(local_bal[i], chain_bal[i]);
                if d > max_bal_diff[i] {
                    max_bal_diff[i] = d;
                    max_bal_block[i] = block;
                }
            }

            let local_rates = [pool.rates[0], pool.rates[1]];
            let chain_rates = stable
                .stored_rates()
                .block(BlockId::from(block))
                .call()
                .await?;
            if chain_rates.len() >= 2 {
                let chain_rates2 = [chain_rates[0], chain_rates[1]];
                if first_chain_rates.is_none() {
                    first_chain_rates = Some(chain_rates2);
                }
                last_chain_rates = chain_rates2;
                if let Some(prev) = prev_chain_rates {
                    for i in 0..2 {
                        let step = abs_diff(chain_rates2[i], prev[i]);
                        if step > U256::ZERO {
                            rate_change_count[i] += 1;
                        }
                        if step > max_step_rate_change[i] {
                            max_step_rate_change[i] = step;
                        }
                        if chain_rates2[i] < prev[i] {
                            monotonic_non_decreasing[i] = false;
                        }
                        if chain_rates2[i] > prev[i] {
                            monotonic_non_increasing[i] = false;
                        }
                    }
                }
                prev_chain_rates = Some(chain_rates2);
                for i in 0..2 {
                    let d = abs_diff(local_rates[i], chain_rates2[i]);
                    if d > max_rate_diff[i] {
                        max_rate_diff[i] = d;
                        max_rate_block[i] = block;
                    }
                }
            }
        }

        let first = first_chain_rates.ok_or_else(|| eyre!("missing first chain rates"))?;
        let total_steps = case.end_block.saturating_sub(START_BLOCK);
        let mut net_change_bps = [0u128, 0u128];
        for i in 0..2 {
            if !first[i].is_zero() {
                let num = abs_diff(last_chain_rates[i], first[i]) * U256::from(10_000u64);
                net_change_bps[i] = (num / first[i]).try_into().unwrap_or(u128::MAX);
            }
        }

        println!(
            "[{}] window={}..{} max_bal_diff=[{}@{}, {}@{}] max_rate_diff=[{}@{}, {}@{}]",
            case.label,
            START_BLOCK,
            case.end_block,
            max_bal_diff[0],
            max_bal_block[0],
            max_bal_diff[1],
            max_bal_block[1],
            max_rate_diff[0],
            max_rate_block[0],
            max_rate_diff[1],
            max_rate_block[1]
        );
        println!(
            "[{}] rates_change_freq coin0={}/{} coin1={}/{} max_step=[{}, {}] monotonic_non_decreasing=[{}, {}] monotonic_non_increasing=[{}, {}] net_change_bps=[{}, {}]",
            case.label,
            rate_change_count[0],
            total_steps,
            rate_change_count[1],
            total_steps,
            max_step_rate_change[0],
            max_step_rate_change[1],
            monotonic_non_decreasing[0],
            monotonic_non_decreasing[1],
            monotonic_non_increasing[0],
            monotonic_non_increasing[1],
            net_change_bps[0],
            net_change_bps[1]
        );
    }

    Ok(())
}

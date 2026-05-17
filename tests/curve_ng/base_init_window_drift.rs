use alloy::{
    eips::BlockId,
    primitives::{address, Address, B256, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::{Filter, Log},
};
use amms::amms::{
    amm::{AutomatedMarketMaker, AMM},
    curve_ng::{CurveNGPool, CurveNGPoolType, ICurveNGPool},
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

fn env_u64(name: &str) -> Option<u64> {
    env::var(name).ok().and_then(|v| v.parse::<u64>().ok())
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

fn abs_diff(a: U256, b: U256) -> U256 {
    if a >= b {
        a - b
    } else {
        b - a
    }
}

fn pool_balances_2(amm: &AMM) -> Result<[U256; 2]> {
    match amm {
        AMM::CurveNGPool(pool) => {
            if pool.balances.len() < 2 {
                return Err(eyre!("CurveNG balances len < 2"));
            }
            Ok([pool.balances[0], pool.balances[1]])
        }
        _ => Err(eyre!("not a CurveNG pool")),
    }
}

#[tokio::test]
async fn test_curve_ng_base_init_window_drift_regression() -> Result<()> {
    let rpc_url = match base_provider_url() {
        Some(url) => url,
        None => {
            println!("Skipping test: BASE_PROVIDER or BASE_RPC_URL not set");
            return Ok(());
        }
    };
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    let range_override = env_u64("CURVE_NG_BASE_DRIFT_RANGE");
    let check_interval = env_u64("CURVE_NG_BASE_DRIFT_CHECK_INTERVAL")
        .unwrap_or(1)
        .max(1);
    let tol = env_u64("CURVE_NG_BASE_DRIFT_TOL")
        .map(U256::from)
        .unwrap_or_else(|| U256::from(100u64));

    let mut cases = [
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

    if let Some(range) = range_override {
        let end_block = START_BLOCK.saturating_add(range);
        for case in &mut cases {
            case.end_block = end_block;
        }
    }

    for case in cases {
        let init_amm = AMM::CurveNGPool(CurveNGPool::new(case.pool, CurveNGPoolType::StableSwap))
            .init(BlockId::from(START_BLOCK), provider.clone())
            .await?;

        let sync_topics: HashSet<B256> = init_amm.sync_events().into_iter().collect();
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
        let mut non_sync_topic_logs = 0usize;
        for log in all_logs {
            if let Some(topic0) = log.topics().first() {
                if sync_topics.contains(topic0) {
                    sync_logs.push(log);
                } else {
                    non_sync_topic_logs += 1;
                }
            } else {
                non_sync_topic_logs += 1;
            }
        }
        sort_logs(&mut sync_logs);

        assert_eq!(
            non_sync_topic_logs, 0,
            "[{}] expected zero non-sync-topic logs in window",
            case.label
        );

        let mut local = init_amm.clone();
        let mut log_idx = 0usize;
        let mut max_diff = [U256::ZERO, U256::ZERO];
        let mut max_diff_block = [START_BLOCK, START_BLOCK];

        for block in (START_BLOCK + 1)..=case.end_block {
            while log_idx < sync_logs.len()
                && sync_logs[log_idx].block_number.unwrap_or_default() == block
            {
                local.sync(&sync_logs[log_idx])?;
                log_idx += 1;
            }

            let should_check = block == case.end_block
                || (block.saturating_sub(START_BLOCK) % check_interval == 0);
            if !should_check {
                continue;
            }

            let local_balances = pool_balances_2(&local)?;
            let chain_balances = chain_balances_at_block(&provider, case.pool, block).await?;
            for idx in 0..2 {
                let d = abs_diff(local_balances[idx], chain_balances[idx]);
                if d > max_diff[idx] {
                    max_diff[idx] = d;
                    max_diff_block[idx] = block;
                }
            }
        }

        println!(
            "[{}] window={}..{} max_diff_coin0={} at block {}, max_diff_coin1={} at block {}, tol={}",
            case.label,
            START_BLOCK + 1,
            case.end_block,
            max_diff[0],
            max_diff_block[0],
            max_diff[1],
            max_diff_block[1],
            tol
        );
        assert!(
            max_diff[0] <= tol && max_diff[1] <= tol,
            "[{}] drift too large after fix: max_diff_coin0={} at block {}, max_diff_coin1={} at block {}",
            case.label,
            max_diff[0],
            max_diff_block[0],
            max_diff[1],
            max_diff_block[1]
        );
    }

    Ok(())
}

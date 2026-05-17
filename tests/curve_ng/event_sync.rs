use alloy::{
    eips::BlockId,
    primitives::Address,
    providers::{Provider, ProviderBuilder},
    rpc::types::{Filter, Log},
    sol_types::SolEvent,
};
use amms::amms::{
    amm::{AutomatedMarketMaker, SyncAction},
    curve_ng::{
        CurveNGPool, CurveNGPoolType, CurveNGTwoCryptoVariant, ICurveNGPool, ICurveTwoCryptoEvent,
    },
};
use eyre::Result;
use std::{env, str::FromStr};

use crate::common::rpc::provider_url;

fn base_provider_url() -> Option<String> {
    dotenv::dotenv().ok();
    env::var("BASE_PROVIDER")
        .or_else(|_| env::var("BASE_RPC_URL"))
        .or_else(|_| env::var("BASE_MAINNET_RPC_URL"))
        .ok()
}

#[tokio::test]
async fn test_ng_twocrypto_event_sync_flow_consistency() -> Result<()> {
    let rpc_url = match provider_url() {
        Some(url) => url,
        None => {
            println!("Skipping test: ETHEREUM_PROVIDER/ETHEREUM_RPC_URL not set");
            return Ok(());
        }
    };

    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    if let Err(e) = provider.get_block_number().await {
        println!("Skipping test: Ethereum RPC unavailable ({e})");
        return Ok(());
    }

    let cases = [
        (
            "standard",
            "0xca546ae6c3b2bb9fba2b6e5eeb0881097cece5b0",
            CurveNGTwoCryptoVariant::StandardV210,
        ),
        (
            "yieldbasis-special",
            "0x83f24023d15d835a213df24fd309c47dab5beb32",
            CurveNGTwoCryptoVariant::PeripheryV210d,
        ),
    ];

    for (label, addr_str, expected_variant) in cases {
        let addr = Address::from_str(addr_str)?;
        let latest = match provider.get_block_number().await {
            Ok(v) => v,
            Err(e) => {
                println!("[{label}] skipping: Ethereum RPC unavailable ({e})");
                return Ok(());
            }
        };
        let from = latest.saturating_sub(10_000);

        let mut pool = CurveNGPool::new(addr, CurveNGPoolType::TwoCrypto)
            .init(BlockId::number(latest), provider.clone())
            .await?;

        assert_eq!(
            pool.twocrypto_variant, expected_variant,
            "[{}] twocrypto variant mismatch",
            label
        );

        let filter = Filter::new()
            .address(addr)
            .event_signature(pool.sync_events())
            .from_block(from)
            .to_block(latest);

        let mut logs: Vec<Log> = provider.get_logs(&filter).await?;
        assert!(
            !logs.is_empty(),
            "[{}] no twocrypto sync events found",
            label
        );

        logs.sort_by(|a, b| {
            let a_bn = a.block_number.unwrap_or(0);
            let b_bn = b.block_number.unwrap_or(0);
            if a_bn != b_bn {
                return a_bn.cmp(&b_bn);
            }
            let a_tx = a.transaction_index.unwrap_or(0);
            let b_tx = b.transaction_index.unwrap_or(0);
            if a_tx != b_tx {
                return a_tx.cmp(&b_tx);
            }
            a.log_index.unwrap_or(0).cmp(&b.log_index.unwrap_or(0))
        });

        let last_log = logs.last().expect("log exists").clone();
        let topic0 = last_log.topics()[0];

        let expected_action = if topic0 == ICurveTwoCryptoEvent::TokenExchange::SIGNATURE_HASH
            || topic0 == ICurveTwoCryptoEvent::AddLiquidity::SIGNATURE_HASH
            || topic0 == ICurveTwoCryptoEvent::RemoveLiquidity::SIGNATURE_HASH
            || topic0 == ICurveTwoCryptoEvent::RemoveLiquidityOne::SIGNATURE_HASH
        {
            SyncAction::AsyncUpdate
        } else if topic0 == ICurveTwoCryptoEvent::NewParameters::SIGNATURE_HASH {
            SyncAction::None
        } else {
            panic!("[{}] unexpected event topic: {:?}", label, topic0);
        };

        let action = pool.sync(&last_log)?;
        assert_eq!(
            action, expected_action,
            "[{}] unexpected sync action for topic {:?}",
            label, topic0
        );

        if action == SyncAction::AsyncUpdate {
            pool.update(provider.clone()).await?;
            assert_eq!(
                pool.balances.len(),
                2,
                "[{}] balances length invalid",
                label
            );
            assert!(
                pool.price_scale
                    .as_ref()
                    .map(|v| !v.is_empty())
                    .unwrap_or(false),
                "[{}] missing price_scale after update",
                label
            );
            assert!(pool.d.is_some(), "[{}] missing D after update", label);
            assert!(
                pool.twocrypto_future_a_gamma_time.is_some(),
                "[{}] missing future_A_gamma_time after update",
                label
            );
            assert!(
                pool.twocrypto_last_timestamp.is_some(),
                "[{}] missing last_timestamp after update",
                label
            );
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_ng_stableswap_events_return_async_update() -> Result<()> {
    let rpc_url = match base_provider_url() {
        Some(url) => url,
        None => {
            println!("Skipping test: BASE_PROVIDER or BASE_RPC_URL not set");
            return Ok(());
        }
    };

    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let latest = match provider.get_block_number().await {
        Ok(v) => v,
        Err(e) => {
            println!("Skipping test: Base RPC unavailable ({e})");
            return Ok(());
        }
    };
    let addr = Address::from_str("ae07db17dEbde8391c6ea3a6990eBdD9E4939494")?;
    let from_block = 45_000_000u64;

    let cases = [
        ("TokenExchange", ICurveNGPool::TokenExchange::SIGNATURE_HASH),
        ("AddLiquidity", ICurveNGPool::AddLiquidity::SIGNATURE_HASH),
        (
            "RemoveLiquidity",
            ICurveNGPool::RemoveLiquidity::SIGNATURE_HASH,
        ),
        (
            "RemoveLiquidityOne",
            ICurveNGPool::RemoveLiquidityOne::SIGNATURE_HASH,
        ),
    ];

    for (label, sig) in cases {
        let mut logs = provider
            .get_logs(
                &Filter::new()
                    .address(addr)
                    .event_signature(vec![sig])
                    .from_block(from_block)
                    .to_block(latest),
            )
            .await?;
        assert!(
            !logs.is_empty(),
            "[StableSwap {}] no logs found for pool {} in window {}..{}",
            label,
            addr,
            from_block,
            latest
        );

        logs.sort_by(|a, b| {
            let a_bn = a.block_number.unwrap_or(0);
            let b_bn = b.block_number.unwrap_or(0);
            if a_bn != b_bn {
                return a_bn.cmp(&b_bn);
            }
            let a_tx = a.transaction_index.unwrap_or(0);
            let b_tx = b.transaction_index.unwrap_or(0);
            if a_tx != b_tx {
                return a_tx.cmp(&b_tx);
            }
            a.log_index.unwrap_or(0).cmp(&b.log_index.unwrap_or(0))
        });
        let log = logs.last().expect("log exists").clone();
        let block = log.block_number.unwrap_or_default();
        let init_block = block.saturating_sub(1);

        let mut pool = CurveNGPool::new(addr, CurveNGPoolType::StableSwap)
            .init(BlockId::from(init_block), provider.clone())
            .await?;
        let action = pool.sync(&log)?;
        assert_eq!(
            action,
            SyncAction::AsyncUpdate,
            "[StableSwap {}] unexpected action at block {}",
            label,
            block
        );
    }

    Ok(())
}

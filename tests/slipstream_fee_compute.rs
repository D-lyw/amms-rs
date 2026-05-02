use alloy::{
    eips::BlockId,
    primitives::{address, Address},
    providers::{Provider, ProviderBuilder},
    rpc::{client::ClientBuilder, types::Filter},
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::amms::{
    aerodrome_slipstream::{AerodromeSlipstreamFactory, AerodromeSlipstreamPool, ICLPool},
    amm::{AutomatedMarketMaker, AMM},
};
use std::{collections::HashMap, env, sync::Arc};

const TARGET_POOLS: [Address; 4] = [
    address!("56AeaF4af2DF4bdFD9D865830Fefdd278b25E7Ef"),
    address!("99fb961b5f8D5Bf137976bA50cF6999546b0503f"),
    address!("be4C36B9542610dF83Ca690C8b5BC53BbbC5d542"),
    address!("EFCC15B5976af35aADD4755A730022FF9feA440B"),
];

const START_BLOCK: u64 = 45_408_821;
const REPLAY_BLOCKS: u64 = 100;

fn base_provider_url() -> Option<String> {
    dotenv::dotenv().ok();
    env::var("BASE_PROVIDER")
        .or_else(|_| env::var("BASE_RPC_URL"))
        .ok()
}

#[tokio::test]
async fn test_slipstream_basefee_zero_pool_fee_parity() -> eyre::Result<()> {
    let rpc_endpoint = match base_provider_url() {
        Some(url) => url,
        None => {
            println!("Skipping test: BASE_PROVIDER or BASE_RPC_URL not set");
            return Ok(());
        }
    };

    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(300))
        .layer(RetryBackoffLayer::new(5, 200, 350))
        .http(rpc_endpoint.parse()?);
    let provider = Arc::new(ProviderBuilder::new().connect_client(client));

    let input_amms: Vec<AMM> = TARGET_POOLS
        .iter()
        .copied()
        .map(|addr| AMM::AerodromeSlipstreamPool(AerodromeSlipstreamPool::new(addr)))
        .collect();

    let initialized = AerodromeSlipstreamFactory::init_batch(
        input_amms,
        BlockId::from(START_BLOCK),
        provider.clone(),
    )
    .await?;

    assert_eq!(
        initialized.len(),
        TARGET_POOLS.len(),
        "expected all 4 target pools to initialize"
    );

    let mut pools: HashMap<Address, AerodromeSlipstreamPool> = initialized
        .into_iter()
        .map(|amm| match amm {
            AMM::AerodromeSlipstreamPool(p) => (p.address, p),
            _ => unreachable!("only slipstream pools are passed in"),
        })
        .collect();

    for addr in TARGET_POOLS {
        assert!(
            pools.contains_key(&addr),
            "pool missing after init_batch: {addr}"
        );
    }

    let init_block = provider
        .get_block(BlockId::from(START_BLOCK))
        .await?
        .ok_or_else(|| eyre::eyre!("block {START_BLOCK} not found"))?;
    let init_ts = init_block.header.timestamp as u32;

    for addr in TARGET_POOLS {
        let pool = pools.get(&addr).expect("pool must exist");
        assert_eq!(
            pool.dynamic_fee_config.base_fee, 0,
            "pool {addr} expected base_fee=0, got {}",
            pool.dynamic_fee_config.base_fee
        );

        let local_fee = pool.compute_fee(init_ts);
        let rpc_fee = ICLPool::new(addr, provider.clone())
            .fee()
            .block(BlockId::from(START_BLOCK))
            .call()
            .await?
            .to::<u32>();

        assert_eq!(
            local_fee, rpc_fee,
            "init block fee mismatch: pool={addr} block={START_BLOCK} local={local_fee} rpc={rpc_fee} tick={} factory_tick_spacing_fee={} dfc={:?}",
            pool.tick,
            pool.factory_tick_spacing_fee,
            pool.dynamic_fee_config,
        );
    }

    let end_block = START_BLOCK + REPLAY_BLOCKS;
    let addresses: Vec<Address> = TARGET_POOLS.into_iter().collect();
    let sync_topics = pools
        .values()
        .next()
        .map(|pool| {
            let amm = AMM::AerodromeSlipstreamPool(pool.clone());
            amm.sync_events()
        })
        .unwrap_or_default();

    let mut logs = provider
        .get_logs(
            &Filter::new()
                .address(addresses)
                .event_signature(sync_topics)
                .from_block(START_BLOCK + 1)
                .to_block(end_block),
        )
        .await?;

    logs.sort_by(|a, b| {
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
        a.log_index.unwrap_or(0).cmp(&b.log_index.unwrap_or(0))
    });

    for block_num in (START_BLOCK + 1)..=end_block {
        for log in logs
            .iter()
            .filter(|l| l.block_number.unwrap_or(0) == block_num)
        {
            let addr = log.address();
            let Some(pool) = pools.get_mut(&addr) else {
                continue;
            };
            let mut amm = AMM::AerodromeSlipstreamPool(pool.clone());
            amm.sync(log)?;
            if let AMM::AerodromeSlipstreamPool(updated) = amm {
                *pool = updated;
            }
        }

        let block = provider
            .get_block(BlockId::from(block_num))
            .await?
            .ok_or_else(|| eyre::eyre!("block {block_num} not found"))?;
        let ts = block.header.timestamp as u32;

        for addr in TARGET_POOLS {
            let pool = pools.get(&addr).expect("pool must exist");
            let local_fee = pool.compute_fee(ts);
            let rpc_fee = ICLPool::new(addr, provider.clone())
                .fee()
                .block(BlockId::from(block_num))
                .call()
                .await?
                .to::<u32>();

            assert_eq!(
                local_fee, rpc_fee,
                "replay fee mismatch: pool={addr} block={block_num} local={local_fee} rpc={rpc_fee} tick={} factory_tick_spacing_fee={} dfc={:?} last_obs_ts={:?}",
                pool.tick,
                pool.factory_tick_spacing_fee,
                pool.dynamic_fee_config,
                pool.observations_cache.last().map(|o| o.block_timestamp),
            );
        }
    }

    Ok(())
}

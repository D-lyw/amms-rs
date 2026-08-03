use std::env;

use alloy::{
    eips::BlockId,
    primitives::{address, Address},
    providers::{Provider, ProviderBuilder},
};
use amms::amms::{
    amm::AutomatedMarketMaker,
    curve_legacy::{CurveLegacyPool, CurveLegacyPoolType},
};
use eyre::Result;

#[derive(Clone, Copy)]
struct LegacyPoolSpec {
    name: &'static str,
    address: Address,
    expected_family: CurveLegacyPoolType,
    expected_is_meta: bool,
}

const POOL_INDEX_LEGACY_ETHEREUM: &[LegacyPoolSpec] = &[
    LegacyPoolSpec {
        name: "Factory Stable 0x59ab",
        address: address!("59ab5a5b5d617e478a2479b0cad80da7e2831492"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: false,
    },
    LegacyPoolSpec {
        name: "LDO/USDC",
        address: address!("3211c6cbef1429da3d0d58494938299c92ad5860"),
        expected_family: CurveLegacyPoolType::CryptoSwap,
        expected_is_meta: false,
    },
    LegacyPoolSpec {
        name: "WETH/cbETH",
        address: address!("5fae7e604fc3e24fd43a72867cebac94c65b404a"),
        expected_family: CurveLegacyPoolType::CryptoSwap,
        expected_is_meta: false,
    },
    LegacyPoolSpec {
        name: "WETH/rETH",
        address: address!("0f3159811670c117c372428d4e69ac32325e4d0f"),
        expected_family: CurveLegacyPoolType::CryptoSwap,
        expected_is_meta: false,
    },
    LegacyPoolSpec {
        name: "Factory Stable 0x447d",
        address: address!("447ddd4960d9fdbf6af9a790560d0af76795cb08"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: false,
    },
    LegacyPoolSpec {
        name: "Tricrypto2",
        address: address!("d51a44d3fae010294c616388b506acda1bfaae46"),
        expected_family: CurveLegacyPoolType::CryptoSwap,
        expected_is_meta: false,
    },
    LegacyPoolSpec {
        name: "3pool",
        address: address!("bebc44782c7db0a1a60cb6fe97d0b483032ff1c7"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: false,
    },
    LegacyPoolSpec {
        name: "Old Tricrypto",
        address: address!("80466c64868e1ab14a1ddf27a676c3fcbe638fe5"),
        expected_family: CurveLegacyPoolType::CryptoSwap,
        expected_is_meta: false,
    },
    LegacyPoolSpec {
        name: "FRAX/USDC",
        address: address!("dcef968d416a41cdac0ed8702fac8128a64241a2"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: false,
    },
];

fn ethereum_provider_url() -> Option<String> {
    dotenv::dotenv().ok();
    env::var("ETHEREUM_PROVIDER")
        .or_else(|_| env::var("ETH_RPC_URL"))
        .or_else(|_| env::var("MAINNET_RPC_URL"))
        .ok()
        .or_else(|| Some("https://ethereum.publicnode.com".to_string()))
}

async fn resolve_fork_block<P: Provider>(provider: &P) -> Result<u64> {
    if let Ok(raw) = env::var("CURVE_LEGACY_POOL_INDEX_FORK_BLOCK") {
        return Ok(raw.parse::<u64>()?);
    }
    Ok(provider.get_block_number().await?)
}

#[tokio::test]
async fn test_curve_legacy_pool_index_ethereum_matrix() -> Result<()> {
    let Some(rpc_url) = ethereum_provider_url() else {
        println!("Skipping: ETHEREUM_PROVIDER/ETH_RPC_URL not set");
        return Ok(());
    };

    let upstream = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let fork_block = resolve_fork_block(&upstream).await?;
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let block_id = BlockId::number(fork_block);

    for spec in POOL_INDEX_LEGACY_ETHEREUM {
        let pool = CurveLegacyPool::new(spec.address, spec.expected_family)
            .init(block_id, provider.clone())
            .await?;

        println!(
            "pool_index legacy matrix: {} {} family={:?} stable_type={:?} is_meta={} coins={} underlying={} base_pool={:?} base_lp={:?}",
            spec.name,
            spec.address,
            pool.pool_type,
            pool.stable_type,
            pool.is_meta_pool(),
            pool.coins.len(),
            pool.underlying_coins.len(),
            pool.base_pool_address,
            pool.base_lp_token,
        );

        assert_eq!(pool.pool_type, spec.expected_family);
        assert_eq!(pool.is_meta_pool(), spec.expected_is_meta);
    }

    Ok(())
}

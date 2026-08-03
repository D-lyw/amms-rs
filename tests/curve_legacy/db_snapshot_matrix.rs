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
use eyre::{eyre, Result};

#[derive(Clone, Copy)]
struct MatrixPoolSpec {
    address: Address,
    expected_family: CurveLegacyPoolType,
    expected_is_meta: bool,
    zap_address: Option<Address>,
}

const ETH_LEGACY_META_POOLS: &[MatrixPoolSpec] = &[
    MatrixPoolSpec {
        address: address!("04b727c7e246ca70d496ecf52e6b6280f3c8077d"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("071c661b4deefb59e2a3ddb20db036821eee8f4b"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("0f9cb53ebe405d49a0bbdbd291a65ff571bc83e1"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("3e01dd8a5e1fb3481f0f589056b428fc308af0fb"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("42d7025938bec20b69cbae5a77421082407f053a"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("43b4fdfd4ff969587185cdb6f0bd875c5fc83f8c"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("4807862aa8b2bf68830e4c8dc86d0e9a998e085a"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("4f062658eaaf2c1ccf8c8e36d6824cdf41167956"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("5a6a4d54456819380173272a5e8e9b9904bdf41b"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("618788357d0ebd8a37e763adab3bc575d54c2c7d"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("8474ddbe98f5aa3179b3b3f5942d724afcdec9f6"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("84c333e94aea4a51a21f6cf0c7f528c50dc7592c"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("aeda92e6a3b1028edc139a4ae56ec881f3064d4f"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("b30da2376f63de30b42dc055c93fa474f31330a5"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("c25099792e9349c7dd09759744ea681c7de2cb66"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("c3b19502f8c02be75f3f77fd673503520deb51dd"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("d632f22692fac7611d2aa1c0d552930d43caed3b"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("d81da8d904b52208541bade1bd6595d8a251f8dd"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("e7a24ef0c5e95ffb0f6684b813a78f2a3ad7d171"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("ed279fdd11ca84beef15af5d39bb4d4bee23f0ca"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("31c325a01861c7dbd331a9270296a31296d797a0"),
        expected_family: CurveLegacyPoolType::CryptoSwap,
        expected_is_meta: true,
        zap_address: Some(address!("5de4ef4879f4fe3bbadf2227d2ac5d0e2d76c895")),
    },
    MatrixPoolSpec {
        address: address!("4149d1038575ce235e03e03b39487a80fd709d31"),
        expected_family: CurveLegacyPoolType::CryptoSwap,
        expected_is_meta: true,
        zap_address: Some(address!("5de4ef4879f4fe3bbadf2227d2ac5d0e2d76c895")),
    },
    MatrixPoolSpec {
        address: address!("af4264916b467e2c9c8acf07acc22b9edddadf33"),
        expected_family: CurveLegacyPoolType::CryptoSwap,
        expected_is_meta: true,
        zap_address: Some(address!("5de4ef4879f4fe3bbadf2227d2ac5d0e2d76c895")),
    },
    MatrixPoolSpec {
        address: address!("bec570d92afb7ffc553bdd9d4b4638121000b10d"),
        expected_family: CurveLegacyPoolType::CryptoSwap,
        expected_is_meta: true,
        zap_address: Some(address!("5de4ef4879f4fe3bbadf2227d2ac5d0e2d76c895")),
    },
    MatrixPoolSpec {
        address: address!("fc1e8bf3e81383ef07be24c3fd146745719de48d"),
        expected_family: CurveLegacyPoolType::CryptoSwap,
        expected_is_meta: true,
        zap_address: Some(address!("5de4ef4879f4fe3bbadf2227d2ac5d0e2d76c895")),
    },
];

const ARBITRUM_LEGACY_MATRIX: &[MatrixPoolSpec] = &[
    MatrixPoolSpec {
        address: address!("960ea3e3c7fb317332d990873d354e18d7645590"),
        expected_family: CurveLegacyPoolType::CryptoSwap,
        expected_is_meta: false,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("a827a652ead76c6b0b3d19dba05452e06e25c27e"),
        expected_family: CurveLegacyPoolType::CryptoSwap,
        expected_is_meta: true,
        zap_address: Some(address!("25e2e8d104bc1a70492e2be32da7c1f8367f9d2c")),
    },
    MatrixPoolSpec {
        address: address!("1deb3b1ca6afca0ff9c5ce9301950dc98ac0d523"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: false,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("3e01dd8a5e1fb3481f0f589056b428fc308af0fb"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: false,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("59bf0545fca0e5ad48e13da269facd2e8c886ba4"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: false,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("6eb2dc694eb516b16dc9fbc678c60052bbdd7d80"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: false,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("7f90122bf0700f9e7e1f688fe926940e8839f353"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: false,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("c9b8a3fdecb9d5b218d02555a8baf332e5b740d5"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: false,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("30df229cefa463e991e29d42db0bae2e122b2ac7"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("b34a7d1444a707349bc7b981b7f2e1f20f81f013"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("d2239b95890018a8f52ffd17d7f94c3a82f05389"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: true,
        zap_address: None,
    },
];

const BASE_LEGACY_PLAIN_POOLS: &[MatrixPoolSpec] = &[
    MatrixPoolSpec {
        address: address!("11c1fbd4b3de66bc0565779b35171a6cf3e71f59"),
        expected_family: CurveLegacyPoolType::CryptoSwap,
        expected_is_meta: false,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("de37e221442fa15c35dc19fbae11ed106ba52fb2"),
        expected_family: CurveLegacyPoolType::CryptoSwap,
        expected_is_meta: false,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("f6c5f01c7f3148891ad0e19df78743d31e390d1f"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: false,
        zap_address: None,
    },
];

// Database snapshot currently tags these pools as LegacyStableMeta, but on-chain they behave as plain
// StableSwap pools: no expanded underlying, no base_pool() route, and our init resolves them as non-meta.
const ETH_DB_META_LABEL_ANOMALIES: &[MatrixPoolSpec] = &[
    MatrixPoolSpec {
        address: address!("06cb22615ba53e60d67bf6c341a0fd5e718e1655"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: false,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("5b3b5df2bf2b6543f78e053bd91c4bdd820929f1"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: false,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("87650d7bbfc3a9f10587d7778206671719d9910d"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: false,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("892d701d94a43bdbcb5ea28891daca2fa22a690b"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: false,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("bcb91e689114b9cc865ad7871845c95241df4105"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: false,
        zap_address: None,
    },
    MatrixPoolSpec {
        address: address!("fbdca68601f835b27790d98bbb8ec7f05fdeaa9b"),
        expected_family: CurveLegacyPoolType::StableSwap,
        expected_is_meta: false,
        zap_address: None,
    },
];

fn ethereum_provider_urls() -> Vec<String> {
    dotenv::dotenv().ok();
    let mut urls = Vec::new();
    for value in [
        env::var("ETHEREUM_PROVIDER")
            .or_else(|_| env::var("ETH_RPC_URL"))
            .or_else(|_| env::var("MAINNET_RPC_URL"))
            .ok(),
        Some("https://ethereum.publicnode.com".to_string()),
    ] {
        if let Some(url) = value {
            if !urls.contains(&url) {
                urls.push(url);
            }
        }
    }
    urls
}

fn arbitrum_provider_urls() -> Vec<String> {
    dotenv::dotenv().ok();
    let mut urls = Vec::new();
    for value in [
        env::var("ARBITRUM_PROVIDER")
            .or_else(|_| env::var("ARB_RPC_URL"))
            .ok(),
        Some("https://arb1.arbitrum.io/rpc".to_string()),
    ] {
        if let Some(url) = value {
            if !urls.contains(&url) {
                urls.push(url);
            }
        }
    }
    urls
}

fn base_provider_urls() -> Vec<String> {
    dotenv::dotenv().ok();
    let mut urls = Vec::new();
    for value in [
        env::var("BASE_PROVIDER").ok(),
        Some("https://mainnet.base.org".to_string()),
    ] {
        if let Some(url) = value {
            if !urls.contains(&url) {
                urls.push(url);
            }
        }
    }
    urls
}

async fn connect_first_working(urls: &[String]) -> Result<(String, u64)> {
    let mut last_error = None;

    for url in urls {
        let provider = ProviderBuilder::new().connect_http(url.parse()?);
        match provider.get_block_number().await {
            Ok(block) => return Ok((url.clone(), block)),
            Err(error) => last_error = Some(format!("{} => {}", url, error)),
        }
    }

    Err(eyre!(
        "all RPC candidates failed: {}",
        last_error.unwrap_or_else(|| "no RPC URL candidates".to_string())
    ))
}

async fn run_matrix(
    label: &str,
    specs: &[MatrixPoolSpec],
    block_id: BlockId,
    provider: impl Provider + Clone + 'static,
) -> Result<()> {
    let mut mismatches = Vec::new();

    for &spec in specs {
        let mut pool = CurveLegacyPool::new(spec.address, spec.expected_family);
        pool.zap_address = spec.zap_address;
        let pool = pool
            .init(block_id, provider.clone())
            .await
            .map_err(|e| eyre!("{} init failed for {:?}: {}", label, spec.address, e))?;
        println!(
            "{} matrix: pool={} family={:?} stable_type={:?} is_meta={} direct={} underlying={} base_pool={:?}",
            label,
            spec.address,
            pool.pool_type,
            pool.stable_type,
            pool.is_meta_pool(),
            pool.coins.len(),
            pool.underlying_coins.len(),
            pool.base_pool_address,
        );

        if pool.pool_type != spec.expected_family {
            mismatches.push(format!(
                "pool {:?}: expected family {:?}, got {:?}",
                spec.address, spec.expected_family, pool.pool_type
            ));
        }
        if pool.is_meta_pool() != spec.expected_is_meta {
            mismatches.push(format!(
                "pool {:?}: expected is_meta={}, got {}",
                spec.address,
                spec.expected_is_meta,
                pool.is_meta_pool()
            ));
        }

        if spec.expected_is_meta {
            if pool.underlying_coins.len() <= pool.coins.len() {
                mismatches.push(format!(
                    "pool {:?}: expected expanded underlying coins, direct={} underlying={}",
                    spec.address,
                    pool.coins.len(),
                    pool.underlying_coins.len()
                ));
            }
            if pool.base_pool_address.is_none() {
                mismatches.push(format!(
                    "pool {:?}: expected base_pool_address",
                    spec.address
                ));
            }
            if pool.base_lp_token.is_none() {
                mismatches.push(format!("pool {:?}: expected base_lp_token", spec.address));
            }
            if pool.base_pool_view.is_none() {
                mismatches.push(format!("pool {:?}: expected base_pool_view", spec.address));
            }
        } else if pool.base_pool_address.is_some() {
            mismatches.push(format!(
                "pool {:?}: expected plain pool without base_pool_address, got {:?}",
                spec.address, pool.base_pool_address
            ));
        }
    }

    if !mismatches.is_empty() {
        return Err(eyre!(
            "{} matrix mismatches:\n{}",
            label,
            mismatches.join("\n")
        ));
    }

    Ok(())
}

#[tokio::test]
async fn test_curve_legacy_ethereum_meta_matrix_from_db_snapshot() -> Result<()> {
    let rpc_urls = ethereum_provider_urls();
    if rpc_urls.is_empty() {
        println!("Skipping: ETHEREUM_PROVIDER/ETH_RPC_URL not set");
        return Ok(());
    }

    let (rpc_url, latest_block) = connect_first_working(&rpc_urls).await?;
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let fork_block = if let Ok(raw) = env::var("CURVE_LEGACY_ETH_DB_MATRIX_BLOCK") {
        raw.parse::<u64>()?
    } else {
        latest_block
    };

    run_matrix(
        "ethereum-meta",
        ETH_LEGACY_META_POOLS,
        BlockId::number(fork_block),
        provider,
    )
    .await
}

#[tokio::test]
async fn test_curve_legacy_arbitrum_matrix_from_db_snapshot() -> Result<()> {
    let rpc_urls = arbitrum_provider_urls();
    if rpc_urls.is_empty() {
        println!("Skipping: ARBITRUM_PROVIDER/ARB_RPC_URL not set");
        return Ok(());
    }

    let (rpc_url, latest_block) = connect_first_working(&rpc_urls).await?;
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let fork_block = if let Ok(raw) = env::var("CURVE_LEGACY_ARB_DB_MATRIX_BLOCK") {
        raw.parse::<u64>()?
    } else {
        latest_block
    };

    run_matrix(
        "arbitrum-legacy",
        ARBITRUM_LEGACY_MATRIX,
        BlockId::number(fork_block),
        provider,
    )
    .await
}

#[tokio::test]
async fn test_curve_legacy_base_plain_matrix_from_db_snapshot() -> Result<()> {
    let rpc_urls = base_provider_urls();
    if rpc_urls.is_empty() {
        println!("Skipping: BASE_PROVIDER not set");
        return Ok(());
    }

    let (rpc_url, latest_block) = connect_first_working(&rpc_urls).await?;
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let fork_block = if let Ok(raw) = env::var("CURVE_LEGACY_BASE_DB_MATRIX_BLOCK") {
        raw.parse::<u64>()?
    } else {
        latest_block
    };

    run_matrix(
        "base-legacy-plain",
        BASE_LEGACY_PLAIN_POOLS,
        BlockId::number(fork_block),
        provider,
    )
    .await
}

#[tokio::test]
async fn test_curve_legacy_ethereum_db_meta_label_anomalies() -> Result<()> {
    let rpc_urls = ethereum_provider_urls();
    if rpc_urls.is_empty() {
        println!("Skipping: ETHEREUM_PROVIDER/ETH_RPC_URL not set");
        return Ok(());
    }

    let (rpc_url, latest_block) = connect_first_working(&rpc_urls).await?;
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let fork_block = if let Ok(raw) = env::var("CURVE_LEGACY_ETH_DB_MATRIX_BLOCK") {
        raw.parse::<u64>()?
    } else {
        latest_block
    };

    run_matrix(
        "ethereum-db-anomaly",
        ETH_DB_META_LABEL_ANOMALIES,
        BlockId::number(fork_block),
        provider,
    )
    .await
}

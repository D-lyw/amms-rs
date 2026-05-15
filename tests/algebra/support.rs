use alloy::primitives::{address, Address, U256};

pub const QUICKSWAP_BASE_QUOTER_V2: Address = address!("23e0583a3a000d567bb3848115065c1890d87fb5");
pub const HYDREX_BASE_QUOTER_V2: Address = address!("08b46265643a5389529d6f6616fa4a0d66f13fdb");

// ── Existing test pools ──────────────────────────────────────────────
pub const QUICKSWAP_V4_WETH_USDC_POOL: Address =
    address!("5a9Ad2BB92B0B3E5C571FDD5125114E04E02be1a");
pub const QUICKSWAP_V4_LINK_ETH_POOL: Address =
    address!("603F3FD0247e5A444a561D7DA081c3b00fcF7De9");
pub const QUICKSWAP_V4_CBBTC_USDC_POOL: Address =
    address!("aCc2874ed22e811afdc47979c7b7985cCEd53b29");

pub const HYDREX_WETH_CBBTC_POOL: Address = address!("3f9b863EF4B295d6Ba370215bcCa3785FCC44f44");
pub const HYDREX_KVCM_USDC_POOL: Address = address!("Ef96Ec76eEB36584FC4922e9fA268e0780170f33");

// ── New Quickswap V4 pools (by descending liquidity) ─────────────────
pub const QSV4_CBETH_WETH: Address = address!("74a4bf2cad1b172cdc82a5da7130c7a59eb5460b");
pub const QSV4_WETH_WSTETH: Address = address!("03122fc0c902d6fe9cf019e16a406eac8efa7fdd");
pub const QSV4_WETH_CBBTC: Address = address!("d800b7e8c0949dffbd59e3df9527a22e311c0e7b");
pub const QSV4_USDC_USDS: Address = address!("5d0bc342178c8fe2c2f9a9fcc9d52555c99936db");
pub const QSV4_WETH_WOETH: Address = address!("be50cc9cf0905cac4f8f8453bb6c9798898658bd");
pub const QSV4_WETH_USBC: Address = address!("e716634679af01f6511df3facf130a130927ee8c");

// ── New Hydrex pools (by descending liquidity) ───────────────────────
pub const HYDREX_WETH_USDC: Address = address!("82dbe18346a8656dbb5e76f74bf3ae279cc16b29");
pub const HYDREX_WEETH_WETH: Address = address!("1c419ac8fbaf8082eb7276fc2f243a74daf1c927");

pub const ALGEBRA_DRIFT_FROM_BLOCK: u64 = 45_504_973;
pub const ALGEBRA_DRIFT_TO_BLOCK: u64 = 45_510_973;
pub const ALGEBRA_COMPARE_BLOCK: u64 = 45_510_973;

#[derive(Clone, Copy)]
pub struct PoolCase {
    pub label: &'static str,
    pub pool: Address,
    pub quoter: Address,
    pub deployer: Address,
}

pub fn algebra_cases() -> Vec<PoolCase> {
    vec![
        // ── Existing ──────────────────────────────────────────────
        PoolCase {
            label: "QuickSwap V4 WETH-USDC",
            pool: QUICKSWAP_V4_WETH_USDC_POOL,
            quoter: QUICKSWAP_BASE_QUOTER_V2,
            deployer: Address::ZERO,
        },
        PoolCase {
            label: "QuickSwap V4 LINK-ETH",
            pool: QUICKSWAP_V4_LINK_ETH_POOL,
            quoter: QUICKSWAP_BASE_QUOTER_V2,
            deployer: Address::ZERO,
        },
        PoolCase {
            label: "QuickSwap V4 cbBTC-USDC",
            pool: QUICKSWAP_V4_CBBTC_USDC_POOL,
            quoter: QUICKSWAP_BASE_QUOTER_V2,
            deployer: Address::ZERO,
        },
        PoolCase {
            label: "Hydrex WETH-cbBTC",
            pool: HYDREX_WETH_CBBTC_POOL,
            quoter: HYDREX_BASE_QUOTER_V2,
            deployer: Address::ZERO,
        },
        PoolCase {
            label: "Hydrex kVCM-USDC",
            pool: HYDREX_KVCM_USDC_POOL,
            quoter: HYDREX_BASE_QUOTER_V2,
            deployer: Address::ZERO,
        },
        // ── New Quickswap V4 ──────────────────────────────────────
        PoolCase {
            label: "QSV4 cbETH-WETH",
            pool: QSV4_CBETH_WETH,
            quoter: QUICKSWAP_BASE_QUOTER_V2,
            deployer: Address::ZERO,
        },
        PoolCase {
            label: "QSV4 WETH-wstETH",
            pool: QSV4_WETH_WSTETH,
            quoter: QUICKSWAP_BASE_QUOTER_V2,
            deployer: Address::ZERO,
        },
        PoolCase {
            label: "QSV4 WETH-cbBTC",
            pool: QSV4_WETH_CBBTC,
            quoter: QUICKSWAP_BASE_QUOTER_V2,
            deployer: Address::ZERO,
        },
        PoolCase {
            label: "QSV4 USDC-USDS",
            pool: QSV4_USDC_USDS,
            quoter: QUICKSWAP_BASE_QUOTER_V2,
            deployer: Address::ZERO,
        },
        PoolCase {
            label: "QSV4 WETH-woETH",
            pool: QSV4_WETH_WOETH,
            quoter: QUICKSWAP_BASE_QUOTER_V2,
            deployer: Address::ZERO,
        },
        PoolCase {
            label: "QSV4 WETH-USBC",
            pool: QSV4_WETH_USBC,
            quoter: QUICKSWAP_BASE_QUOTER_V2,
            deployer: Address::ZERO,
        },
        // ── New Hydrex ────────────────────────────────────────────
        PoolCase {
            label: "Hydrex WETH-USDC",
            pool: HYDREX_WETH_USDC,
            quoter: HYDREX_BASE_QUOTER_V2,
            deployer: Address::ZERO,
        },
        PoolCase {
            label: "Hydrex weETH-WETH",
            pool: HYDREX_WEETH_WETH,
            quoter: HYDREX_BASE_QUOTER_V2,
            deployer: Address::ZERO,
        },
    ]
}

pub fn provider_url_for_base() -> Option<String> {
    dotenv::dotenv().ok();
    std::env::var("BASE_PROVIDER")
        .or_else(|_| std::env::var("BASE_RPC_URL"))
        .or_else(|_| std::env::var("ETHEREUM_PROVIDER"))
        .or_else(|_| std::env::var("ETHEREUM_RPC_URL"))
        .ok()
        .filter(|v| !v.trim().is_empty())
}

pub fn bps_diff(lhs: U256, rhs: U256) -> U256 {
    if lhs.is_zero() && rhs.is_zero() {
        return U256::ZERO;
    }

    let maxv = if lhs > rhs { lhs } else { rhs };
    let minv = if lhs > rhs { rhs } else { lhs };
    if maxv.is_zero() {
        return U256::ZERO;
    }

    (maxv - minv) * U256::from(10_000u64) / maxv
}

fn at_least_one(v: U256) -> U256 {
    if v.is_zero() {
        U256::from(1u8)
    } else {
        v
    }
}

pub fn exact_in_amounts_by_decimals(decimals: u8) -> Vec<U256> {
    let one = U256::from(10u8).pow(U256::from(decimals));
    let thousand = U256::from(1_000u64);
    let hundred = U256::from(100u64);
    let ten = U256::from(10u64);
    let mut amounts = vec![
        at_least_one(one / thousand),
        at_least_one(one / hundred),
        at_least_one(one / ten),
        one,
        one * U256::from(5u64),
    ];
    if decimals > 8 {
        amounts.push(one * U256::from(20u64));
        amounts.push(one * U256::from(100u64));
        amounts.push(one * U256::from(500u64));
    }
    amounts
}

pub fn exact_out_amounts_by_decimals(decimals: u8) -> Vec<U256> {
    let one = U256::from(10u8).pow(U256::from(decimals));
    let thousand = U256::from(1_000u64);
    let hundred = U256::from(100u64);
    let ten = U256::from(10u64);

    vec![
        at_least_one(one / thousand),
        at_least_one(one / hundred),
        at_least_one(one / ten),
        one,
        one * U256::from(3u64),
        one * U256::from(10u64),
        one * U256::from(30u64),
        one * U256::from(100u64),
    ]
}

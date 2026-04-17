use std::{str::FromStr, sync::Arc};

use alloy::{
    primitives::{
        address,
        aliases::{I24, U24},
        Address, U256,
    },
    providers::ProviderBuilder,
    rpc::client::ClientBuilder,
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::{
    amms::{
        amm::{AutomatedMarketMaker, AMM},
        uniswap_v4::{IPoolManager::PoolKey, UniswapV4Pool},
    },
    state_space::StateSpaceBuilder,
};

fn parse_env_address(key: &str, default: Address) -> Address {
    std::env::var(key)
        .ok()
        .and_then(|v| Address::from_str(v.trim()).ok())
        .unwrap_or(default)
}

fn parse_env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(default)
}

fn parse_env_i32(key: &str, default: i32) -> i32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<i32>().ok())
        .unwrap_or(default)
}

fn format_amount(amount: U256, decimals: u8) -> String {
    if decimals == 0 {
        return amount.to_string();
    }
    let s = amount.to_string();
    let d = decimals as usize;
    if s.len() <= d {
        format!("0.{}{}", "0".repeat(d - s.len()), s)
    } else {
        let split = s.len() - d;
        format!("{}.{}", &s[..split], &s[split..])
    }
}

fn small_amount(decimals: u8) -> U256 {
    // 0.001 token
    if decimals >= 3 {
        U256::from(10u8).pow(U256::from(decimals - 3))
    } else {
        U256::from(1u8)
    }
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(true)
        .init();

    // Supports both production env naming and generic RPC_URL.
    let rpc_endpoint = std::env::var("BASE_PROVIDER")
        .or_else(|_| std::env::var("RPC_URL"))
        .or_else(|_| std::env::var("ETHEREUM_PROVIDER"))
        .or_else(|_| std::env::var("MAINNET_RPC_URL"))
        .expect("Please set BASE_PROVIDER / RPC_URL / ETHEREUM_PROVIDER / MAINNET_RPC_URL");

    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(500))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(rpc_endpoint.parse()?);
    let provider = Arc::new(ProviderBuilder::new().connect_client(client));

    // Default target = Base: native ETH / USDC, fee 2500, tick spacing 50.
    // The corresponding virtual pool address is:
    // 0xbc158a569b62211e27b358581ff1420df0e5c120
    let manager_address = parse_env_address(
        "POOL_MANAGER",
        address!("498581ff718922c3f8e6a244956af099b2652b2b"),
    );
    let token0 = parse_env_address(
        "TOKEN0",
        address!("0000000000000000000000000000000000000000"),
    );
    let token1 = parse_env_address(
        "TOKEN1",
        address!("833589fcd6edb6e08f4c7c32d4f71b54bda02913"),
    );
    let fee = parse_env_u32("FEE", 2500);
    let tick_spacing = parse_env_i32("TICK_SPACING", 50);
    let hooks = parse_env_address(
        "HOOKS",
        address!("0000000000000000000000000000000000000000"),
    );

    let pool_key = PoolKey {
        currency0: token0,
        currency1: token1,
        fee: U24::from(fee as u64),
        tickSpacing: I24::from_str(&tick_spacing.to_string()).unwrap_or(I24::ZERO),
        hooks,
    };
    let probe_pool = UniswapV4Pool::new(manager_address, pool_key.clone());
    let expected_pool_id = probe_pool.pool_id;
    let expected_virtual_addr = probe_pool.address();

    println!("=== INPUT ===");
    println!("manager={:?}", manager_address);
    println!(
        "pool_key=(token0={:?}, token1={:?}, fee={}, tick_spacing={}, hooks={:?})",
        token0, token1, fee, tick_spacing, hooks
    );
    println!("expected_pool_id={:?}", expected_pool_id);
    println!("expected_virtual_address={:?}", expected_virtual_addr);

    let amms: Vec<AMM> = vec![AMM::UniswapV4Pool(probe_pool.clone())];

    println!("\n=== BATCH INIT (production path) ===");
    println!("StateSpaceBuilder::new(provider).with_amms(...).sync().await");
    let state_space_manager = StateSpaceBuilder::new(provider.clone())
        .with_amms(amms)
        .sync()
        .await?;

    let state_guard = state_space_manager.state.read().await;
    println!("state_size={}", state_guard.state.len());

    let maybe_pool = state_guard
        .state
        .get(&expected_virtual_addr)
        .and_then(|amm| match amm {
            AMM::UniswapV4Pool(p) => Some(p.clone()),
            _ => None,
        });

    if maybe_pool.is_none() {
        println!("\n[RESULT] POOL NOT FOUND AFTER INIT");
        println!(
            "The pool is not retained in state. It was filtered during init (structural invalid or dust)."
        );
        println!("Available V4 pools in state:");
        for amm in state_guard.state.values() {
            if let AMM::UniswapV4Pool(p) = amm {
                println!(
                    "  v4_addr={:?} pool_id={:?} liq={} ticks={}",
                    p.address(),
                    p.pool_id,
                    p.liquidity,
                    p.ticks.len()
                );
            }
        }
        return Ok(());
    }

    let pool = maybe_pool.expect("checked above");
    println!("\n[RESULT] POOL RETAINED");
    println!("pool_id={:?}", pool.pool_id);
    println!(
        "token0={:?} decimals={}, token1={:?} decimals={}",
        pool.token_a.address, pool.token_a.decimals, pool.token_b.address, pool.token_b.decimals
    );
    println!(
        "tick={} sqrt_price={} liquidity={} tick_words={} tick_entries={}",
        pool.tick,
        pool.sqrt_price,
        pool.liquidity,
        pool.tick_bitmap.len(),
        pool.ticks.len()
    );

    println!("\n=== SWAP SIMULATION ===");
    let amount_in_0 = small_amount(pool.token_a.decimals);
    let amount_in_1 = small_amount(pool.token_b.decimals);

    match pool.simulate_swap(pool.token_a.address, pool.token_b.address, amount_in_0) {
        Ok(out) => println!(
            "ExactIn token0->token1: in={} (raw={}) out={} (raw={})",
            format_amount(amount_in_0, pool.token_a.decimals),
            amount_in_0,
            format_amount(out, pool.token_b.decimals),
            out
        ),
        Err(e) => println!(
            "ExactIn token0->token1: in={} (raw={}) error={}",
            format_amount(amount_in_0, pool.token_a.decimals),
            amount_in_0,
            e
        ),
    }

    match pool.simulate_swap(pool.token_b.address, pool.token_a.address, amount_in_1) {
        Ok(out) => println!(
            "ExactIn token1->token0: in={} (raw={}) out={} (raw={})",
            format_amount(amount_in_1, pool.token_b.decimals),
            amount_in_1,
            format_amount(out, pool.token_a.decimals),
            out
        ),
        Err(e) => println!(
            "ExactIn token1->token0: in={} (raw={}) error={}",
            format_amount(amount_in_1, pool.token_b.decimals),
            amount_in_1,
            e
        ),
    }

    Ok(())
}

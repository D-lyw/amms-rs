use std::str::FromStr;
use std::sync::Arc;

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

fn create_v4_pool(
    token0: Address,
    token1: Address,
    fee: u32,
    tick_spacing: i32,
    hooks: Address,
    manager_address: Address,
) -> AMM {
    let pool_key = PoolKey {
        currency0: token0,
        currency1: token1,
        fee: U24::from(fee as u64),
        tickSpacing: I24::from_str(&tick_spacing.to_string()).unwrap_or(I24::ZERO),
        hooks,
    };
    UniswapV4Pool::new(manager_address, pool_key).into()
}

fn format_amount(amount: U256, decimals: u8) -> String {
    let s = amount.to_string();
    if decimals == 0 {
        return s;
    }
    let d = decimals as usize;
    if s.len() <= d {
        format!("0.{}{}", "0".repeat(d - s.len()), s)
    } else {
        let split = s.len() - d;
        format!("{}.{}", &s[..split], &s[split..])
    }
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .init();

    let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")
        .or_else(|_| std::env::var("MAINNET_RPC_URL"))
        .expect("Please set ETHEREUM_PROVIDER or MAINNET_RPC_URL");

    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(500))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(rpc_endpoint.parse()?);

    let provider = Arc::new(ProviderBuilder::new().connect_client(client));

    let manager_address = address!("000000000004444c5dc75cb358380d2e3de08a90");
    let hooks_zero = address!("0000000000000000000000000000000000000000");
    let token0 = address!("7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0"); // wstETH
    let token1 = address!("a1290d69c65a6fe4df752f95823fae25cb99e5a7"); // rsETH
    let fee = 475u32;
    let tick_spacing = 2i32;

    let v4_pools: Vec<AMM> = vec![create_v4_pool(
        token0,
        token1,
        fee,
        tick_spacing,
        hooks_zero,
        manager_address,
    )];

    println!("Initializing target UniswapV4 pool via StateSpaceBuilder with batch init...");
    let state_space_manager = StateSpaceBuilder::new(provider.clone())
        .with_amms(v4_pools.clone())
        .sync()
        .await?;

    let state_space = state_space_manager.state.read().await;
    println!("state_size={}", state_space.state.len());

    let pool = state_space
        .state
        .values()
        .find_map(|amm| match amm {
            AMM::UniswapV4Pool(p) => Some(p.clone()),
            _ => None,
        })
        .expect("Pool not found in state after sync");

    println!("Pool initialized:");
    println!("  pool_id={:?}", pool.pool_id);
    println!(
        "  token0={:?} decimals={}",
        pool.token_a.address, pool.token_a.decimals
    );
    println!(
        "  token1={:?} decimals={}",
        pool.token_b.address, pool.token_b.decimals
    );
    println!(
        "  tick={} sqrt_price={} liquidity={}",
        pool.tick, pool.sqrt_price, pool.liquidity
    );
    println!(
        "  tick_bitmap_words={} tick_entries={}",
        pool.tick_bitmap.len(),
        pool.ticks.len()
    );

    let amount_in_0 = U256::from(10u64).pow(U256::from(pool.token_a.decimals.saturating_sub(2))); // 0.01 token0
    let amount_in_1 = U256::from(10u64).pow(U256::from(pool.token_b.decimals.saturating_sub(2))); // 0.01 token1

    let out_0_to_1 = pool.simulate_swap(pool.token_a.address, pool.token_b.address, amount_in_0)?;
    let out_1_to_0 = pool.simulate_swap(pool.token_b.address, pool.token_a.address, amount_in_1)?;

    println!("\nExactIn simulations:");
    println!(
        "  token0 -> token1: in={} (raw={}) out={} (raw={})",
        format_amount(amount_in_0, pool.token_a.decimals),
        amount_in_0,
        format_amount(out_0_to_1, pool.token_b.decimals),
        out_0_to_1
    );
    println!(
        "  token1 -> token0: in={} (raw={}) out={} (raw={})",
        format_amount(amount_in_1, pool.token_b.decimals),
        amount_in_1,
        format_amount(out_1_to_0, pool.token_a.decimals),
        out_1_to_0
    );

    let target_out_1 = if out_0_to_1 > U256::ZERO {
        out_0_to_1 / U256::from(10u64)
    } else {
        U256::from(10u64).pow(U256::from(pool.token_b.decimals.saturating_sub(6)))
    };
    let target_out_0 = if out_1_to_0 > U256::ZERO {
        out_1_to_0 / U256::from(10u64)
    } else {
        U256::from(10u64).pow(U256::from(pool.token_a.decimals.saturating_sub(6)))
    };

    println!("\nExactOut simulations:");
    match pool.simulate_swap_exact_out(pool.token_a.address, pool.token_b.address, target_out_1) {
        Ok(amount_in) => {
            println!(
                "  token0 -> token1: target_out={} (raw={}) required_in={} (raw={})",
                format_amount(target_out_1, pool.token_b.decimals),
                target_out_1,
                format_amount(amount_in, pool.token_a.decimals),
                amount_in
            );
        }
        Err(e) => {
            println!(
                "  token0 -> token1: target_out={} (raw={}) error={}",
                format_amount(target_out_1, pool.token_b.decimals),
                target_out_1,
                e
            );
        }
    }

    match pool.simulate_swap_exact_out(pool.token_b.address, pool.token_a.address, target_out_0) {
        Ok(amount_in) => {
            println!(
                "  token1 -> token0: target_out={} (raw={}) required_in={} (raw={})",
                format_amount(target_out_0, pool.token_a.decimals),
                target_out_0,
                format_amount(amount_in, pool.token_b.decimals),
                amount_in
            );
        }
        Err(e) => {
            println!(
                "  token1 -> token0: target_out={} (raw={}) error={}",
                format_amount(target_out_0, pool.token_a.decimals),
                target_out_0,
                e
            );
        }
    }

    Ok(())
}

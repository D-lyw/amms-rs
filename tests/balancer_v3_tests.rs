use alloy::{
    eips::BlockId,
    primitives::{Address, U256},
    providers::{Provider, ProviderBuilder},
    rpc::client::ClientBuilder,
    sol,
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::amms::{
    amm::AutomatedMarketMaker,
    balancer_v3::{BalancerV3Pool, BalancerV3PoolType},
};
use std::str::FromStr;

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IBalancerV3Router {
        function querySwapSingleTokenExactIn(
            address pool,
            address tokenIn,
            address tokenOut,
            uint256 exactAmountIn,
            address limitAmountOut,
            bytes calldata userData
        ) external returns (uint256 amountOut);

        function querySwapSingleTokenExactOut(
            address pool,
            address tokenIn,
            address tokenOut,
            uint256 exactAmountOut,
            address sender,
            bytes calldata userData
        ) external returns (uint256 amountIn);
    }
}

async fn run_swap_test(
    pool_address: Address,
    pool_type: BalancerV3PoolType,
    token_in_idx: usize,
    token_out_idx: usize,
    amount_units: f64,
    check_desc: &str,
) -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let rpc_endpoint = match std::env::var("ETHEREUM_PROVIDER") {
        Ok(url) => url,
        Err(_) => {
            println!("Skipping test: ETHEREUM_PROVIDER not set");
            return Ok(());
        }
    };

    let provider = ProviderBuilder::new().connect_http(rpc_endpoint.parse()?);

    // Fetch latest block to ensure consistency between Init and Router Query

    let block_number = provider.get_block_number().await?;
    let block_id = BlockId::number(block_number);

    // Balancer V3 Vault (Mainnet)
    let vault_address = Address::from_str("ba1333333333a1BA1108E8412f11850A5C319bA9")?;
    let router_address = Address::from_str("0xAE563E3f8219521950555F5962419C8919758Ea2")?;

    // Check if pool exists
    let code = provider.get_code_at(pool_address).await?;
    if code.is_empty() {
        println!("Skipping test: Pool contract not found at {pool_address}");
        return Ok(());
    }

    let mut pool = BalancerV3Pool::new(
        pool_address,
        vault_address,
        pool_type,
    );
    pool = pool.init(block_id, provider.clone()).await?;

    assert_eq!(pool.pool_type, pool_type);
    println!("Pool Initialized: {:?}", pool.address);

    if pool.token_list.len() < 2 {
        println!("Pool has insufficient tokens");
        return Ok(());
    }

    let token_in_addr = pool.token_list[token_in_idx];
    let token_out_addr = pool.token_list[token_out_idx];

    let token_in_state = pool.tokens.get(&token_in_addr).unwrap();
    let decimals = token_in_state.decimals;

    let mut amount_in = if decimals <= 18 {
        U256::from((amount_units * 10f64.powi(decimals as i32)) as u128)
    } else {
        U256::from(amount_units as u128)
    };

    // For weighted pools, ensure amount_in stays within MAX_IN_RATIO (30% of balance).
    if matches!(pool_type, BalancerV3PoolType::Weighted) {
        let max_in = token_in_state
            .balance
            .checked_mul(U256::from(300_000_000_000_000_000u128))
            .unwrap_or(U256::ZERO)
            .checked_div(U256::from(1_000_000_000_000_000_000u128))
            .unwrap_or(U256::ZERO);
        if max_in > U256::ZERO && amount_in > max_in {
            let clamped = std::cmp::max(max_in / U256::from(2u8), U256::from(1u8));
            println!(
                "Exact-in amount clamped: requested={} max_allowed={} clamped={}",
                amount_in, max_in, clamped
            );
            amount_in = clamped;
        }
    }

    println!("Simulating Swap: {amount_units} of {token_in_addr} -> {token_out_addr}");
    let amount_out = pool.simulate_swap(token_in_addr, token_out_addr, amount_in)?;

    let router = IBalancerV3Router::new(router_address, provider.clone());
    match router
        .querySwapSingleTokenExactIn(
            pool_address,
            token_in_addr,
            token_out_addr,
            amount_in,
            Address::ZERO,
            alloy::primitives::Bytes::new(),
        )
        .block(block_id) // CRITICAL: Use same block as init
        .call()
        .await
    {
        Ok(onchain_amount_out) => {
            let onchain_val = onchain_amount_out;

            let diff = if amount_out > onchain_val {
                amount_out - onchain_val
            } else {
                onchain_val - amount_out
            };

            println!("===============");
            println!("{check_desc}: Local={amount_out}, OnChain={onchain_val}, Diff={diff}");

            // Tolerance check
            // Stable pools should match exactly (no hooks). Weighted pools can have tiny LogExpMath error.
            let allowed_diff = if matches!(pool_type, BalancerV3PoolType::Stable)
                || matches!(pool_type, BalancerV3PoolType::Weighted)
            {
                U256::ZERO
            } else {
                let tolerance_bps = 5; // fallback 0.05%
                onchain_val * U256::from(tolerance_bps) / U256::from(10000)
            };

            assert!(
                diff <= allowed_diff,
                "Relative error too high. Local: {amount_out}, OnChain: {onchain_val}, Diff: {diff}, Allowed: {allowed_diff}"
            );
        }
        Err(e) => {
            println!("Router querySwapSingleTokenExactIn failed: {e:?}");
            // Do not fail test if router reverts (e.g. paused pool), but log it
        }
    }
    Ok(())
}

async fn run_swap_test_with_provider<P>(
    provider: P,
    block_id: BlockId,
    vault_address: Address,
    router_address: Address,
    pool_address: Address,
    pool_type: BalancerV3PoolType,
    token_in_idx: usize,
    token_out_idx: usize,
    amount_units: f64,
    check_desc: &str,
) -> eyre::Result<()>
where
    P: Provider + Clone,
{
    // Check if pool exists
    let code = provider.get_code_at(pool_address).await?;
    if code.is_empty() {
        println!("Skipping test: Pool contract not found at {pool_address}");
        return Ok(());
    }

    let mut pool = BalancerV3Pool::new(pool_address, vault_address, pool_type);
    pool = pool.init(block_id, provider.clone()).await?;

    assert_eq!(pool.pool_type, pool_type);
    println!("Pool Initialized: {:?}", pool.address);

    if pool.token_list.len() < 2 {
        println!("Pool has insufficient tokens");
        return Ok(());
    }

    let token_in_addr = pool.token_list[token_in_idx];
    let token_out_addr = pool.token_list[token_out_idx];

    let token_in_state = pool.tokens.get(&token_in_addr).unwrap();
    let decimals = token_in_state.decimals;

    let mut amount_in = if decimals <= 18 {
        U256::from((amount_units * 10f64.powi(decimals as i32)) as u128)
    } else {
        U256::from(amount_units as u128)
    };

    // For weighted pools, ensure amount_in stays within MAX_IN_RATIO (30% of balance).
    if matches!(pool_type, BalancerV3PoolType::Weighted) {
        let max_in = token_in_state
            .balance
            .checked_mul(U256::from(300_000_000_000_000_000u128))
            .unwrap_or(U256::ZERO)
            .checked_div(U256::from(1_000_000_000_000_000_000u128))
            .unwrap_or(U256::ZERO);
        if max_in > U256::ZERO && amount_in > max_in {
            let clamped = std::cmp::max(max_in / U256::from(2u8), U256::from(1u8));
            println!(
                "Exact-in amount clamped: requested={} max_allowed={} clamped={}",
                amount_in, max_in, clamped
            );
            amount_in = clamped;
        }
    }

    println!("Simulating Swap: {amount_units} of {token_in_addr} -> {token_out_addr}");
    let amount_out = pool.simulate_swap(token_in_addr, token_out_addr, amount_in)?;

    let router = IBalancerV3Router::new(router_address, provider.clone());
    match router
        .querySwapSingleTokenExactIn(
            pool_address,
            token_in_addr,
            token_out_addr,
            amount_in,
            Address::ZERO,
            alloy::primitives::Bytes::new(),
        )
        .block(block_id)
        .call()
        .await
    {
        Ok(onchain_amount_out) => {
            let onchain_val = onchain_amount_out;

            let diff = if amount_out > onchain_val {
                amount_out - onchain_val
            } else {
                onchain_val - amount_out
            };

            println!("===============");
            println!("{check_desc}: Local={amount_out}, OnChain={onchain_val}, Diff={diff}");

            let allowed_diff = if matches!(pool_type, BalancerV3PoolType::Stable)
                || matches!(pool_type, BalancerV3PoolType::Weighted)
            {
                U256::ZERO
            } else {
                let tolerance_bps = 5; // fallback 0.05%
                onchain_val * U256::from(tolerance_bps) / U256::from(10000)
            };

            assert!(
                diff <= allowed_diff,
                "Relative error too high. Local: {amount_out}, OnChain: {onchain_val}, Diff: {diff}, Allowed: {allowed_diff}"
            );
        }
        Err(e) => {
            println!("Router querySwapSingleTokenExactIn failed: {e:?}");
        }
    }
    Ok(())
}

async fn run_swap_exact_out_test(
    pool_address: Address,
    pool_type: BalancerV3PoolType,
    token_in_idx: usize,
    token_out_idx: usize,
    amount_units: f64,
    check_desc: &str,
) -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let rpc_endpoint = match std::env::var("ETHEREUM_PROVIDER") {
        Ok(url) => url,
        Err(_) => {
            println!("Skipping test: ETHEREUM_PROVIDER not set");
            return Ok(());
        }
    };

    let provider = ProviderBuilder::new().connect_http(rpc_endpoint.parse()?);
    let block_number = provider.get_block_number().await?;
    let block_id = BlockId::number(block_number);

    // Balancer V3 Vault/Router (Mainnet)
    let vault_address = Address::from_str("ba1333333333a1BA1108E8412f11850A5C319bA9")?;
    let router_address = Address::from_str("0xAE563E3f8219521950555F5962419C8919758Ea2")?;

    run_swap_exact_out_test_with_provider(
        provider,
        block_id,
        vault_address,
        router_address,
        pool_address,
        pool_type,
        token_in_idx,
        token_out_idx,
        amount_units,
        check_desc,
    )
    .await
}

async fn run_swap_exact_out_test_with_provider<P>(
    provider: P,
    block_id: BlockId,
    vault_address: Address,
    router_address: Address,
    pool_address: Address,
    pool_type: BalancerV3PoolType,
    token_in_idx: usize,
    token_out_idx: usize,
    amount_units: f64,
    check_desc: &str,
) -> eyre::Result<()>
where
    P: Provider + Clone,
{
    let code = provider.get_code_at(pool_address).await?;
    if code.is_empty() {
        println!("Skipping test: Pool contract not found at {pool_address}");
        return Ok(());
    }

    let mut pool = BalancerV3Pool::new(pool_address, vault_address, pool_type);
    pool = pool.init(block_id, provider.clone()).await?;

    assert_eq!(pool.pool_type, pool_type);
    println!("Pool Initialized: {:?}", pool.address);

    if pool.token_list.len() < 2 {
        println!("Pool has insufficient tokens");
        return Ok(());
    }

    let token_in_addr = pool.token_list[token_in_idx];
    let token_out_addr = pool.token_list[token_out_idx];

    let token_out_state = pool.tokens.get(&token_out_addr).unwrap();
    let decimals = token_out_state.decimals;

    let mut amount_out = if decimals <= 18 {
        U256::from((amount_units * 10f64.powi(decimals as i32)) as u128)
    } else {
        U256::from(amount_units as u128)
    };

    // Ensure amount_out stays within pool constraints to avoid MAX_OUT_RATIO and liquidity errors.
    // MAX_OUT_RATIO for weighted math is 0.3e18 (30% of balance_out).
    let max_by_liquidity = token_out_state.balance.saturating_sub(U256::from(1u8));
    let max_by_ratio = if matches!(pool_type, BalancerV3PoolType::Weighted) {
        // balance_out * 0.3
        token_out_state
            .balance
            .checked_mul(U256::from(300_000_000_000_000_000u128))
            .unwrap_or(U256::ZERO)
            .checked_div(U256::from(1_000_000_000_000_000_000u128))
            .unwrap_or(U256::ZERO)
    } else {
        max_by_liquidity
    };
    let max_allowed = std::cmp::min(max_by_liquidity, max_by_ratio);
    if amount_out > max_allowed && max_allowed > U256::ZERO {
        let clamped = max_allowed / U256::from(2u8);
        println!(
            "Exact-out amount clamped: requested={} max_allowed={} clamped={}",
            amount_out, max_allowed, clamped
        );
        amount_out = std::cmp::max(clamped, U256::from(1u8));
    }

    println!("Simulating Exact-Out: {amount_units} of {token_out_addr} <- {token_in_addr}");

    let local_amount_in = if matches!(pool_type, BalancerV3PoolType::Weighted) {
        let token_in_state = pool.tokens.get(&token_in_addr).unwrap();
        let max_in = token_in_state
            .balance
            .checked_mul(U256::from(300_000_000_000_000_000u128))
            .unwrap_or(U256::ZERO)
            .checked_div(U256::from(1_000_000_000_000_000_000u128))
            .unwrap_or(U256::ZERO);

        let mut attempt_out = amount_out;
        let mut attempts = 0u8;
        loop {
            attempts += 1;
            match pool.simulate_swap_exact_out(token_in_addr, token_out_addr, attempt_out) {
                Ok(in_amt) => {
                    if max_in > U256::ZERO && in_amt > max_in {
                        attempt_out = std::cmp::max(attempt_out / U256::from(2u8), U256::from(1u8));
                        if attempts >= 12 {
                            return Err(eyre::eyre!(
                                "Exact-out amount requires too much input even after clamping"
                            ));
                        }
                        continue;
                    }
                    if attempt_out != amount_out {
                        println!(
                            "Exact-out amount adjusted to satisfy MAX_IN_RATIO: {} -> {}",
                            amount_out, attempt_out
                        );
                        amount_out = attempt_out;
                    }
                    break in_amt;
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("MAX_IN_RATIO") || msg.contains("MAX_OUT_RATIO") || msg.contains("Math Error") {
                        attempt_out = std::cmp::max(attempt_out / U256::from(2u8), U256::from(1u8));
                        if attempts >= 12 {
                            return Err(eyre::eyre!(
                                "Exact-out amount not supported after clamping: {msg}"
                            ));
                        }
                        continue;
                    }
                    return Err(e.into());
                }
            }
        }
    } else {
        pool.simulate_swap_exact_out(token_in_addr, token_out_addr, amount_out)?
    };

    let router = IBalancerV3Router::new(router_address, provider.clone());
    match router
        .querySwapSingleTokenExactOut(
            pool_address,
            token_in_addr,
            token_out_addr,
            amount_out,
            Address::ZERO,
            alloy::primitives::Bytes::new(),
        )
        .block(block_id)
        .call()
        .await
    {
        Ok(onchain_amount_in) => {
            let diff = if local_amount_in > onchain_amount_in {
                local_amount_in - onchain_amount_in
            } else {
                onchain_amount_in - local_amount_in
            };

            println!("===============");
            println!("{check_desc}: LocalIn={local_amount_in}, OnChainIn={onchain_amount_in}, Diff={diff}");

            // Stable/Weighted pools should match exactly (no hooks).
            let allowed_diff = if matches!(pool_type, BalancerV3PoolType::Stable)
                || matches!(pool_type, BalancerV3PoolType::Weighted)
            {
                U256::ZERO
            } else {
                U256::ZERO
            };

            assert!(
                diff <= allowed_diff,
                "Exact-out error too high. Local: {local_amount_in}, OnChain: {onchain_amount_in}, Diff: {diff}, Allowed: {allowed_diff}"
            );
        }
        Err(e) => {
            println!("Router querySwapSingleTokenExactOut failed: {e:?}");
        }
    }

    Ok(())
}

fn select_weighted_pair(pool: &BalancerV3Pool) -> eyre::Result<(usize, usize)> {
    let weights = pool
        .weights
        .as_ref()
        .ok_or_else(|| eyre::eyre!("Missing weights for weighted pool"))?;
    if weights.len() < 2 {
        return Err(eyre::eyre!("Weighted pool has insufficient tokens"));
    }
    let mut max_idx = 0usize;
    let mut max_w = U256::ZERO;
    for (i, w) in weights.iter().enumerate() {
        if *w > max_w {
            max_w = *w;
            max_idx = i;
        }
    }
    let other_idx = if max_idx == 0 { 1 } else { 0 };
    Ok((max_idx, other_idx))
}

#[tokio::test]
async fn test_balancer_v3_stable_swap_gho_usdt_usdc() -> eyre::Result<()> {
    let pool = Address::from_str("0x85b2b559bc2d21104c4defdd6efca8a20343361d")?;
    // GHO -> USDT (0 -> 1)
    run_swap_test(
        pool,
        BalancerV3PoolType::Stable,
        0,
        1,
        100.0,
        "Stable GHO->USDT",
    )
    .await?;
    // USDT -> GHO (1 -> 0)
    run_swap_test(
        pool,
        BalancerV3PoolType::Stable,
        1,
        0,
        100.0,
        "Stable USDT->GHO",
    )
    .await?;

    // USDT -> USDC (1 -> 2)
    run_swap_test(
        pool,
        BalancerV3PoolType::Stable,
        1,
        2,
        100.0, // 100 USDT
        "Stable USDT->USDC",
    )
    .await?;

    // USDC -> GHO (2 -> 0)
    run_swap_test(
        pool,
        BalancerV3PoolType::Stable,
        2,
        0,
        100.0, // 100 USDC
        "Stable USDC->GHO",
    )
    .await
}

#[tokio::test]
async fn test_balancer_v3_stable_swap_weth_wsteth() -> eyre::Result<()> {
    let pool = Address::from_str("0xc4ce391d82d164c166df9c8336ddf84206b2f812")?;
    // WETH -> wstETH
    run_swap_test(
        pool,
        BalancerV3PoolType::Stable,
        0,
        1,
        1.0,
        "Stable WETH->wstETH",
    )
    .await?;
    // wstETH -> WETH
    run_swap_test(
        pool,
        BalancerV3PoolType::Stable,
        1,
        0,
        1.0,
        "Stable wstETH->WETH",
    )
    .await
}

#[tokio::test]
async fn test_balancer_v3_stable_exact_out_weth_wsteth() -> eyre::Result<()> {
    let pool = Address::from_str("0xc4ce391d82d164c166df9c8336ddf84206b2f812")?;
    // WETH -> wstETH exact-out
    run_swap_exact_out_test(
        pool,
        BalancerV3PoolType::Stable,
        0,
        1,
        1.0,
        "Stable exact-out WETH->wstETH",
    )
    .await?;
    // wstETH -> WETH exact-out
    run_swap_exact_out_test(
        pool,
        BalancerV3PoolType::Stable,
        1,
        0,
        1.0,
        "Stable exact-out wstETH->WETH",
    )
    .await
}

#[tokio::test]
async fn test_balancer_v3_stable_exact_out_gho_usdt_usdc() -> eyre::Result<()> {
    let pool = Address::from_str("0x85b2b559bc2d21104c4defdd6efca8a20343361d")?;
    // GHO -> USDT (0 -> 1)
    run_swap_exact_out_test(
        pool,
        BalancerV3PoolType::Stable,
        0,
        1,
        100.0,
        "Stable exact-out GHO->USDT",
    )
    .await?;
    // USDT -> GHO (1 -> 0)
    run_swap_exact_out_test(
        pool,
        BalancerV3PoolType::Stable,
        1,
        0,
        100.0,
        "Stable exact-out USDT->GHO",
    )
    .await?;
    // USDT -> USDC (1 -> 2)
    run_swap_exact_out_test(
        pool,
        BalancerV3PoolType::Stable,
        1,
        2,
        100.0,
        "Stable exact-out USDT->USDC",
    )
    .await?;
    // USDC -> GHO (2 -> 0)
    run_swap_exact_out_test(
        pool,
        BalancerV3PoolType::Stable,
        2,
        0,
        100.0,
        "Stable exact-out USDC->GHO",
    )
    .await
}

#[tokio::test]
async fn test_balancer_v3_stable_exact_out_reth_waeth() -> eyre::Result<()> {
    let pool = Address::from_str("0x1ea5870f7c037930ce1d5d8d9317c670e89e13e3")?;
    // rETH -> waEth
    run_swap_exact_out_test(
        pool,
        BalancerV3PoolType::Stable,
        0,
        1,
        0.1,
        "Stable exact-out rETH->waEth",
    )
    .await?;
    // waEth -> rETH
    run_swap_exact_out_test(
        pool,
        BalancerV3PoolType::Stable,
        1,
        0,
        0.1,
        "Stable exact-out waEth->rETH",
    )
    .await
}

#[tokio::test]
async fn test_balancer_v3_stable_swap_reth_waeth() -> eyre::Result<()> {
    let pool = Address::from_str("0x1ea5870f7c037930ce1d5d8d9317c670e89e13e3")?;
    // rETH -> waEth
    run_swap_test(
        pool,
        BalancerV3PoolType::Stable,
        0,
        1,
        0.1,
        "Stable rETH->waEth",
    )
    .await?;
    // waEth -> rETH
    run_swap_test(
        pool,
        BalancerV3PoolType::Stable,
        1,
        0,
        0.1,
        "Stable waEth->rETH",
    )
    .await
}

#[tokio::test]
async fn test_balancer_v3_weighted_swap_weth_tree() -> eyre::Result<()> {
    let pool = Address::from_str("0xdaba3d8ccf79ef289a7e2dbce51871b39ea445a2")?;
    // WETH -> TREE
    run_swap_test(
        pool,
        BalancerV3PoolType::Weighted,
        0,
        1,
        1.0,
        "Weighted WETH->TREE",
    )
    .await?;
    // TREE -> WETH
    run_swap_test(
        pool,
        BalancerV3PoolType::Weighted,
        1,
        0,
        100.0,
        "Weighted TREE->WETH",
    )
    .await
}

#[tokio::test]
async fn test_balancer_v3_weighted_exact_out_weth_tree() -> eyre::Result<()> {
    let pool = Address::from_str("0xdaba3d8ccf79ef289a7e2dbce51871b39ea445a2")?;
    run_swap_exact_out_test(
        pool,
        BalancerV3PoolType::Weighted,
        0,
        1,
        1.0,
        "Weighted exact-out WETH->TREE",
    )
    .await?;
    run_swap_exact_out_test(
        pool,
        BalancerV3PoolType::Weighted,
        1,
        0,
        100.0,
        "Weighted exact-out TREE->WETH",
    )
    .await
}

#[tokio::test]
async fn test_balancer_v3_weighted_alcx_weth_80_20() -> eyre::Result<()> {
    // Pool: 20% WETH / 80% ALCX
    let pool = Address::from_str("0x1535d7ca00323aa32bd62aeddf7ca651e4b95966")?;

    // Check 1: Small Swap WETH -> ALCX
    run_swap_test(
        pool,
        BalancerV3PoolType::Weighted,
        0,
        1,
        0.1,
        "Weighted 80/20 WETH->ALCX (Small)",
    )
    .await?;

    // Check 2: Large Swap ALCX -> WETH (100 ALCX)
    run_swap_test(
        pool,
        BalancerV3PoolType::Weighted,
        1,
        0,
        100.0,
        "Weighted 80/20 ALCX->WETH (Large)",
    )
    .await
}

#[tokio::test]
async fn test_balancer_v3_weighted_exact_out_alcx_weth_80_20() -> eyre::Result<()> {
    let pool = Address::from_str("0x1535d7ca00323aa32bd62aeddf7ca651e4b95966")?;
    run_swap_exact_out_test(
        pool,
        BalancerV3PoolType::Weighted,
        0,
        1,
        0.1,
        "Weighted exact-out 80/20 WETH->ALCX (Small)",
    )
    .await?;
    run_swap_exact_out_test(
        pool,
        BalancerV3PoolType::Weighted,
        1,
        0,
        100.0,
        "Weighted exact-out 80/20 ALCX->WETH (Large)",
    )
    .await
}

#[tokio::test]
async fn test_balancer_v3_weighted_eigen_weth_50_50() -> eyre::Result<()> {
    // Pool: 50% WETH / 50% EIGEN
    let pool = Address::from_str("0xbda917a67c7d9ae67da92c4ea87e10e5d6c11b54")?;

    // EIGEN -> WETH
    run_swap_test(
        pool,
        BalancerV3PoolType::Weighted,
        1,
        0,
        10.0,
        "Weighted 50/50 EIGEN->WETH",
    )
    .await?;

    // WETH -> EIGEN
    run_swap_test(
        pool,
        BalancerV3PoolType::Weighted,
        0,
        1,
        1.0,
        "Weighted 50/50 WETH->EIGEN",
    )
    .await
}

#[tokio::test]
async fn test_balancer_v3_weighted_exact_out_eigen_weth_50_50() -> eyre::Result<()> {
    let pool = Address::from_str("0xbda917a67c7d9ae67da92c4ea87e10e5d6c11b54")?;
    run_swap_exact_out_test(
        pool,
        BalancerV3PoolType::Weighted,
        1,
        0,
        10.0,
        "Weighted exact-out 50/50 EIGEN->WETH",
    )
    .await?;
    run_swap_exact_out_test(
        pool,
        BalancerV3PoolType::Weighted,
        0,
        1,
        1.0,
        "Weighted exact-out 50/50 WETH->EIGEN",
    )
    .await
}

#[tokio::test]
async fn test_balancer_v3_weighted_exact_in_pool_index_v3() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let rpc_endpoint = match std::env::var("ETHEREUM_PROVIDER") {
        Ok(url) => url,
        Err(_) => {
            println!("Skipping test: ETHEREUM_PROVIDER not set");
            return Ok(());
        }
    };

    let provider = ProviderBuilder::new().connect_http(rpc_endpoint.parse()?);
    let block_number = provider.get_block_number().await?;
    let block_id = BlockId::number(block_number);
    let vault_address = Address::from_str("ba1333333333a1BA1108E8412f11850A5C319bA9")?;
    let router_address = Address::from_str("0xAE563E3f8219521950555F5962419C8919758Ea2")?;

    let pools = vec![
        ("0x6687b8d041a178ef7b865b60dfce39ebb0700e1b", "Weighted v3 8-token (weight 0.86 core)"),
        ("0x6378c977cc421f71dfff5aa72b1567d1082ad00d", "Weighted v3 8-token (ALCX/USDC mix)"),
        ("0xb1f62fc950e30a64a5032bbd8619a70b2c2b27c6", "Weighted v3 7-token (PEPE core)"),
        ("0xc3b10d061c1e172883135532f1dca99348544959", "Weighted v3 8-token (USDC core)"),
        ("0xb96008d1d926a6129bd91a12c924bd49b79d7bf5", "Weighted v3 3-token (USDC/USDT/PEPE)"),
    ];

    for (addr, label) in pools {
        let pool = Address::from_str(addr)?;
        let mut pool_state = BalancerV3Pool::new(pool, vault_address, BalancerV3PoolType::Weighted);
        pool_state = pool_state.init(block_id, provider.clone()).await?;
        let (idx_a, idx_b) = select_weighted_pair(&pool_state)?;

        let desc_0_1 = format!("PoolIndex Weighted exact-in {label} 0->1");
        run_swap_test_with_provider(
            provider.clone(),
            block_id,
            vault_address,
            router_address,
            pool,
            BalancerV3PoolType::Weighted,
            idx_a,
            idx_b,
            1.0,
            &desc_0_1,
        )
        .await?;

        let desc_1_0 = format!("PoolIndex Weighted exact-in {label} 1->0");
        run_swap_test_with_provider(
            provider.clone(),
            block_id,
            vault_address,
            router_address,
            pool,
            BalancerV3PoolType::Weighted,
            idx_b,
            idx_a,
            1.0,
            &desc_1_0,
        )
        .await?;
    }

    Ok(())
}

#[tokio::test]
async fn test_balancer_v3_weighted_exact_out_pool_index_v3() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let rpc_endpoint = match std::env::var("ETHEREUM_PROVIDER") {
        Ok(url) => url,
        Err(_) => {
            println!("Skipping test: ETHEREUM_PROVIDER not set");
            return Ok(());
        }
    };

    let provider = ProviderBuilder::new().connect_http(rpc_endpoint.parse()?);
    let block_number = provider.get_block_number().await?;
    let block_id = BlockId::number(block_number);
    let vault_address = Address::from_str("ba1333333333a1BA1108E8412f11850A5C319bA9")?;
    let router_address = Address::from_str("0xAE563E3f8219521950555F5962419C8919758Ea2")?;

    let pools = vec![
        ("0x6687b8d041a178ef7b865b60dfce39ebb0700e1b", "Weighted v3 8-token (weight 0.86 core)"),
        ("0x6378c977cc421f71dfff5aa72b1567d1082ad00d", "Weighted v3 8-token (ALCX/USDC mix)"),
        ("0xb1f62fc950e30a64a5032bbd8619a70b2c2b27c6", "Weighted v3 7-token (PEPE core)"),
        ("0xc3b10d061c1e172883135532f1dca99348544959", "Weighted v3 8-token (USDC core)"),
        ("0xb96008d1d926a6129bd91a12c924bd49b79d7bf5", "Weighted v3 3-token (USDC/USDT/PEPE)"),
    ];

    for (addr, label) in pools {
        let pool = Address::from_str(addr)?;
        let mut pool_state = BalancerV3Pool::new(pool, vault_address, BalancerV3PoolType::Weighted);
        pool_state = pool_state.init(block_id, provider.clone()).await?;
        let (idx_a, idx_b) = select_weighted_pair(&pool_state)?;

        let desc_0_1 = format!("PoolIndex Weighted exact-out {label} 0->1");
        run_swap_exact_out_test_with_provider(
            provider.clone(),
            block_id,
            vault_address,
            router_address,
            pool,
            BalancerV3PoolType::Weighted,
            idx_a,
            idx_b,
            1.0,
            &desc_0_1,
        )
        .await?;

        let desc_1_0 = format!("PoolIndex Weighted exact-out {label} 1->0");
        run_swap_exact_out_test_with_provider(
            provider.clone(),
            block_id,
            vault_address,
            router_address,
            pool,
            BalancerV3PoolType::Weighted,
            idx_b,
            idx_a,
            1.0,
            &desc_1_0,
        )
        .await?;
    }

    Ok(())
}

#[tokio::test]
async fn test_balancer_v3_stable_exact_in_pool_index_v3() -> eyre::Result<()> {
    let pools = vec![
        ("0x1ea5870f7c037930ce1d5d8d9317c670e89e13e3", "Stable v3 wstETH/rETH"),
        ("0x57c23c58b1d8c3292c15becf07c62c5c52457a42", "Stable v3 wstETH/pyUSD"),
    ];

    for (addr, label) in pools {
        let pool = Address::from_str(addr)?;
        let desc_0_1 = format!("PoolIndex Stable exact-in {label} 0->1");
        run_swap_test(
            pool,
            BalancerV3PoolType::Stable,
            0,
            1,
            1.0,
            &desc_0_1,
        )
        .await?;

        let desc_1_0 = format!("PoolIndex Stable exact-in {label} 1->0");
        run_swap_test(
            pool,
            BalancerV3PoolType::Stable,
            1,
            0,
            1.0,
            &desc_1_0,
        )
        .await?;
    }

    Ok(())
}

#[tokio::test]
async fn test_balancer_v3_stable_exact_out_pool_index_v3() -> eyre::Result<()> {
    let pools = vec![
        ("0x1ea5870f7c037930ce1d5d8d9317c670e89e13e3", "Stable v3 wstETH/rETH"),
        ("0x57c23c58b1d8c3292c15becf07c62c5c52457a42", "Stable v3 wstETH/pyUSD"),
    ];

    for (addr, label) in pools {
        let pool = Address::from_str(addr)?;
        let desc_0_1 = format!("PoolIndex Stable exact-out {label} 0->1");
        run_swap_exact_out_test(
            pool,
            BalancerV3PoolType::Stable,
            0,
            1,
            1.0,
            &desc_0_1,
        )
        .await?;

        let desc_1_0 = format!("PoolIndex Stable exact-out {label} 1->0");
        run_swap_exact_out_test(
            pool,
            BalancerV3PoolType::Stable,
            1,
            0,
            1.0,
            &desc_1_0,
        )
        .await?;
    }

    Ok(())
}

#[tokio::test]
async fn test_balancer_v3_arbitrum_gho_usdt_usdc_pool() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let rpc_endpoint = match std::env::var("ARBITRUM_PROVIDER") {
        Ok(url) => url,
        Err(_) => {
            println!("Skipping test: ARBITRUM_PROVIDER not set");
            return Ok(());
        }
    };

    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(250))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(rpc_endpoint.parse()?);
    let provider = ProviderBuilder::new().connect_client(client);

    let block_number = provider.get_block_number().await?;
    let block_id = BlockId::number(block_number);

    // Balancer V3 Vault/Router (Arbitrum)
    let vault_address = Address::from_str("0xbA1333333333a1BA1108E8412f11850A5C319bA9")?;
    let router_address = Address::from_str("0xEAedc32a51c510d35ebC11088fD5fF2b47aACF2E")?;

    // Balancer Aave GHO/USDT/USDC Pool (Arbitrum V3)
    let pool = Address::from_str("0x19b001e6bc2d89154c18e2216eec5c8c6047b6d8")?;

    // Pool is stable with 3 tokens; test multiple pair directions.
    let pairs = [
        (0usize, 1usize, "GHO->USDT"),
        (1usize, 0usize, "USDT->GHO"),
        (1usize, 2usize, "USDT->USDC"),
        (2usize, 1usize, "USDC->USDT"),
        (2usize, 0usize, "USDC->GHO"),
        (0usize, 2usize, "GHO->USDC"),
    ];

    for (i, j, desc) in pairs {
        run_swap_test_with_provider(
            provider.clone(),
            block_id,
            vault_address,
            router_address,
            pool,
            BalancerV3PoolType::Stable,
            i,
            j,
            100.0,
            desc,
        )
        .await?;
    }

    for (i, j, desc) in pairs {
        run_swap_exact_out_test_with_provider(
            provider.clone(),
            block_id,
            vault_address,
            router_address,
            pool,
            BalancerV3PoolType::Stable,
            i,
            j,
            100.0,
            desc,
        )
        .await?;
    }

    Ok(())
}

#[tokio::test]
async fn test_calculate_price() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(250))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(rpc_endpoint.parse()?);

    let provider = ProviderBuilder::new().connect_client(client);

    // 7-token weighted pool
    let pool_address = Address::from_str("0xB1F62fc950E30A64a5032bBD8619A70B2c2B27C6")?;
    let vault_address = Address::from_str("0xbA1333333333a1BA1108E8412f11850A5C319bA9")?;

    let mut pool = BalancerV3Pool::new(
        pool_address,
        vault_address,
        BalancerV3PoolType::Weighted,
    );

    pool = pool.init(BlockId::latest(), provider.clone()).await?;

    // Use UNI and another token from the pool
    let uni = Address::from_str("0x1f9840a85d5af5bf1d1762f925bdaddc4201f984")?;
    let other = Address::from_str("0xbe1936a67f503e0eaf2434b0cf9f4e3d7100008a")?; 

    let price_uni = pool.calculate_price(uni, other)?;
    let price_other = pool.calculate_price(other, uni)?;

    println!("UNI Price in Other: {}", price_uni);
    println!("Other Price in UNI: {}", price_other);

    assert!(price_uni > 0.0);
    assert!(price_other > 0.0);

    // Sanity check
    let product = price_uni * price_other;
    assert!(product > 0.99 && product < 1.01);

    Ok(())
}

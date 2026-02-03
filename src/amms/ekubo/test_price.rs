#[cfg(test)]
mod tests {
    use crate::amms::{
        amm::AutomatedMarketMaker,
        ekubo::{EkuboPool, EkuboPoolKey},
    };
    use alloy::{
        eips::BlockId,
        primitives::{address, Address, U256},
        providers::ProviderBuilder,
    };
    use eyre::Result;
    use tracing_subscriber;

    // ETH/USDC Pool values from mod.rs unit test
    // Token0: Address::ZERO (Native ETH)
    // Token1: USDC
    // Config: 0x...

    #[tokio::test]
    async fn test_calculate_price() -> Result<()> {
        let _ = tracing_subscriber::fmt::try_init();
        dotenv::dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let provider = ProviderBuilder::new().connect_http(rpc_endpoint.parse()?);

        // ETH (Native)
        let token0 = Address::ZERO;
        // USDC
        let token1 = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

        // Config from mod.rs test_pool_key_from_raw
        // This corresponds to some fee/tick_spacing.
        // fee: 3689348814741910
        // tick_spacing: 5000 ?? From unit test?
        // Actually, let's use the raw config from mod.rs test
        let config_hex = "0000000000000000000000000000000000000000000d1b71758e21960000137e";
        let config = U256::from_str_radix(config_hex, 16).unwrap();

        let pool_key = EkuboPoolKey::from_raw(token0, token1, config);
        let pool_id = pool_key.pool_id();
        println!("Ekubo Pool ID: {:?}", pool_id);

        // Core address
        let core_address = address!("e0e0e08a6a4b9dc7bd67bcb7aade5cf48157d444");

        let mut pool = EkuboPool::new(core_address, pool_key);

        // Use a recent block
        let block_number = BlockId::latest();

        // fetch_core_state is internal (pub(super)).
        // I need to access it.
        // If it's pub(super) and I am in a submodule `test_price`, `super::pool` access?
        // Wait, `test_price.rs` is `mod test_price`. `pool` is `super::pool`.
        // `EkuboPool` is re-exported in `mod.rs` (super).
        // But `fetch_core_state` is `pub(super)`.
        // If `test_price` is a child of `ekubo`, `super` is `ekubo`.
        // `pool` is `ekubo::pool`.
        // `Am I` inside `ekubo` module? Yes if `mod.rs` has `mod test_price`.
        // `pub(super)` in `pool.rs` means visible to `ekubo`.
        // `test_price` is `ekubo::test_price`.
        // Does `ekubo::test_price` have access to `pub(super)` of `ekubo::pool`?
        // No. `pub(super)` in `pool.rs` means visible in `ekubo` (mod.rs).
        // `test_price` is a sibling of `pool`?
        // If `mod.rs` has `mod test_price;` and `mod pool;`, they are siblings.
        // `pub(super)` in `pool` matches parent `ekubo`.
        // Siblings can access `pub(super)` members of other siblings if they are in the same parent.
        // Yes, `test_price` is in `ekubo`. `pool` defines methods visible to `ekubo`.
        // So `test_price` can access them via `crate::amms::ekubo::pool::EkuboPool::fetch_core_state`?
        // Or access via `pool.fetch_core_state`.
        // I need to import `EkuboPool` from `super`.

        // Wait, `fetch_core_state` takes `self` and returns `Result<Self>`.
        // Let's try calling it.

        pool = pool
            .fetch_core_state(block_number, provider.clone())
            .await?;

        println!("Liquidity: {}", pool.liquidity);
        println!("Sqrt Price: {}", pool.sqrt_price);
        println!("Tick: {}", pool.tick);

        // Calculate price
        // Token A (ETH) -> Token B (USDC)
        let price_eth_usdc = pool.calculate_price(token0, token1)?;
        println!("ETH price in USDC: {}", price_eth_usdc);

        if pool.liquidity > 0 {
            assert!(
                price_eth_usdc > 1000.0 && price_eth_usdc < 10000.0,
                "ETH price reasonableness check"
            );
        } else {
            println!("Pool has no liquidity at this block/config, skipping price assertion");
        }

        Ok(())
    }
}

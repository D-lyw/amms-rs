#[cfg(test)]
mod tests {
    use crate::amms::{amm::{AMM, AutomatedMarketMaker}, erc_4626::ERC4626Vault};
    use alloy::{eips::BlockId, primitives::address, providers::ProviderBuilder, rpc::client::ClientBuilder, transports::layers::{RetryBackoffLayer, ThrottleLayer}};
    use eyre::Result;

    #[tokio::test]
    #[ignore]
    async fn test_init_batch() -> Result<()> {
        dotenv::dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;
        
        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(100))
            .layer(RetryBackoffLayer::new(5, 100, 300))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        // sDAI (Savings DAI)
        let sdai_address = address!("83F20F44975D03b1b09e64809B757c47f942BEeA");
        let vaults = vec![
            AMM::ERC4626Vault(ERC4626Vault::new(sdai_address)),
            // AMM::ERC4626Vault(ERC4626Vault::new(wsteth_address)),
        ];

        let block_number = BlockId::from(20000000);

        let initialized_vaults = ERC4626Vault::init_batch::<_, _>(vaults, block_number, provider).await?;

        assert_eq!(initialized_vaults.len(), 1);

        // Check sDAI
        let sdai = initialized_vaults.iter().find(|v| v.address() == sdai_address).unwrap();
        if let AMM::ERC4626Vault(v) = sdai {
            assert!(v.vault_reserve > alloy::primitives::U256::ZERO);
            assert!(v.asset_reserve > alloy::primitives::U256::ZERO);
            assert_eq!(v.vault_token_decimals, 18);
            assert_eq!(v.asset_token_decimals, 18);
            // Check prices
            assert!(v.vault_token_price > 0.0);
            assert!(v.asset_token_price > 0.0);
            println!("sDAI Price: {}", v.vault_token_price);
        } else {
            panic!("Wrong type for sDAI");
        }
/*
        // Check wstETH
        let wsteth = initialized_vaults.iter().find(|v| v.address() == wsteth_address).unwrap();
        if let AMM::ERC4626Vault(v) = wsteth {
            assert!(v.vault_reserve > alloy::primitives::U256::ZERO);
            assert!(v.asset_reserve > alloy::primitives::U256::ZERO);
            assert_eq!(v.vault_token_decimals, 18);
            // wstETH is 18, stETH is 18.
            
            println!("wstETH Price: {}", v.vault_token_price);
            // wstETH price in stETH should be > 1.0
            assert!(v.vault_token_price > 1.0);
        } else {
            panic!("Wrong type for wstETH");
        }
*/

        Ok(())
    }
}

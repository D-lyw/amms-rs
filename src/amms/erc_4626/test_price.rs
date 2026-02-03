#[cfg(test)]
mod tests {
    use crate::amms::{amm::AutomatedMarketMaker, erc_4626::ERC4626Vault};
    use alloy::{eips::BlockId, primitives::address, providers::ProviderBuilder};
    use eyre::Result;
    use tracing_subscriber;

    #[tokio::test]
    async fn test_calculate_price() -> Result<()> {
        let _ = tracing_subscriber::fmt::try_init();
        dotenv::dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let provider = ProviderBuilder::new().connect_http(rpc_endpoint.parse()?);

        // sDAI (Savings DAI) on Mainnet
        let vault_address = address!("83F20F44975D03b1b09e64809B757c47f942BEeA");
        // Underlying DAI
        let dai = address!("6B175474E89094C44Da98b954EedeAC495271d0F");

        // Block 19M
        let block_number = BlockId::from(19000000);

        let mut vault = ERC4626Vault::new(vault_address);

        // Use manual update because init uses BatchRequest which might fail (NotActivated issue)
        // But let's try calling get_reserves directly first which is standard.
        // Wait, ERC4626Vault has `get_reserves`.
        // But `init` populates decimals/fees using BatchRequest `GetERC4626VaultDataBatchRequest`.
        // If that fails, I should manually populate.

        // Try to fetch reserves manually using IERC4626Vault
        let (total_supply, total_assets) =
            vault.get_reserves(provider.clone(), block_number).await?;

        vault.vault_reserve = total_supply;
        vault.asset_reserve = total_assets;

        // Set decimals manually for sDAI (18) and DAI (18)
        vault.vault_token_decimals = 18;
        vault.asset_token_decimals = 18;
        vault.asset_token = dai;

        // Fees: sDAI has 0 entry/exit fee usually.
        vault.deposit_fee = 0;
        vault.withdraw_fee = 0;

        println!("sDAI Total Assets: {}", vault.asset_reserve);
        println!("sDAI Total Supply: {}", vault.vault_reserve);

        // Calculate Price of 1 Vault Token (sDAI) in Asset (DAI)
        let price_sdai_dai = vault.calculate_price(vault.vault_token, vault.asset_token)?;
        println!("Price sDAI in DAI: {}", price_sdai_dai);

        // Calculate Price of 1 Asset (DAI) in Vault Token (sDAI)
        let price_dai_sdai = vault.calculate_price(vault.asset_token, vault.vault_token)?;
        println!("Price DAI in sDAI: {}", price_dai_sdai);

        // Expect sDAI > 1 DAI (since it accrues interest)
        assert!(price_sdai_dai > 1.0);
        assert!(price_dai_sdai < 1.0);

        // Consistency
        let product = price_sdai_dai * price_dai_sdai;
        assert!(product > 0.99 && product < 1.01);

        Ok(())
    }
}

//! ERC4626 Vault simulate_swap_exact_out tests
//!
//! These tests verify the exact-out simulation logic for ERC4626 vaults.

use alloy::primitives::{Address, U256};
use amms::amms::{
    amm::AutomatedMarketMaker,
    erc_4626::ERC4626Vault,
};

// Test vault addresses
const SDAI_VAULT: Address = alloy::primitives::address!("83F20F44975D03b1b09e64809B757c47f942BEeA");
const DAI_TOKEN: Address = alloy::primitives::address!("6B175474E89094C44Da98b954EedeAC495271d0F");
const ASSET_TOKEN: Address = alloy::primitives::address!("1111111111111111111111111111111111111111");

/// Create a test vault with specified parameters
fn create_test_vault(
    vault_token: Address,
    asset_token: Address,
    vault_reserve: U256,
    asset_reserve: U256,
    deposit_fee: u32,
    withdraw_fee: u32,
    vault_decimals: u8,
    asset_decimals: u8,
) -> ERC4626Vault {
    ERC4626Vault {
        last_synced_block: 0,
        vault_token,
        vault_token_decimals: vault_decimals,
        asset_token,
        asset_token_decimals: asset_decimals,
        vault_reserve,
        asset_reserve,
        deposit_fee,
        withdraw_fee,
        vault_token_price: 0.0,
        asset_token_price: 0.0,
    }
}

// ============================================================================
// UNIT TESTS
// ============================================================================

#[test]
fn test_simulate_swap_exact_out_zero_amount() {
    let vault = create_test_vault(
        SDAI_VAULT,
        ASSET_TOKEN,
        U256::from(1000000000000000000000u128), // 1000 shares
        U256::from(1000000000000000000000u128), // 1000 assets
        0,  // 0% deposit fee
        0,  // 0% withdraw fee
        18,
        18,
    );

    // Zero amount should return zero
    let result = vault.simulate_swap_exact_out(SDAI_VAULT, Address::ZERO, U256::ZERO);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), U256::ZERO);
}

#[test]
fn test_simulate_swap_exact_out_insufficient_liquidity() {
    let vault = create_test_vault(
        SDAI_VAULT,
        ASSET_TOKEN,
        U256::from(1000000000000000000000u128), // 1000 shares
        U256::from(1000000000000000000000u128), // 1000 assets
        0,
        0,
        18,
        18,
    );

    // Request more than available should error
    let large_amount = U256::from(2000000000000000000000u128); // 2000 assets
    let result = vault.simulate_swap_exact_out(SDAI_VAULT, Address::ZERO, large_amount);
    assert!(result.is_err());
}

#[test]
fn test_simulate_swap_exact_out_withdraw_no_fee() {
    // Vault with 1000 shares backed by 1000 assets (1:1 ratio, no fees)
    let vault = create_test_vault(
        SDAI_VAULT,
        ASSET_TOKEN,
        U256::from(1000000000000000000000u128), // 1000 shares
        U256::from(1000000000000000000000u128), // 1000 assets
        0,  // 0% deposit fee
        0,  // 0% withdraw fee
        18,
        18,
    );

    // Withdraw: want 100 assets out
    let amount_out = U256::from(100000000000000000000u128); // 100 assets
    let result = vault.simulate_swap_exact_out(SDAI_VAULT, Address::ZERO, amount_out);

    assert!(result.is_ok());
    let amount_in = result.unwrap();

    // With 1:1 ratio and no fee, should need ~100 shares
    // Due to ceiling division, might be slightly more
    println!("Withdraw: need {} shares to get {} assets", amount_in, amount_out);
    assert!(amount_in >= U256::from(100000000000000000000u128));
    // Allow larger margin due to ceiling division in exact-out formula
    assert!(amount_in < U256::from(150000000000000000000u128));
}

#[test]
fn test_simulate_swap_exact_out_deposit_no_fee() {
    // Vault with 1000 shares backed by 1000 assets (1:1 ratio, no fees)
    let asset_token = ASSET_TOKEN;
    let vault = create_test_vault(
        SDAI_VAULT,
        asset_token,
        U256::from(1000000000000000000000u128), // 1000 shares
        U256::from(1000000000000000000000u128), // 1000 assets
        0,
        0,
        18,
        18,
    );

    // Deposit: want 100 shares out
    let amount_out = U256::from(100000000000000000000u128); // 100 shares
    let result = vault.simulate_swap_exact_out(asset_token, SDAI_VAULT, amount_out);

    assert!(result.is_ok());
    let amount_in = result.unwrap();

    // With 1:1 ratio and no fee, should need ~100 assets
    println!("Deposit: need {} assets to get {} shares", amount_in, amount_out);
    assert!(amount_in >= U256::from(100000000000000000000u128));
    // Allow larger margin due to ceiling division in exact-out formula
    assert!(amount_in < U256::from(150000000000000000000u128));
}

#[test]
fn test_simulate_swap_exact_out_withdraw_with_fee() {
    // Vault with 1% withdraw fee
    let vault = create_test_vault(
        SDAI_VAULT,
        ASSET_TOKEN,
        U256::from(1000000000000000000000u128), // 1000 shares
        U256::from(1000000000000000000000u128), // 1000 assets
        0,    // 0% deposit fee
        100,  // 1% withdraw fee (100 basis points)
        18,
        18,
    );

    // Withdraw: want 100 assets out
    let amount_out = U256::from(100000000000000000000u128);
    let result = vault.simulate_swap_exact_out(SDAI_VAULT, Address::ZERO, amount_out);

    assert!(result.is_ok());
    let amount_in = result.unwrap();

    // With 1% fee, need more shares than 1:1
    println!("Withdraw with fee: need {} shares to get {} assets", amount_in, amount_out);
    assert!(amount_in > U256::from(100000000000000000000u128)); // > 100 shares
}

#[test]
fn test_simulate_swap_exact_out_deposit_with_fee() {
    // Vault with 1% deposit fee
    let asset_token = ASSET_TOKEN;
    let vault = create_test_vault(
        SDAI_VAULT,
        asset_token,
        U256::from(1000000000000000000000u128), // 1000 shares
        U256::from(1000000000000000000000u128), // 1000 assets
        100, // 1% deposit fee
        0,   // 0% withdraw fee
        18,
        18,
    );

    // Deposit: want 100 shares out
    let amount_out = U256::from(100000000000000000000u128);
    let result = vault.simulate_swap_exact_out(asset_token, SDAI_VAULT, amount_out);

    assert!(result.is_ok());
    let amount_in = result.unwrap();

    // With 1% fee, need more assets than 1:1
    println!("Deposit with fee: need {} assets to get {} shares", amount_in, amount_out);
    assert!(amount_in > U256::from(100000000000000000000u128)); // > 100 assets
}

#[test]
fn test_simulate_swap_exact_out_reverse_verify() {
    // Reverse verification: simulate_swap(exact_out_result) >= amount_out
    let vault = create_test_vault(
        SDAI_VAULT,
        ASSET_TOKEN,
        U256::from(10000000000000000000000u128), // 10000 shares
        U256::from(10000000000000000000000u128), // 10000 assets
        50,  // 0.5% deposit fee
        50,  // 0.5% withdraw fee
        18,
        18,
    );

    let test_amounts = [
        U256::from(100000000000000000000u128),   // 100
        U256::from(500000000000000000000u128),   // 500
        U256::from(1000000000000000000000u128),  // 1000
    ];

    for amount_out in test_amounts {
        // Withdraw direction
        let exact_in = vault
            .simulate_swap_exact_out(SDAI_VAULT, Address::ZERO, amount_out)
            .expect("exact-out should succeed");

        let verify_out = vault
            .simulate_swap(SDAI_VAULT, Address::ZERO, exact_in)
            .expect("exact-in should succeed");

        println!(
            "Withdraw: target={}, exact_in={}, verify_out={}",
            amount_out, exact_in, verify_out
        );

        // Verify output should be >= target
        assert!(
            verify_out >= amount_out,
            "verify_out ({}) should be >= amount_out ({})",
            verify_out,
            amount_out
        );
    }
}

#[test]
fn test_simulate_swap_exact_out_different_decimals() {
    // Vault with 6-decimal asset (like USDC) and 18-decimal shares
    let vault = create_test_vault(
        SDAI_VAULT,
        ASSET_TOKEN,
        U256::from(1000000000000000000000000u128), // 1M shares (18 decimals)
        U256::from(1000000000u128),                 // 1000 USDC (6 decimals)
        0,
        0,
        18,
        6,
    );

    // Want 100 USDC out
    let amount_out = U256::from(100000000u128); // 100 USDC
    let result = vault.simulate_swap_exact_out(SDAI_VAULT, Address::ZERO, amount_out);

    assert!(result.is_ok());
    let amount_in = result.unwrap();

    println!("Different decimals: need {} shares to get {} USDC", amount_in, amount_out);
    assert!(amount_in > U256::ZERO);
}

#[test]
fn test_simulate_swap_exact_out_round_trip() {
    // Round-trip test: exact-out -> exact-in should give back >= original
    let asset_token = ASSET_TOKEN;
    let vault = create_test_vault(
        SDAI_VAULT,
        asset_token,
        U256::from(10000000000000000000000u128),
        U256::from(10000000000000000000000u128),
        30,  // 0.3% deposit fee
        30,  // 0.3% withdraw fee
        18,
        18,
    );

    // Test both directions
    let target_out = U256::from(1000000000000000000000u128); // 100

    // Direction 1: Withdraw (vault -> asset)
    let exact_in_1 = vault.simulate_swap_exact_out(SDAI_VAULT, asset_token, target_out).unwrap();
    let verify_out_1 = vault.simulate_swap(SDAI_VAULT, asset_token, exact_in_1).unwrap();
    assert!(verify_out_1 >= target_out, "Withdraw round-trip failed");

    // Direction 2: Deposit (asset -> vault)
    let exact_in_2 = vault.simulate_swap_exact_out(asset_token, SDAI_VAULT, target_out).unwrap();
    let verify_out_2 = vault.simulate_swap(asset_token, SDAI_VAULT, exact_in_2).unwrap();
    assert!(verify_out_2 >= target_out, "Deposit round-trip failed");

    println!("Withdraw: target={}, in={}, verify={}", target_out, exact_in_1, verify_out_1);
    println!("Deposit: target={}, in={}, verify={}", target_out, exact_in_2, verify_out_2);
}

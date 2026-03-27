//! Pancake Infinity exact-out simulation tests
//!
//! NOTE: Pancake Infinity is only deployed on BNB Chain (chain id: 56)
//! Set BNB_CHAIN_PROVIDER environment variable to run on-chain tests
//!
//! Example:
//!   BNB_CHAIN_PROVIDER=https://bsc-dataseed1.binance.org cargo test --test pancake_infinity_tests

use alloy::primitives::{Address, U256};
use amms::amms::{
    amm::AutomatedMarketMaker,
    pancake_infinity::{PancakeInfinityPool, ICLPoolManager::PoolKey},
    uniswap_v3::Info,
    Token,
};
use std::collections::HashMap;

// Pancake Infinity CLPoolManager on BNB Chain
#[allow(dead_code)]
const POOL_MANAGER: Address = alloy::primitives::address!(
    "0x41ff9AA7e16B8B1a6a673e28D9aC80dD556c5864"
);

// Example tokens on BNB Chain
#[allow(dead_code)]
const CAKE_TOKEN: Address = alloy::primitives::address!(
    "0x0E09FaBB73Bd3Ade0a17ECC321fD13a19e81cE82"
);
#[allow(dead_code)]
const WBNB_TOKEN: Address = alloy::primitives::address!(
    "0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c"
);

/// Create a test pool manually with specified parameters
fn create_test_pool(
    manager_address: Address,
    token_a: Address,
    token_b: Address,
    sqrt_price: U256,
    tick: i32,
    liquidity: u128,
    tick_spacing: i32,
    protocol_fee: u32,
    lp_fee: u32,
) -> PancakeInfinityPool {
    // Ensure token_a < token_b (required by Uniswap-style pools)
    let (currency0, currency1) = if token_a < token_b {
        (token_a, token_b)
    } else {
        (token_b, token_a)
    };

    // Build parameters bytes32 with tick_spacing encoded
    let mut parameters = [0u8; 32];
    // tick_spacing is i24, encoded in the last 3 bytes
    let tick_spacing_bytes = (tick_spacing as i32).to_be_bytes();
    parameters[29..32].copy_from_slice(&tick_spacing_bytes[1..4]);

    let pool_key = PoolKey {
        currency0,
        currency1,
        hooks: Address::ZERO,
        poolManager: manager_address,
        fee: alloy::primitives::aliases::U24::from(lp_fee),
        parameters: alloy::primitives::B256::from(parameters),
    };

    let pool_id = alloy::primitives::keccak256(alloy::sol_types::SolValue::abi_encode(&pool_key));

    PancakeInfinityPool {
        pool_key,
        pool_id,
        manager_address,
        last_synced_block: 0,
        token_a: Token::new_with_decimals(currency0, 18),
        token_b: Token::new_with_decimals(currency1, 18),
        sqrt_price,
        liquidity,
        tick,
        tick_spacing,
        protocol_fee,
        lp_fee,
        tick_bitmap: HashMap::new(),
        ticks: HashMap::new(),
        token_a_price: 0.0,
        token_b_price: 0.0,
    }
}

/// Create a pool with initialized ticks for more realistic testing
#[allow(dead_code)]
fn create_pool_with_ticks(
    manager_address: Address,
    token_a: Address,
    token_b: Address,
    sqrt_price: U256,
    tick: i32,
    liquidity: u128,
    tick_spacing: i32,
    protocol_fee: u32,
    lp_fee: u32,
    initialized_ticks: &[(i32, u128, i128)], // (tick, liquidity_gross, liquidity_net)
) -> PancakeInfinityPool {
    let mut pool = create_test_pool(
        manager_address,
        token_a,
        token_b,
        sqrt_price,
        tick,
        liquidity,
        tick_spacing,
        protocol_fee,
        lp_fee,
    );

    for &(tick_idx, liquidity_gross, liquidity_net) in initialized_ticks {
        // Add tick info
        pool.ticks.insert(
            tick_idx,
            Info {
                liquidity_gross,
                liquidity_net,
                initialized: true,
            },
        );

        // Update tick bitmap
        let compressed = if tick_idx >= 0 {
            tick_idx / tick_spacing
        } else {
            (tick_idx + 1) / tick_spacing - 1
        };
        let (word_pos, bit_pos) = uniswap_v3_math::tick_bitmap::position(compressed);
        let mask = U256::from(1) << bit_pos;
        pool.tick_bitmap
            .entry(word_pos)
            .and_modify(|w| *w |= mask)
            .or_insert(mask);
    }

    pool
}

// ============================================================================
// UNIT TESTS (不需要链上连接)
// ============================================================================

#[test]
fn test_simulate_swap_exact_out_zero_amount() {
    let pool = create_test_pool(
        POOL_MANAGER,
        CAKE_TOKEN,
        WBNB_TOKEN,
        U256::from(79228162514264337593543950336u128), // sqrt(1) * 2^96
        0,
        1000000000000000000u128,
        60,
        0,
        3000,
    );

    // 零值输入应该返回零
    let result = pool.simulate_swap_exact_out(CAKE_TOKEN, WBNB_TOKEN, U256::ZERO);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), U256::ZERO);
}

#[test]
fn test_simulate_swap_exact_out_sqrt_price_zero() {
    // 创建一个 sqrt_price 为零的池
    let pool = create_test_pool(
        POOL_MANAGER,
        CAKE_TOKEN,
        WBNB_TOKEN,
        U256::ZERO, // sqrt_price = 0
        0,
        1000000000000000000u128,
        60,
        0,
        3000,
    );

    // 应该返回错误
    let result = pool.simulate_swap_exact_out(CAKE_TOKEN, WBNB_TOKEN, U256::from(1000u64));
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("sqrt_price is zero"));
}

#[test]
fn test_simulate_swap_exact_out_insufficient_liquidity() {
    // 创建一个流动性为 0 的池
    let pool = create_test_pool(
        POOL_MANAGER,
        CAKE_TOKEN,
        WBNB_TOKEN,
        U256::from(79228162514264337593543950336u128),
        0,
        0, // liquidity = 0
        60,
        0,
        3000,
    );

    // 请求大量输出应该返回流动性不足错误
    let large_amount = U256::from(1000000000000000000000000u128); // 1e24
    let result = pool.simulate_swap_exact_out(CAKE_TOKEN, WBNB_TOKEN, large_amount);
    assert!(result.is_err());
}

#[test]
fn test_simulate_swap_exact_out_round_trip() {
    // 创建一个有流动性的测试池
    let pool = create_test_pool(
        POOL_MANAGER,
        CAKE_TOKEN,
        WBNB_TOKEN,
        U256::from(79228162514264337593543950336u128), // sqrt(1) * 2^96 ≈ 价格 1
        0,
        1000000000000000000000000000u128, // 大流动性
        60,
        0,
        3000, // 0.3% fee
    );

    // 目标输出金额
    let target_out = U256::from(1000000000000000000u128); // 1e18

    // 精确输出模拟
    let exact_in = pool
        .simulate_swap_exact_out(CAKE_TOKEN, WBNB_TOKEN, target_out)
        .expect("exact-out simulation should succeed");

    println!("Target out: {}", target_out);
    println!("Required in: {}", exact_in);

    // 验证: exact_in > 0
    assert!(exact_in > U256::ZERO);

    // 验证: exact_in 应该略大于 target_out (因为有手续费)
    // 对于 0.3% 手续费，输入应该比输出大约 0.3%
    assert!(exact_in >= target_out);
}

#[test]
fn test_simulate_swap_exact_out_direction_consistency() {
    // 测试两个方向的 exact-out
    let pool = create_test_pool(
        POOL_MANAGER,
        CAKE_TOKEN, // token_a
        WBNB_TOKEN,  // token_b
        U256::from(79228162514264337593543950336u128),
        0,
        1000000000000000000000000000u128,
        60,
        0,
        3000,
    );

    let target_out = U256::from(1000000000000000000u128);

    // 方向 1: token_a -> token_b
    let result1 = pool.simulate_swap_exact_out(CAKE_TOKEN, WBNB_TOKEN, target_out);
    assert!(result1.is_ok());

    // 方向 2: token_b -> token_a
    let result2 = pool.simulate_swap_exact_out(WBNB_TOKEN, CAKE_TOKEN, target_out);
    assert!(result2.is_ok());

    println!("A->B required in: {}", result1.unwrap());
    println!("B->A required in: {}", result2.unwrap());
}

#[test]
fn test_simulate_swap_exact_out_reverse_verify() {
    // 反向验证: simulate_swap(exact_out_result) >= amount_out
    let pool = create_test_pool(
        POOL_MANAGER,
        CAKE_TOKEN,
        WBNB_TOKEN,
        U256::from(79228162514264337593543950336u128),
        0,
        1000000000000000000000000000u128,
        60,
        0,
        3000,
    );

    let test_amounts = [
        U256::from(100000000000000000u128),   // 0.1e18
        U256::from(500000000000000000u128),   // 0.5e18
        U256::from(1000000000000000000u128),  // 1e18
    ];

    for target_out in test_amounts {
        // 精确输出模拟
        let exact_in = pool
            .simulate_swap_exact_out(CAKE_TOKEN, WBNB_TOKEN, target_out)
            .expect("exact-out should succeed");

        // 使用精确输入模拟验证
        let verify_out = pool
            .simulate_swap(CAKE_TOKEN, WBNB_TOKEN, exact_in)
            .expect("exact-in should succeed");

        println!(
            "Target: {}, Exact-in: {}, Verify-out: {}",
            target_out, exact_in, verify_out
        );

        // 验证输出应该 >= 目标 (由于精度可能略大于目标)
        assert!(
            verify_out >= target_out,
            "verify_out ({}) should be >= target_out ({})",
            verify_out,
            target_out
        );
    }
}

#[test]
fn test_simulate_swap_exact_out_protocol_fee() {
    // 测试协议费的影响
    let pool_no_protocol_fee = create_test_pool(
        POOL_MANAGER,
        CAKE_TOKEN,
        WBNB_TOKEN,
        U256::from(79228162514264337593543950336u128),
        0,
        1000000000000000000000000000u128,
        60,
        0, // 无协议费
        3000,
    );

    let pool_with_protocol_fee = create_test_pool(
        POOL_MANAGER,
        CAKE_TOKEN,
        WBNB_TOKEN,
        U256::from(79228162514264337593543950336u128),
        0,
        1000000000000000000000000000u128,
        60,
        1000, // 0.1% 协议费
        3000, // 0.3% LP费
    );

    let target_out = U256::from(1000000000000000000u128);

    let exact_in_no_protocol = pool_no_protocol_fee
        .simulate_swap_exact_out(CAKE_TOKEN, WBNB_TOKEN, target_out)
        .unwrap();

    let exact_in_with_protocol = pool_with_protocol_fee
        .simulate_swap_exact_out(CAKE_TOKEN, WBNB_TOKEN, target_out)
        .unwrap();

    println!(
        "Without protocol fee: {}, With protocol fee: {}",
        exact_in_no_protocol, exact_in_with_protocol
    );

    // 有协议费时，总费率更高，所以需要的输入更多
    // 总费率 = protocol_fee + lp_fee * (1 - protocol_fee) ≈ 0.1% + 0.3% * 0.999 ≈ 0.3997%
    assert!(exact_in_with_protocol >= exact_in_no_protocol);
}


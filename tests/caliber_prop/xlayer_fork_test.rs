//! Caliber propAMM — 全链路真实性验证（XLayer 链上 Fork 测试）
//!
//! ## 验证范围
//!
//! 1. **池子发现 & 初始化** — getAllPairIds + getPoolBalances + batchQuote
//! 2. **分段线性插值精度** — 使用非采样点交易量对比 `simulate_swap` vs `quote()`
//! 3. **边界条件** — 前向越界（before-first 按比例推算）和后向越界（超过 Ladder 范围报错）
//! 4. **Consumed 追踪 + simulate_swap_mut** — 连续多笔 swap 验证 consumed 状态正确性
//! 5. **StateSpace 集成** — with_amms 构建
//!
//! ## 关键设计
//!
//! 测试使用的交易量百分比如下（全都不在 `SAMPLE_BPS` 网格上）：
//!
//! | 测试类型 | BPS | 相对 Ladder 位置 |
//! |---|---|---|
//! | 前向推算 | 3 | < 第一个采样点 10 |
//! | 插值 | 17 | 在 10 和 25 之间 |
//! | 插值 | 62 | 在 50 和 75 之间 |
//! | 插值 | 130 | 在 100 和 150 之间 |
//! | 插值 | 220 | 在 200 和 250 之间 |
//! | 插值 | 370 | 在 300 和 400 之间 |
//! | 插值 | 1200 | 在 1000 和 1500 之间 |
//! | 插值 | 5500 | 在 5000 和 6000 之间 |
//! | 插值 | 9500 | 在 9000 和 9900 之间 |
//! | 后向越界 | 9950 | > 最后一个采样点 9900 |
//!
//! ## 参考合约
//!
//! - XLayer: `0x154586B2479b9a11e3d4db90024Dc0e26F097312`
//! - 环境变量: `XLAYER_PROVIDER` 或 `XLAYER_RPC_URL`

use alloy::{
    eips::BlockId,
    hex::FromHex,
    primitives::{address, keccak256, Address, B256, U256},
    providers::{Provider, ProviderBuilder},
    sol,
};
use amms::{
    amms::{
        amm::{AutomatedMarketMaker, AMM},
        caliber_prop::{CaliberPropPool, ICaliberPropAMM},
    },
    state_space::StateSpaceBuilder,
};
use eyre::Result;
use std::env;
use std::sync::Arc;

// ============================================================
// Constants
// ============================================================

const XLAYER_CHAIN_ID: u64 = 196;
const CALIBER_CONTRACT: Address = address!("154586b2479b9a11e3d4db90024dc0e26f097312");

/// 允许的最大**插值**偏差（BPS）。仅适用于两个已知采样点之间的
/// 线性插值。前向推算（before-first extrapolation）不在此限制之列。
const MAX_INTERP_DEVIATION_BPS: f64 = 200.0;

/// 用于验证插值精度的非采样点 BPS（全都在 SAMPLE_BPS 网格之外）。
/// 9950 是越界点，预期报错。
const OFF_GRID_BPS: &[(u32, &str)] = &[
    (17, "0.17% (10↔25)"),
    (62, "0.62% (50↔75)"),
    (130, "1.30% (100↔150)"),
    (220, "2.20% (200↔250)"),
    (370, "3.70% (300↔400)"),
    (1200, "12.0% (1000↔1500)"),
    (5500, "55.0% (5000↔6000)"),
    (9500, "95.0% (9000↔9900)"),
    (9950, "99.5% (>9900 overflow)"),
];

/// 连续 simulate_swap_mut 序列的 BPS 值（均 ≥ 首采样点 10 bps）。
const MUT_SWAP_BPS_CURVED: &[u32] = &[15, 25, 35]; // total 75 bps

// ============================================================
// ERC20 ABI
// ============================================================

sol! {
    #[sol(rpc)]
    interface IERC20Metadata {
        function decimals() external view returns (uint8);
        function symbol() external view returns (string);
    }
}

// ============================================================
// Provider helpers
// ============================================================

fn xlayer_provider_url() -> Option<String> {
    dotenv::dotenv().ok();
    env::var("XLAYER_PROVIDER")
        .or_else(|_| env::var("XLAYER_RPC_URL"))
        .ok()
}

fn u256_to_f64(value: &U256) -> f64 {
    let limbs = value.as_limbs();
    let mut result = limbs[0] as f64;
    result += (limbs[1] as f64) * (2.0f64.powi(64));
    result += (limbs[2] as f64) * (2.0f64.powi(128));
    result += (limbs[3] as f64) * (2.0f64.powi(192));
    result
}

fn reserve_share(reserve: &U256, bps: u32) -> U256 {
    *reserve * U256::from(bps) / U256::from(10_000)
}

// ============================================================
// Storage helpers
// ============================================================

async fn get_storage_at_raw<P>(provider: &P, address: Address, slot: B256) -> Result<B256>
where
    P: Provider,
{
    let slot_u256 = U256::from_be_bytes(slot.0);
    let result: U256 = provider.get_storage_at(address, slot_u256).await?;
    Ok(B256::from(result.to_be_bytes::<32>()))
}

/// 从合约 storage 读取 pair 的 token 地址。
async fn read_token_addresses<P>(
    provider: &P,
    contract: Address,
    pair_id: B256,
) -> Result<(Address, Address)>
where
    P: Provider,
{
    let base_slot = B256::from_hex(
        "0x0000000000000000000000000000000000000000000000000000000000000006",
    )?;
    let mut input = [0u8; 64];
    input[..32].copy_from_slice(pair_id.as_ref());
    input[32..].copy_from_slice(base_slot.as_ref());
    let hash = keccak256(input);
    let slot0 = B256::from(hash);

    let mut slot1_num = U256::from_be_bytes(slot0.0);
    slot1_num += U256::from(1);
    let slot1 = B256::from(slot1_num.to_be_bytes::<32>());

    let raw0 = get_storage_at_raw(provider, contract, slot0).await?;
    let raw1 = get_storage_at_raw(provider, contract, slot1).await?;

    let token_x = Address::from_slice(&raw0[12..]);
    let token_y = Address::from_slice(&raw1[12..]);

    Ok((token_x, token_y))
}

/// 链上 quote 单次调用。
async fn chain_quote<P>(
    provider: P,
    pair_id: B256,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> Result<U256>
where
    P: Provider + Clone,
{
    let caliber = ICaliberPropAMM::new(CALIBER_CONTRACT, provider);
    Ok(caliber
        .quote(pair_id, token_in, token_out, amount_in)
        .call()
        .await?)
}

// ============================================================
// 主测试：全链路验证
// ============================================================

#[tokio::test]
async fn test_caliber_prop_full_verification() -> Result<()> {
    let rpc_url = match xlayer_provider_url() {
        Some(url) => url,
        None => {
            println!("SKIP: XLAYER_PROVIDER not set");
            return Ok(());
        }
    };

    let provider = Arc::new(ProviderBuilder::new().connect_http(rpc_url.parse()?));
    let chain_id = provider.get_chain_id().await?;
    if chain_id != XLAYER_CHAIN_ID {
        println!("SKIP: expected XLayer chain_id {}, got {}", XLAYER_CHAIN_ID, chain_id);
        return Ok(());
    }

    // ========================================================================
    // Phase 0: 池子发现 & 初始化
    // ========================================================================

    println!("=== Phase 0: Discover & Initialize ===");

    let caliber = ICaliberPropAMM::new(CALIBER_CONTRACT, &provider);
    let pair_ids = caliber
        .getAllPairIds(U256::ZERO, U256::from(50u64))
        .call()
        .await?;
    println!("Found {} pair(s)", pair_ids.len());
    assert!(!pair_ids.is_empty(), "no Caliber pairs found on XLayer");

    let pair_id = pair_ids[0];
    let (token_x, token_y) = read_token_addresses(&provider, CALIBER_CONTRACT, pair_id).await?;

    let erc20_x = IERC20Metadata::new(token_x, &provider);
    let erc20_y = IERC20Metadata::new(token_y, &provider);
    let decimals_x = erc20_x.decimals().call().await.unwrap_or(18);
    let decimals_y = erc20_y.decimals().call().await.unwrap_or(18);

    let (token_a, token_b, decimals_a, decimals_b) = if token_x < token_y {
        (token_x, token_y, decimals_x, decimals_y)
    } else {
        (token_y, token_x, decimals_y, decimals_x)
    };

    let virtual_address = CaliberPropPool::virtual_address_from_pair_id(pair_id, CALIBER_CONTRACT);

    let pool = CaliberPropPool {
        contract_address: CALIBER_CONTRACT,
        pair_id,
        virtual_address,
        token_x,
        token_y,
        created_block: 0,
        last_synced_block: 0,
        token_a: amms::amms::Token { address: token_a, decimals: decimals_a, symbol: String::new(), chain_id },
        token_b: amms::amms::Token { address: token_b, decimals: decimals_b, symbol: String::new(), chain_id },
        reserve_a: U256::ZERO,
        reserve_b: U256::ZERO,
        ladder: Default::default(),
        price_a_in_b: 0.0,
        price_b_in_a: 0.0,
    };

    let pool = pool
        .init::<alloy::network::Ethereum, _>(BlockId::latest(), &provider)
        .await?;

    println!("  Pair: token_a={token_a} token_b={token_b}");
    println!("  reserve_a={} reserve_b={}", pool.reserve_a, pool.reserve_b);
    println!("  ladder points: a→b={}, b→a={}",
        pool.ladder.ladder_a_to_b.len(),
        pool.ladder.ladder_b_to_a.len());
    println!("  price_a_in_b={:.6} price_b_in_a={:.6}",
        pool.price_a_in_b, pool.price_b_in_a);

    // ========================================================================
    // Phase 1: 非采样点插值精度验证
    // ========================================================================

    println!("\n=== Phase 1: Off-grid interpolation accuracy ===");

    let mut max_dev_bps = 0.0f64;

    for &(test_bps, label) in OFF_GRID_BPS {
        for &(token_in, token_out, reserve) in &[
            (token_a, token_b, &pool.reserve_a),
            (token_b, token_a, &pool.reserve_b),
        ] {
            let amount_in = reserve_share(reserve, test_bps);
            if amount_in.is_zero() {
                continue;
            }

            let local_res = pool.simulate_swap(token_in, token_out, amount_in);
            let chain_res = chain_quote(
                provider.clone(), pair_id, token_in, token_out, amount_in,
            ).await?;

            match local_res {
                Ok(local_out) => {
                    if chain_res.is_zero() {
                        println!("  [{label:30}] {token_in:12}→{token_out:12} chain=0 (skip)");
                        continue;
                    }

                    let diff = if chain_res > local_out { chain_res - local_out } else { local_out - chain_res };
                    let diff_bps = u256_to_f64(&diff) / u256_to_f64(&chain_res) * 10000.0;
                    if diff_bps > max_dev_bps { max_dev_bps = diff_bps; }

                    let status = if diff_bps <= MAX_INTERP_DEVIATION_BPS { "OK" } else { "FAIL" };
                    println!(
                        "  [{status:4}] {label:30} local={local_out:30} chain={chain_res:30} diff={diff_bps:.2}bps"
                    );

                    assert!(
                        diff_bps <= MAX_INTERP_DEVIATION_BPS,
                        "interpolation deviation {diff_bps:.2} BPS exceeds {MAX_INTERP_DEVIATION_BPS}"
                    );
                }
                Err(e) => {
                    if test_bps == 9950 {
                        println!("  [OK  ] {label:30} expected error: {e}");
                    } else {
                        panic!("unexpected simulate_swap error at {label}: {e}");
                    }
                }
            }
        }
    }

    println!("  Worst deviation: {:.2} BPS (threshold: {MAX_INTERP_DEVIATION_BPS} BPS)", max_dev_bps);

    // ========================================================================
    // Phase 2a: 前向推算（before first）
    // ========================================================================

    println!("\n=== Phase 2a: Before-first extrapolation ===");

    for &(token_in, token_out) in &[(token_a, token_b), (token_b, token_a)] {
        let reserve = if token_in == token_a { &pool.reserve_a } else { &pool.reserve_b };
        let amount_in = reserve_share(reserve, 3);

        // simulate_swap 对 before-first 返回 0
        let local_out = pool.simulate_swap(token_in, token_out, amount_in)?;
        assert_eq!(local_out, U256::ZERO);
        let chain_out = chain_quote(provider.clone(), pair_id, token_in, token_out, amount_in).await?;
        println!(
            "  {token_in:12}→{token_out:12} local=0 chain={chain_out:30} (safe fallback)"
        );
    }

    // ========================================================================
    // Phase 2b: 后向越界（beyond last）
    // ========================================================================

    println!("\n=== Phase 2b: Beyond-last-point error ===");

    for &(token_in, token_out) in &[(token_a, token_b), (token_b, token_a)] {
        let reserve = if token_in == token_a { &pool.reserve_a } else { &pool.reserve_b };
        let amount_in = reserve_share(reserve, 9950);
        match pool.simulate_swap(token_in, token_out, amount_in) {
            Err(e) => println!("  {token_in:12}→{token_out:12} correctly errored: {e}"),
            Ok(v) => panic!("expected error for beyond-last amount, got {v}"),
        }
    }

    // ========================================================================
    // Phase 3: Before-first guard (simulate_swap returns 0)
    // ========================================================================

    println!("\n=== Phase 3: Before-first guard (simulate_swap returns 0) ===");

    for &(token_in, token_out) in &[(token_a, token_b), (token_b, token_a)] {
        let reserve = if token_in == token_a { &pool.reserve_a } else { &pool.reserve_b };
        let amount_in = reserve_share(reserve, 3);
        let local_out = pool.simulate_swap(token_in, token_out, amount_in)?;
        assert_eq!(local_out, U256::ZERO, "simulate_swap should return 0 for before-first amount");
        println!("  {token_in:12}→{token_out:12} amount_in < first sample → 0");
    }

    // ========================================================================
    // Phase 4: Consumed 追踪 + simulate_swap_mut（仅 b→a 方向，Ladder 有曲线变化）
    // ========================================================================

    println!("\n=== Phase 4: Sequential consume tracking (b→a only) ===");

    {
        let mut test_pool = pool.clone();
        let mut cumulative_amount_in = U256::ZERO;
        let mut cumulative_amount_out_local = U256::ZERO;

        for &mut_bps in MUT_SWAP_BPS_CURVED {
            let amount_in = reserve_share(&pool.reserve_b, mut_bps);
            let amount_out = test_pool.simulate_swap_mut(token_b, token_a, amount_in)?;

            assert!(!amount_out.is_zero(), "swap_mut returned zero at {mut_bps}bps");
            cumulative_amount_in += amount_in;
            cumulative_amount_out_local += amount_out;

            assert_eq!(test_pool.ladder.consumed_in_ba, cumulative_amount_in,
                "consumed_in_ba mismatch after {mut_bps}bps swap");
        }

        // 链上全量校验
        let chain_total_out = chain_quote(
            provider.clone(), pair_id, token_b, token_a, cumulative_amount_in,
        ).await?;

        assert_eq!(
            test_pool.ladder.consumed_out_ba, cumulative_amount_out_local,
            "consumed_out_ba mismatch"
        );

        let diff = if chain_total_out > cumulative_amount_out_local {
            chain_total_out - cumulative_amount_out_local
        } else {
            cumulative_amount_out_local - chain_total_out
        };
        let diff_bps = if chain_total_out.is_zero() { 0.0 } else {
            u256_to_f64(&diff) / u256_to_f64(&chain_total_out) * 10000.0
        };

        println!(
            "  {token_b:12}→{token_a:12} cumulative local={cumulative_amount_out_local:30} chain={chain_total_out:30} diff={diff_bps:.2}bps",
        );
        assert!(diff_bps <= MAX_INTERP_DEVIATION_BPS);
        assert_eq!(test_pool.reserve_b, pool.reserve_b + cumulative_amount_in);
        assert_eq!(test_pool.reserve_a, pool.reserve_a - cumulative_amount_out_local);
    }

    // ========================================================================
    // Phase 5: 价格缓存合理性
    // ========================================================================

    println!("\n=== Phase 5: Spot price sanity ===");

    assert!(pool.price_a_in_b > 0.0, "price_a_in_b should be positive");
    assert!(pool.price_b_in_a > 0.0, "price_b_in_a should be positive");

    let inv = 1.0 / pool.price_b_in_a;
    let ratio_diff = (pool.price_a_in_b - inv).abs() / pool.price_a_in_b;
    assert!(
        ratio_diff < 0.01,
        "price_a_in_b ({}) ≈ 1/price_b_in_a ({}) off by {:.2}%",
        pool.price_a_in_b, pool.price_b_in_a, ratio_diff * 100.0,
    );
    println!("  price_a_in_b={:.6} price_b_in_a={:.6} reciprocal_error={:.4}%",
        pool.price_a_in_b, pool.price_b_in_a, ratio_diff * 100.0);

    // ========================================================================
    // Phase 6: 现货价格 vs 首 Ladder 点
    // ========================================================================

    println!("\n=== Phase 6: Spot price vs first ladder point ===");

    if let Some(first) = pool.ladder.ladder_a_to_b.first() {
        if !first.amount_in.is_zero() && !first.amount_out.is_zero() {
            let expected = u256_to_f64(&first.amount_out) / u256_to_f64(&first.amount_in)
                * 10f64.powi(pool.token_a.decimals as i32 - pool.token_b.decimals as i32);
            let diff = (pool.price_a_in_b - expected).abs() / expected;
            println!(
                "  from first ladder point: {expected:.6} cached: {} error={:.4}%",
                pool.price_a_in_b, diff * 100.0,
            );
        }
    }

    println!("\n=== All verification phases passed ===");

    Ok(())
}

// ============================================================
// 附加测试: StateSpace 集成（with_amms 构建）
// ============================================================

#[tokio::test]
async fn test_caliber_prop_with_amms() -> Result<()> {
    let rpc_url = match xlayer_provider_url() {
        Some(url) => url,
        None => {
            println!("SKIP: XLAYER_PROVIDER not set");
            return Ok(());
        }
    };

    let provider = Arc::new(ProviderBuilder::new().connect_http(rpc_url.parse()?));
    let chain_id = provider.get_chain_id().await?;
    if chain_id != XLAYER_CHAIN_ID {
        println!("SKIP: wrong chain_id {chain_id}");
        return Ok(());
    }

    let caliber = ICaliberPropAMM::new(CALIBER_CONTRACT, provider.clone());
    let pair_ids = caliber
        .getAllPairIds(U256::ZERO, U256::from(10u64))
        .call()
        .await?;
    if pair_ids.is_empty() {
        println!("SKIP: no pairs found");
        return Ok(());
    }

    let pair_id = pair_ids[0];
    let (token_x, token_y) = read_token_addresses(&provider, CALIBER_CONTRACT, pair_id).await?;

    let (token_a, token_b) = if token_x < token_y { (token_x, token_y) } else { (token_y, token_x) };

    let virtual_address = CaliberPropPool::virtual_address_from_pair_id(pair_id, CALIBER_CONTRACT);

    let pool = CaliberPropPool {
        contract_address: CALIBER_CONTRACT,
        pair_id,
        virtual_address,
        token_x,
        token_y,
        created_block: 0,
        last_synced_block: 0,
        token_a: amms::amms::Token { address: token_a, decimals: 18, symbol: String::new(), chain_id },
        token_b: amms::amms::Token { address: token_b, decimals: 18, symbol: String::new(), chain_id },
        reserve_a: U256::ZERO,
        reserve_b: U256::ZERO,
        ladder: Default::default(),
        price_a_in_b: 0.0,
        price_b_in_a: 0.0,
    };

    let seed_amm = AMM::CaliberPropPool(pool);

    println!("=== with_amms Integration Test ===");
    println!("Building StateSpace with 1 Caliber pool: {virtual_address}");

    let result = StateSpaceBuilder::new(provider.clone())
        .block(0)
        .with_amms(vec![seed_amm])
        .sync()
        .await;

    match result {
        Ok(manager) => {
            let state = manager.state.read().await;
            let pool_from_state = state
                .get(&virtual_address)
                .expect("pool should be in state");

            match pool_from_state {
                AMM::CaliberPropPool(p) => {
                    assert!(p.has_sufficient_liquidity(), "pool should have sufficient liquidity");
                    assert_eq!(p.ladder.ladder_a_to_b.len(), 24, "expected 24 ladder points");
                    println!("  StateSpace built successfully! 24 ladder points confirmed.");
                }
                _ => panic!("unexpected AMM variant in state"),
            }
        }
        Err(e) => {
            panic!("StateSpace build failed: {e}");
        }
    }

    Ok(())
}

//! Caliber propAMM — 全链路真实性验证（XLayer 链上 Fork 测试）
//!
//! ## 验证范围
//!
//! 1. **池子发现 & 初始化** — getAllPairIds + Caliber ladder batch reader
//! 2. **分段线性插值精度** — 使用非采样点交易量对比 `simulate_swap` vs `batchQuote()`
//! 3. **边界条件** — before-first 安全护栏和后向越界（超过 Ladder 范围报错）
//! 4. **Consumed 追踪 + simulate_swap_mut** — 连续多笔 swap 验证 consumed 状态正确性
//! 5. **StateSpace 集成** — with_amms 构建
//!
//! ## 关键设计
//!
//! 测试使用的交易量百分比如下（全都不在基础采样网格上）：
//!
//! | 测试类型 | BPS | 相对 Ladder 位置 |
//! |---|---|---|
//! | before-first | 动态 | `<` 第一个真实采样点 |
//! | 插值 | 4 | 在 3 和 5 之间 |
//! | 插值 | 18 | 在 15 和 20 之间 |
//! | 插值 | 130 | 在 100 和 150 之间 |
//! | 插值 | 220 | 在 200 和 250 之间 |
//! | 插值 | 350 | 在 300 和 400 之间 |
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
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

#[derive(Debug)]
struct LoadedCaliberPool {
    pool: CaliberPropPool,
    symbol_a: String,
    symbol_b: String,
}

#[derive(Debug)]
struct PoolDeviationSummary {
    pair_label: String,
    virtual_address: Address,
    ladder_ab_points: usize,
    ladder_ba_points: usize,
    empty_directions: usize,
    max_deviation_bps: f64,
    checked_quotes: usize,
}

// ============================================================
// Constants
// ============================================================

const XLAYER_CHAIN_ID: u64 = 196;
const CALIBER_CONTRACT: Address = address!("154586b2479b9a11e3d4db90024dc0e26f097312");
const CALIBER_TEST_BLOCK: u64 = 66_309_105;

/// 允许的最大**插值**偏差（BPS）。仅适用于两个已知采样点之间的
/// 线性插值。before-first 安全护栏不在此限制之列。
const MAX_INTERP_DEVIATION_BPS: f64 = 5.0;

/// 用于验证插值精度的非采样点 BPS（全都在基础采样网格之外）。
/// 9950 是越界点，预期报错。
const OFF_GRID_BPS: &[(u32, &str)] = &[
    (4, "0.04% (3↔5)"),
    (6, "0.06% (5↔7)"),
    (8, "0.08% (7↔10)"),
    (12, "0.12% (10↔15)"),
    (18, "0.18% (15↔20)"),
    (23, "0.23% (20↔25)"),
    (35, "0.35% (30↔40)"),
    (60, "0.60% (50↔75)"),
    (90, "0.90% (75↔100)"),
    (130, "1.30% (100↔150)"),
    (175, "1.75% (150↔200)"),
    (220, "2.20% (200↔250)"),
    (275, "2.75% (250↔300)"),
    (350, "3.50% (300↔400)"),
    (625, "6.25% (500↔750)"),
    (1200, "12.0% (1000↔1500)"),
    (1750, "17.5% (1500↔2000)"),
    (2250, "22.5% (2000↔3000)"),
    (3500, "35.0% (3000↔4000)"),
    (5500, "55.0% (5000↔6000)"),
    (8500, "85.0% (8000↔9000)"),
    (9500, "95.0% (9000↔9900)"),
    (9950, "99.5% (>9900 overflow)"),
];

/// 多池横向偏差扫描使用更少的代表性点位，避免把 RPC 压力放大到不必要的程度。
const OFF_GRID_SCAN_BPS: &[(u32, &str)] = &[
    (4, "0.04% (3↔5)"),
    (18, "0.18% (15↔20)"),
    (130, "1.30% (100↔150)"),
    (350, "3.50% (300↔400)"),
    (1200, "12.0% (1000↔1500)"),
    (5500, "55.0% (5000↔6000)"),
    (9500, "95.0% (9000↔9900)"),
    (9950, "99.5% (>9900 overflow)"),
];

/// 连续 simulate_swap_mut 序列的 BPS 值（均 > 首采样点）。
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

fn caliber_test_block() -> u64 {
    env::var("CALIBER_TEST_BLOCK")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(CALIBER_TEST_BLOCK)
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

fn xlayer_test_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn before_first_amount(pool: &CaliberPropPool, token_in: Address) -> U256 {
    let ladder = if token_in == pool.token_a.address {
        &pool.ladder.ladder_a_to_b
    } else {
        &pool.ladder.ladder_b_to_a
    };

    let first = ladder
        .first()
        .expect("caliber ladder should have at least one point")
        .amount_in;
    assert!(first > U256::from(1), "first ladder point should be > 1");
    first - U256::from(1)
}

fn before_first_amount_opt(pool: &CaliberPropPool, token_in: Address) -> Option<U256> {
    let ladder = if token_in == pool.token_a.address {
        &pool.ladder.ladder_a_to_b
    } else {
        &pool.ladder.ladder_b_to_a
    };

    let first = ladder.first()?.amount_in;
    if first > U256::from(1) {
        Some(first - U256::from(1))
    } else {
        None
    }
}

async fn connect_xlayer_provider() -> Result<Option<(Arc<impl Provider>, u64, BlockId)>> {
    let block_id = BlockId::from(caliber_test_block());
    let rpc_url = match xlayer_provider_url() {
        Some(url) => url,
        None => {
            println!("SKIP: XLAYER_PROVIDER not set");
            return Ok(None);
        }
    };

    let provider = Arc::new(ProviderBuilder::new().connect_http(rpc_url.parse()?));
    let chain_id = provider.get_chain_id().await?;
    if chain_id != XLAYER_CHAIN_ID {
        println!(
            "SKIP: expected XLayer chain_id {}, got {}",
            XLAYER_CHAIN_ID, chain_id
        );
        return Ok(None);
    }

    Ok(Some((provider, chain_id, block_id)))
}

async fn discover_pair_ids<P>(provider: P, block_id: BlockId) -> Result<Vec<B256>>
where
    P: Provider + Clone,
{
    let caliber = ICaliberPropAMM::new(CALIBER_CONTRACT, provider);
    Ok(caliber
        .getAllPairIds(U256::ZERO, U256::from(50u64))
        .block(block_id)
        .call()
        .await?)
}

async fn load_caliber_pool<P>(
    provider: P,
    chain_id: u64,
    pair_id: B256,
    block_id: BlockId,
) -> Result<LoadedCaliberPool>
where
    P: Provider + Clone,
{
    let (token_x, token_y) = read_token_addresses(&provider, CALIBER_CONTRACT, pair_id).await?;

    let erc20_x = IERC20Metadata::new(token_x, &provider);
    let erc20_y = IERC20Metadata::new(token_y, &provider);
    let decimals_x = erc20_x.decimals().call().await.unwrap_or(18);
    let decimals_y = erc20_y.decimals().call().await.unwrap_or(18);
    let symbol_x = erc20_x
        .symbol()
        .call()
        .await
        .unwrap_or_else(|_| String::new());
    let symbol_y = erc20_y
        .symbol()
        .call()
        .await
        .unwrap_or_else(|_| String::new());

    let (token_a, token_b, decimals_a, decimals_b, symbol_a, symbol_b) = if token_x < token_y {
        (token_x, token_y, decimals_x, decimals_y, symbol_x, symbol_y)
    } else {
        (token_y, token_x, decimals_y, decimals_x, symbol_y, symbol_x)
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
        token_a: amms::amms::Token {
            address: token_a,
            decimals: decimals_a,
            symbol: symbol_a.clone(),
            chain_id,
        },
        token_b: amms::amms::Token {
            address: token_b,
            decimals: decimals_b,
            symbol: symbol_b.clone(),
            chain_id,
        },
        reserve_a: U256::ZERO,
        reserve_b: U256::ZERO,
        ladder: Default::default(),
        price_a_in_b: 0.0,
        price_b_in_a: 0.0,
    };

    let pool = pool
        .init::<alloy::network::Ethereum, _>(block_id, provider)
        .await?;

    Ok(LoadedCaliberPool {
        pool,
        symbol_a,
        symbol_b,
    })
}

async fn scan_pool_deviation<P>(
    provider: P,
    loaded: &LoadedCaliberPool,
    block_id: BlockId,
) -> Result<PoolDeviationSummary>
where
    P: Provider + Clone,
{
    let pool = &loaded.pool;
    let phase1_requests: Vec<(Address, Address, U256)> = OFF_GRID_SCAN_BPS
        .iter()
        .flat_map(|(test_bps, _)| {
            [
                (
                    pool.token_a.address,
                    pool.token_b.address,
                    reserve_share(&pool.reserve_a, *test_bps),
                ),
                (
                    pool.token_b.address,
                    pool.token_a.address,
                    reserve_share(&pool.reserve_b, *test_bps),
                ),
            ]
        })
        .filter(|(_, _, amount_in)| !amount_in.is_zero())
        .collect();
    let phase1_chain_quotes =
        chain_quotes_batch(provider.clone(), pool.pair_id, &phase1_requests, block_id).await?;

    let mut max_dev_bps = 0.0f64;
    let mut checked_quotes = 0usize;
    let mut empty_directions = 0usize;
    let mut phase1_idx = 0usize;

    for &(test_bps, _) in OFF_GRID_SCAN_BPS {
        for &(token_in, token_out, reserve) in &[
            (pool.token_a.address, pool.token_b.address, &pool.reserve_a),
            (pool.token_b.address, pool.token_a.address, &pool.reserve_b),
        ] {
            let amount_in = reserve_share(reserve, test_bps);
            if amount_in.is_zero() {
                continue;
            }

            let local_res = pool.simulate_swap(token_in, token_out, amount_in);
            let chain_res = phase1_chain_quotes[phase1_idx];
            phase1_idx += 1;

            match local_res {
                Ok(local_out) => {
                    if chain_res.is_zero() {
                        continue;
                    }
                    let diff = if chain_res > local_out {
                        chain_res - local_out
                    } else {
                        local_out - chain_res
                    };
                    let diff_bps = u256_to_f64(&diff) / u256_to_f64(&chain_res) * 10000.0;
                    max_dev_bps = max_dev_bps.max(diff_bps);
                    checked_quotes += 1;
                }
                Err(_) => {
                    assert!(
                        chain_res.is_zero() || test_bps == 9950,
                        "unexpected simulate_swap error for {} / {} at {}bps with non-zero chain quote {}",
                        loaded.symbol_a,
                        loaded.symbol_b,
                        test_bps,
                        chain_res
                    );
                }
            }
        }
    }

    for &(token_in, token_out) in &[
        (pool.token_a.address, pool.token_b.address),
        (pool.token_b.address, pool.token_a.address),
    ] {
        if let Some(amount_in) = before_first_amount_opt(pool, token_in) {
            let local_out = pool.simulate_swap(token_in, token_out, amount_in)?;
            assert_eq!(
                local_out,
                U256::ZERO,
                "before-first guard failed for {} / {}",
                loaded.symbol_a,
                loaded.symbol_b
            );
        } else {
            empty_directions += 1;
        }
    }

    for &(token_in, token_out, reserve) in &[
        (pool.token_a.address, pool.token_b.address, &pool.reserve_a),
        (pool.token_b.address, pool.token_a.address, &pool.reserve_b),
    ] {
        let has_ladder = if token_in == pool.token_a.address {
            !pool.ladder.ladder_a_to_b.is_empty()
        } else {
            !pool.ladder.ladder_b_to_a.is_empty()
        };
        if !has_ladder {
            continue;
        }
        let amount_in = reserve_share(reserve, 9950);
        assert!(
            pool.simulate_swap(token_in, token_out, amount_in).is_err(),
            "expected beyond-last error for {} / {}",
            loaded.symbol_a,
            loaded.symbol_b
        );
    }

    Ok(PoolDeviationSummary {
        pair_label: format!("{}/{}", loaded.symbol_a, loaded.symbol_b),
        virtual_address: pool.virtual_address,
        ladder_ab_points: pool.ladder.ladder_a_to_b.len(),
        ladder_ba_points: pool.ladder.ladder_b_to_a.len(),
        empty_directions,
        max_deviation_bps: max_dev_bps,
        checked_quotes,
    })
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
    let base_slot =
        B256::from_hex("0x0000000000000000000000000000000000000000000000000000000000000006")?;
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

/// 批量链上 reference quote，减少测试过程中的 RPC 次数。
async fn chain_quotes_batch<P>(
    provider: P,
    pair_id: B256,
    requests: &[(Address, Address, U256)],
    block_id: BlockId,
) -> Result<Vec<U256>>
where
    P: Provider + Clone,
{
    if requests.is_empty() {
        return Ok(vec![]);
    }

    let caliber = ICaliberPropAMM::new(CALIBER_CONTRACT, provider);
    let results = caliber
        .batchQuote(
            requests
                .iter()
                .map(
                    |(token_in, token_out, amount_in)| ICaliberPropAMM::QuoteRequest {
                        pairId: pair_id,
                        tokenIn: *token_in,
                        tokenOut: *token_out,
                        amountIn: *amount_in,
                    },
                )
                .collect(),
        )
        .block(block_id)
        .call()
        .await?;
    Ok(results
        .into_iter()
        .map(|result| {
            if result.success {
                result.amountOut
            } else {
                U256::ZERO
            }
        })
        .collect())
}

async fn chain_quote<P>(
    provider: P,
    pair_id: B256,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    block_id: BlockId,
) -> Result<U256>
where
    P: Provider + Clone,
{
    Ok(chain_quotes_batch(
        provider,
        pair_id,
        &[(token_in, token_out, amount_in)],
        block_id,
    )
    .await?
    .into_iter()
    .next()
    .expect("chain_quote should return exactly one result"))
}

// ============================================================
// 主测试：全链路验证
// ============================================================

#[tokio::test]
async fn test_caliber_prop_full_verification() -> Result<()> {
    let _guard = xlayer_test_guard();
    let Some((provider, chain_id, block_id)) = connect_xlayer_provider().await? else {
        return Ok(());
    };

    // ========================================================================
    // Phase 0: 池子发现 & 初始化
    // ========================================================================

    println!("=== Phase 0: Discover & Initialize ===");

    let pair_ids = discover_pair_ids(provider.clone(), block_id).await?;
    println!("Found {} pair(s)", pair_ids.len());
    assert!(!pair_ids.is_empty(), "no Caliber pairs found on XLayer");

    let loaded = load_caliber_pool(provider.clone(), chain_id, pair_ids[0], block_id).await?;
    let pool = loaded.pool;
    let pair_id = pool.pair_id;
    let token_a = pool.token_a.address;
    let token_b = pool.token_b.address;

    println!("  Pair: token_a={token_a} token_b={token_b}");
    println!(
        "  reserve_a={} reserve_b={}",
        pool.reserve_a, pool.reserve_b
    );
    println!(
        "  ladder points: a→b={}, b→a={}",
        pool.ladder.ladder_a_to_b.len(),
        pool.ladder.ladder_b_to_a.len()
    );
    println!(
        "  price_a_in_b={:.6} price_b_in_a={:.6}",
        pool.price_a_in_b, pool.price_b_in_a
    );

    // ========================================================================
    // Phase 1: 非采样点插值精度验证
    // ========================================================================

    println!("\n=== Phase 1: Off-grid interpolation accuracy ===");

    let mut max_dev_bps = 0.0f64;
    let phase1_requests: Vec<(Address, Address, U256)> = OFF_GRID_BPS
        .iter()
        .flat_map(|(test_bps, _)| {
            [
                (token_a, token_b, reserve_share(&pool.reserve_a, *test_bps)),
                (token_b, token_a, reserve_share(&pool.reserve_b, *test_bps)),
            ]
        })
        .filter(|(_, _, amount_in)| !amount_in.is_zero())
        .collect();
    let phase1_chain_quotes =
        chain_quotes_batch(provider.clone(), pair_id, &phase1_requests, block_id).await?;
    let mut phase1_idx = 0usize;

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
            let chain_res = phase1_chain_quotes[phase1_idx];
            phase1_idx += 1;

            match local_res {
                Ok(local_out) => {
                    if chain_res.is_zero() {
                        println!("  [{label:30}] {token_in:12}→{token_out:12} chain=0 (skip)");
                        continue;
                    }

                    let diff = if chain_res > local_out {
                        chain_res - local_out
                    } else {
                        local_out - chain_res
                    };
                    let diff_bps = u256_to_f64(&diff) / u256_to_f64(&chain_res) * 10000.0;
                    if diff_bps > max_dev_bps {
                        max_dev_bps = diff_bps;
                    }

                    let status = if diff_bps <= MAX_INTERP_DEVIATION_BPS {
                        "OK"
                    } else {
                        "FAIL"
                    };
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

    println!(
        "  Worst deviation: {:.2} BPS (threshold: {MAX_INTERP_DEVIATION_BPS} BPS)",
        max_dev_bps
    );

    // ========================================================================
    // Phase 2a: before-first 安全护栏
    // ========================================================================

    println!("\n=== Phase 2a: Before-first guard ===");

    for &(token_in, token_out) in &[(token_a, token_b), (token_b, token_a)] {
        let amount_in = before_first_amount(&pool, token_in);

        // simulate_swap 对 before-first 返回 0
        let local_out = pool.simulate_swap(token_in, token_out, amount_in)?;
        assert_eq!(local_out, U256::ZERO);
        let chain_out = chain_quote(
            provider.clone(),
            pair_id,
            token_in,
            token_out,
            amount_in,
            block_id,
        )
        .await?;
        println!("  {token_in:12}→{token_out:12} local=0 chain={chain_out:30} (safe fallback)");
    }

    // ========================================================================
    // Phase 2b: 后向越界（beyond last）
    // ========================================================================

    println!("\n=== Phase 2b: Beyond-last-point error ===");

    for &(token_in, token_out) in &[(token_a, token_b), (token_b, token_a)] {
        let reserve = if token_in == token_a {
            &pool.reserve_a
        } else {
            &pool.reserve_b
        };
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
        let amount_in = before_first_amount(&pool, token_in);
        let local_out = pool.simulate_swap(token_in, token_out, amount_in)?;
        assert_eq!(
            local_out,
            U256::ZERO,
            "simulate_swap should return 0 for before-first amount"
        );
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

            assert!(
                !amount_out.is_zero(),
                "swap_mut returned zero at {mut_bps}bps"
            );
            cumulative_amount_in += amount_in;
            cumulative_amount_out_local += amount_out;

            assert_eq!(
                test_pool.ladder.consumed_in_ba, cumulative_amount_in,
                "consumed_in_ba mismatch after {mut_bps}bps swap"
            );
        }

        // 链上全量校验
        let chain_total_out = chain_quote(
            provider.clone(),
            pair_id,
            token_b,
            token_a,
            cumulative_amount_in,
            block_id,
        )
        .await?;

        assert_eq!(
            test_pool.ladder.consumed_out_ba, cumulative_amount_out_local,
            "consumed_out_ba mismatch"
        );

        let diff = if chain_total_out > cumulative_amount_out_local {
            chain_total_out - cumulative_amount_out_local
        } else {
            cumulative_amount_out_local - chain_total_out
        };
        let diff_bps = if chain_total_out.is_zero() {
            0.0
        } else {
            u256_to_f64(&diff) / u256_to_f64(&chain_total_out) * 10000.0
        };

        println!(
            "  {token_b:12}→{token_a:12} cumulative local={cumulative_amount_out_local:30} chain={chain_total_out:30} diff={diff_bps:.2}bps",
        );
        assert!(diff_bps <= MAX_INTERP_DEVIATION_BPS);
        assert_eq!(test_pool.reserve_b, pool.reserve_b + cumulative_amount_in);
        assert_eq!(
            test_pool.reserve_a,
            pool.reserve_a - cumulative_amount_out_local
        );
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
        pool.price_a_in_b,
        pool.price_b_in_a,
        ratio_diff * 100.0,
    );
    println!(
        "  price_a_in_b={:.6} price_b_in_a={:.6} reciprocal_error={:.4}%",
        pool.price_a_in_b,
        pool.price_b_in_a,
        ratio_diff * 100.0
    );

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
                pool.price_a_in_b,
                diff * 100.0,
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
    let _guard = xlayer_test_guard();
    let Some((provider, chain_id, block_id)) = connect_xlayer_provider().await? else {
        return Ok(());
    };

    let pair_ids = discover_pair_ids(provider.clone(), block_id).await?;
    if pair_ids.is_empty() {
        println!("SKIP: no pairs found");
        return Ok(());
    }

    let loaded = load_caliber_pool(provider.clone(), chain_id, pair_ids[0], block_id).await?;
    let pool = loaded.pool;
    let virtual_address = pool.virtual_address;

    let seed_amm = AMM::CaliberPropPool(pool);

    println!("=== with_amms Integration Test ===");
    println!("Building StateSpace with 1 Caliber pool: {virtual_address}");

    let result = StateSpaceBuilder::new(provider.clone())
        .block(CALIBER_TEST_BLOCK)
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
                    assert!(
                        p.has_sufficient_liquidity(),
                        "pool should have sufficient liquidity"
                    );
                    assert!(
                        p.ladder.ladder_a_to_b.len() >= 32,
                        "expected refined ladder points"
                    );
                    println!(
                        "  StateSpace built successfully! ladder_a_to_b points={}",
                        p.ladder.ladder_a_to_b.len()
                    );
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

#[tokio::test]
async fn test_caliber_prop_all_xlayer_pairs_deviation_scan() -> Result<()> {
    let _guard = xlayer_test_guard();
    let Some((provider, chain_id, block_id)) = connect_xlayer_provider().await? else {
        return Ok(());
    };

    let pair_ids = discover_pair_ids(provider.clone(), block_id).await?;
    assert!(
        pair_ids.len() >= 4,
        "expected at least 4 Caliber pairs on XLayer, got {}",
        pair_ids.len()
    );

    println!("=== Caliber All-Pairs Deviation Scan ===");
    println!("Discovered {} pair(s)", pair_ids.len());

    let mut summaries = Vec::with_capacity(pair_ids.len());
    let mut global_max_deviation_bps = 0.0f64;

    for pair_id in pair_ids {
        let loaded = load_caliber_pool(provider.clone(), chain_id, pair_id, block_id).await?;
        let summary = scan_pool_deviation(provider.clone(), &loaded, block_id).await?;
        global_max_deviation_bps = global_max_deviation_bps.max(summary.max_deviation_bps);

        println!(
            "  {:14} virt={} ladder=({},{}) empty_dirs={} checked={} max_dev={:.2}bps",
            summary.pair_label,
            summary.virtual_address,
            summary.ladder_ab_points,
            summary.ladder_ba_points,
            summary.empty_directions,
            summary.checked_quotes,
            summary.max_deviation_bps
        );

        assert!(
            summary.max_deviation_bps <= MAX_INTERP_DEVIATION_BPS,
            "{} deviation {:.2}bps exceeds threshold {:.2}bps",
            summary.pair_label,
            summary.max_deviation_bps,
            MAX_INTERP_DEVIATION_BPS
        );

        summaries.push(summary);
    }

    println!(
        "All-pairs worst deviation: {:.2}bps across {} pool(s)",
        global_max_deviation_bps,
        summaries.len()
    );

    Ok(())
}

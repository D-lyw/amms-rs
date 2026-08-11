//! Caliber propAMM — 全链路精确复刻验证（XLayer 链上 Fork 测试）
//!
//! ## 验证范围
//!
//! 1. **池子发现 & 初始化** — getAllPairIds + `fetch_exact_snapshot` 直读存储
//!    （cfg/data/ladder，含 `cfg+7` pos）
//! 2. **正向/反向报价逐位对照** — `simulate_swap` vs 链上 `batchQuote()`，
//!    固定金额列表（含小额 before-first 与跨段大额），**逐位一致**（0 偏差）
//! 3. **Consumed 追踪 + simulate_swap_mut** — 连续多笔 swap 的累计输出
//!    与链上累计输入报价逐位一致
//! 4. **StateSpace 集成** — with_amms 构建
//! 5. **全量 pair 扫描** — 所有 pair 逐位对照
//!
//! ## 运行
//!
//! ```bash
//! XLAYER_PROVIDER=http://127.0.0.1:8557 CALIBER_TEST_BLOCK=66309105 \
//!   cargo test -p amms --test caliber_prop
//! ```
//!
//! ## 参考合约
//!
//! - XLayer: `0x154586B2479b9a11e3d4db90024Dc0e26F097312`
//! - 报价公式与存储布局：`docs/caliber_prop_internal.md`
//! - 向量来源：`docs/caliber_prop_re/model.py`（4 pair × 双向 × 14 金额，
//!   块 66309105 链上 eth_call 验证零 DIFF）

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
    checked_quotes: usize,
}

// ============================================================
// Constants
// ============================================================

const XLAYER_CHAIN_ID: u64 = 196;
const CALIBER_CONTRACT: Address = address!("154586b2479b9a11e3d4db90024dc0e26f097312");
const CALIBER_TEST_BLOCK: u64 = 66_309_105;

/// 逐位对照的固定金额列表（= docs/caliber_prop_re/ 的 Python 验证集，
/// 覆盖 before-first 小额、段内、跨段大额）。
const QUOTE_AMOUNTS: &[u64] = &[
    1,
    2,
    3,
    5,
    10,
    50,
    100,
    1_000,
    10_000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
    1_000_000_000,
];

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

fn xlayer_test_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
            fot_tax: None,
        },
        token_b: amms::amms::Token {
            address: token_b,
            decimals: decimals_b,
            symbol: symbol_b.clone(),
            chain_id,
            fot_tax: None,
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

/// 单 pair 双向固定金额逐位对照
async fn scan_pool_exact<P>(
    provider: P,
    loaded: &LoadedCaliberPool,
    block_id: BlockId,
) -> Result<PoolDeviationSummary>
where
    P: Provider + Clone,
{
    let pool = &loaded.pool;
    let mut requests: Vec<(Address, Address, U256)> = Vec::new();
    for &(token_in, token_out) in &[
        (pool.token_a.address, pool.token_b.address),
        (pool.token_b.address, pool.token_a.address),
    ] {
        for &amt in QUOTE_AMOUNTS {
            requests.push((token_in, token_out, U256::from(amt)));
        }
    }
    let chain_quotes = chain_quotes_batch(provider, pool.pair_id, &requests, block_id).await?;
    assert_eq!(requests.len(), chain_quotes.len());

    let mut checked_quotes = 0usize;
    for (i, &(token_in, token_out, amount_in)) in requests.iter().enumerate() {
        let chain_out = chain_quotes[i];
        let local_out = pool.simulate_swap(token_in, token_out, amount_in)?;
        assert_eq!(
            local_out, chain_out,
            "quote mismatch for {} / {} ({token_in}->{token_out}) amount={amount_in}",
            loaded.symbol_a, loaded.symbol_b
        );
        checked_quotes += 1;
    }

    Ok(PoolDeviationSummary {
        pair_label: format!("{}/{}", loaded.symbol_a, loaded.symbol_b),
        virtual_address: pool.virtual_address,
        ladder_ab_points: pool.ladder.ladder_a_to_b.len(),
        ladder_ba_points: pool.ladder.ladder_b_to_a.len(),
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

/// 从合约 storage 读取 pair 的 token 地址（cfg 基址 keccak256(pairId||6)）。
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
// 主测试：全链路精确验证
// ============================================================

#[tokio::test]
async fn test_caliber_prop_full_verification() -> Result<()> {
    let _guard = xlayer_test_guard();
    let Some((provider, chain_id, block_id)) = connect_xlayer_provider().await? else {
        return Ok(());
    };

    // ========================================================================
    // Phase 0: 池子发现 & 初始化（fetch_exact_snapshot 直读存储）
    // ========================================================================

    println!("=== Phase 0: Discover & Initialize ===");

    let pair_ids = discover_pair_ids(provider.clone(), block_id).await?;
    println!("Found {} pair(s)", pair_ids.len());
    assert!(!pair_ids.is_empty(), "no Caliber pairs found on XLayer");

    let loaded = load_caliber_pool(provider.clone(), chain_id, pair_ids[0], block_id).await?;
    let pool = loaded.pool.clone();
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
        "  quote params: field0={} field1={} fee={} window={} scale={} pos_reverse={} pos_forward={}",
        pool.ladder.field0,
        pool.ladder.field1,
        pool.ladder.fee_rate,
        pool.ladder.window,
        pool.ladder.scale,
        pool.ladder.pos_reverse,
        pool.ladder.pos_forward,
    );
    println!(
        "  price_a_in_b={:.6} price_b_in_a={:.6}",
        pool.price_a_in_b, pool.price_b_in_a
    );

    // ========================================================================
    // Phase 1: 双向固定金额逐位对照
    // ========================================================================

    println!("\n=== Phase 1: Exact quote comparison (simulate_swap vs batchQuote) ===");

    let summary = scan_pool_exact(provider.clone(), &loaded, block_id).await?;
    println!(
        "  {} checked={} quotes, ALL bit-exact",
        summary.pair_label, summary.checked_quotes
    );

    // ========================================================================
    // Phase 2: Consumed 追踪 + simulate_swap_mut（累计输出逐位对照）
    // ========================================================================

    println!("\n=== Phase 2: Sequential consume tracking ===");

    {
        let mut test_pool = pool.clone();
        // 用三个递增金额走 consumed 路径。链上反向报价在 pos 消耗殆尽后，
        // 小额输出为 0 属正常（本地与链上一致），所以先探测哪个方向非零。
        let swap_amounts = [
            U256::from(10_000u64),
            U256::from(100_000u64),
            U256::from(1_000_000u64),
        ];

        // 探测：两个方向 × 三档金额，取全部非零的第一个方向
        let probes: Vec<(Address, Address)> = vec![(token_a, token_b), (token_b, token_a)];
        let mut chosen: Option<(Address, Address)> = None;
        for (t_in, t_out) in probes {
            let chain_outs = chain_quotes_batch(
                provider.clone(),
                pair_id,
                &swap_amounts
                    .iter()
                    .map(|&amt| (t_in, t_out, amt))
                    .collect::<Vec<_>>(),
                block_id,
            )
            .await?;
            if chain_outs.iter().all(|&out| !out.is_zero()) {
                chosen = Some((t_in, t_out));
                break;
            }
        }
        let (token_in, token_out) =
            chosen.expect("no direction with non-zero chain quote for sequential tracking");

        let mut cumulative_amount_in = U256::ZERO;
        let mut cumulative_amount_out_local = U256::ZERO;
        let mut prev_chain_total = U256::ZERO;

        for amount_in in swap_amounts {
            let amount_out = test_pool.simulate_swap_mut(token_in, token_out, amount_in)?;
            assert!(!amount_out.is_zero(), "swap_mut returned zero");
            cumulative_amount_in += amount_in;
            cumulative_amount_out_local += amount_out;

            // 链上对照：quote(累计输入) - quote(上一累计输入) 应等于本笔本地输出（逐位一致）
            let chain_total = chain_quote(
                provider.clone(),
                pair_id,
                token_in,
                token_out,
                cumulative_amount_in,
                block_id,
            )
            .await?;
            let chain_leg = chain_total - prev_chain_total;
            assert_eq!(
                amount_out, chain_leg,
                "per-leg output mismatch: cumulative_in={cumulative_amount_in}"
            );
            prev_chain_total = chain_total;
            println!(
                "  {token_in:?}→{token_out:?} amount_in={amount_in:12} amount_out={amount_out:30} leg_exact"
            );
        }

        // consumed 计数器与累计值一致
        let (consumed_in, consumed_out) = if token_in == token_b {
            (
                test_pool.ladder.consumed_in_ba,
                test_pool.ladder.consumed_out_ba,
            )
        } else {
            (
                test_pool.ladder.consumed_in_ab,
                test_pool.ladder.consumed_out_ab,
            )
        };
        assert_eq!(consumed_in, cumulative_amount_in, "consumed_in mismatch");
        assert_eq!(
            consumed_out, cumulative_amount_out_local,
            "consumed_out mismatch"
        );
        assert_eq!(
            cumulative_amount_out_local, prev_chain_total,
            "consumed cumulative output mismatch vs chain"
        );
        println!(
            "  cumulative local={cumulative_amount_out_local:30} chain={prev_chain_total:30} bit-exact"
        );

        // 储备按方向更新
        if token_in == token_b {
            assert_eq!(test_pool.reserve_b, pool.reserve_b + cumulative_amount_in);
            assert_eq!(
                test_pool.reserve_a,
                pool.reserve_a - cumulative_amount_out_local
            );
        } else {
            assert_eq!(test_pool.reserve_a, pool.reserve_a + cumulative_amount_in);
            assert_eq!(
                test_pool.reserve_b,
                pool.reserve_b - cumulative_amount_out_local
            );
        }
    }

    // ========================================================================
    // Phase 3: 价格缓存合理性
    // ========================================================================

    println!("\n=== Phase 3: Spot price sanity ===");

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
                        p.ladder.ladder_a_to_b.len() >= 3,
                        "expected original ladder points"
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

// ============================================================
// 全量 pair 精确扫描
// ============================================================

#[tokio::test]
async fn test_caliber_prop_all_xlayer_pairs_exact_scan() -> Result<()> {
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

    println!("=== Caliber All-Pairs Exact Scan ===");
    println!("Discovered {} pair(s)", pair_ids.len());

    let mut summaries = Vec::with_capacity(pair_ids.len());
    let mut total_checked = 0usize;

    for pair_id in pair_ids {
        let loaded = load_caliber_pool(provider.clone(), chain_id, pair_id, block_id).await?;
        let summary = scan_pool_exact(provider.clone(), &loaded, block_id).await?;
        total_checked += summary.checked_quotes;

        println!(
            "  {:14} virt={} ladder=({},{}) checked={} bit-exact",
            summary.pair_label,
            summary.virtual_address,
            summary.ladder_ab_points,
            summary.ladder_ba_points,
            summary.checked_quotes,
        );

        summaries.push(summary);
    }

    println!(
        "All-pairs exact: {} quotes across {} pool(s), zero deviation",
        total_checked,
        summaries.len()
    );

    Ok(())
}

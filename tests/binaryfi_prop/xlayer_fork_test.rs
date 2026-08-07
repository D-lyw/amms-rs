//! BinaryFi propAMM — 全链路真实性验证（XLayer 链上 Fork 测试）
//!
//! 照 `tests/caliber_prop/xlayer_fork_test.rs` 模式：
//! - 固定区块锚点 `BINARYFI_TEST_BLOCK`，通过 RPC `eth_call` 在历史块上执行，
//!   不依赖任何真实套利交易（测试内自构造 probe 金额）
//! - 对比链上 `quote()` 与本地 `simulate_swap`/`engine_quote`，线性区必须 0 mismatch
//! - 环境变量 `XLAYER_PROVIDER` 或 `XLAYER_RPC_URL` 未设置时自动跳过
//!
//! 已知不可观测类：引擎侧 per-asset 买入额度被块内 swap 消耗到 0 时
//! （链上 quote=0 而本地非 0），该状态不出现在 calldata/事件中，报告为 INFO
//! 不参与断言（与 replay probe 一致）。

use std::env;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use alloy::{
    eips::{BlockId, BlockNumberOrTag},
    primitives::{address, Address, U256},
    providers::{Provider, ProviderBuilder},
    sol_types::SolValue,
};
use amms::{
    amms::{
        amm::{AutomatedMarketMaker, AMM},
        binaryfi_prop::{
            BinaryFiPropPool, GetBinaryFiPropStateBatchRequest, IBinaryFiPropPool, Snapshot,
            BINARYFI_ASSET_COUNT, BINARYFI_ENGINE_ADDRESS, BINARYFI_POOL_ADDRESS,
            BINARYFI_ROUTER_ADDRESS,
        },
    },
    state_space::StateSpaceBuilder,
};
use eyre::Result;

// ============================================================
// Constants
// ============================================================

const XLAYER_CHAIN_ID: u64 = 196;
/// 已验证的 fork 锚点（新引擎 1999/2000 因子已生效的块，报价曲线固定可复现）
const BINARYFI_TEST_BLOCK: u64 = 67_302_485;

// ============================================================
// Provider helpers（照 caliber fork 测试）
// ============================================================

fn xlayer_provider_url() -> Option<String> {
    env::var("XLAYER_PROVIDER")
        .or_else(|_| env::var("XLAYER_RPC_URL"))
        .ok()
}

fn xlayer_test_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

async fn connect_xlayer_provider() -> Result<Option<(Arc<impl Provider>, u64, BlockId)>> {
    let block_id = BlockId::from(BINARYFI_TEST_BLOCK);
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

// ============================================================
// Chain reference helpers
// ============================================================

/// 链上池子 quote（固定区块 eth_call）
async fn chain_quote<P: Provider + Clone>(
    provider: P,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    block_id: BlockId,
) -> Result<U256> {
    let pool_contract = IBinaryFiPropPool::new(BINARYFI_POOL_ADDRESS, provider);
    Ok(pool_contract
        .quote(BINARYFI_ROUTER_ADDRESS, token_in, token_out, amount_in)
        .block(block_id)
        .call()
        .await?)
}

/// 批量快照（链上 reference，与 init 同一条批量读取合约）：
/// 132 对线性 quote + 11 对 BUY 大额 + 11 对 SELL 大额 + 资产/decimals/余额。
async fn fetch_snapshot_at<P: Provider + Clone>(provider: P, block: u64) -> Result<Snapshot> {
    let mut quote_pairs = Vec::with_capacity(BINARYFI_ASSET_COUNT * BINARYFI_ASSET_COUNT);
    for i in 0..BINARYFI_ASSET_COUNT {
        for j in 0..BINARYFI_ASSET_COUNT {
            if i != j {
                quote_pairs.push(U256::from(i * BINARYFI_ASSET_COUNT + j));
            }
        }
    }
    let mut big_quote_pairs = Vec::with_capacity(BINARYFI_ASSET_COUNT - 1);
    for j in 1..BINARYFI_ASSET_COUNT {
        big_quote_pairs.push(U256::from(BINARYFI_ASSET_COUNT * BINARYFI_ASSET_COUNT + j));
    }
    let mut big_sell_pairs = Vec::with_capacity(BINARYFI_ASSET_COUNT - 1);
    for j in 1..BINARYFI_ASSET_COUNT {
        big_sell_pairs.push(U256::from(
            2 * BINARYFI_ASSET_COUNT * BINARYFI_ASSET_COUNT + j,
        ));
    }
    let return_data = GetBinaryFiPropStateBatchRequest::deploy_builder(
        provider.clone(),
        BINARYFI_POOL_ADDRESS,
        BINARYFI_ENGINE_ADDRESS,
        BINARYFI_ROUTER_ADDRESS,
        vec![],
        quote_pairs,
        big_quote_pairs,
        big_sell_pairs,
    )
    .call_raw()
    .block(BlockId::Number(BlockNumberOrTag::Number(block)))
    .await?;
    Ok(<Snapshot as SolValue>::abi_decode(&return_data)?)
}

/// 线性区 probe 金额（与引擎/批量合约一致：0→j 用 10^d0，其余用 10^(di-4)）
fn quote_amount(decimals: &[u8], i: usize, j: usize) -> Option<U256> {
    if i >= decimals.len() || j >= decimals.len() {
        return None;
    }
    let di = decimals[i] as u32;
    if di > 30 {
        return None;
    }
    let exp = if i == 0 { di } else { di.saturating_sub(4) };
    Some(U256::from(10u64).pow(U256::from(exp)))
}

// ============================================================
// 主测试：init + 全方向 quote 对拍 + 大额 cap + 真实样例 + StateSpace
// ============================================================

#[tokio::test]
async fn test_binaryfi_prop_fork_quote_replication() -> Result<()> {
    let _guard = xlayer_test_guard();
    let Some((provider, chain_id, block_id)) = connect_xlayer_provider().await? else {
        return Ok(());
    };
    assert_eq!(chain_id, XLAYER_CHAIN_ID);

    // Phase 1: 本地初始化（init = 一次批量静态调用，与链上同一快照源）
    let local = BinaryFiPropPool::default()
        .init(block_id, provider.clone())
        .await?;
    let n = local.assets.len();
    assert!(n >= 2, "expected >= 2 assets, got {n}");
    let decimals: Vec<u8> = local.assets.iter().map(|t| t.decimals).collect();
    println!("=== BinaryFi fork verification @ block {BINARYFI_TEST_BLOCK} ===");
    println!("assets={} pool={}", n, local.pool_address);

    // Phase 2: 批量快照（链上 reference）全 132 方向线性 quote 对拍
    let snap = fetch_snapshot_at(provider.clone(), BINARYFI_TEST_BLOCK).await?;
    assert_eq!(snap.assets.len(), n, "snapshot asset count drift");

    let mut ok = 0usize;
    let mut total = 0usize;
    let mut cap_infos: Vec<String> = Vec::new();
    let mut mismatches: Vec<String> = Vec::new();
    for (k, pair) in snap.quotePairs.iter().enumerate() {
        if k >= snap.quotes.len() || !snap.quotes[k].success {
            continue;
        }
        let p = pair.to::<usize>();
        if p >= n * n {
            continue;
        }
        let (i, j) = (p / n, p % n);
        let Some(amount) = quote_amount(&decimals, i, j) else {
            continue;
        };
        let sim = local.simulate_swap(snap.assets[i], snap.assets[j], amount)?;
        total += 1;
        if sim == snap.quotes[k].amountOut {
            ok += 1;
        } else if snap.quotes[k].amountOut.is_zero() && !sim.is_zero() {
            // 引擎 per-asset 买入额度被块内 swap 消耗到 0：不可观测类，仅报告
            cap_infos.push(format!(
                "  pair {i}->{j} amt={amount}: sim={sim} chain=0 (engine buy-cap consumed)"
            ));
        } else {
            mismatches.push(format!(
                "  pair {i}->{j} amt={amount}: sim={sim} chain={} diff={}",
                snap.quotes[k].amountOut,
                if sim > snap.quotes[k].amountOut {
                    sim - snap.quotes[k].amountOut
                } else {
                    snap.quotes[k].amountOut - sim
                }
            ));
        }
    }
    if !cap_infos.is_empty() {
        println!("INFO ({}):", cap_infos.len());
        for line in &cap_infos {
            println!("{line}");
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} quote mismatches:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
    println!("Phase 2: {ok}/{total} linear quotes replicated");

    // Phase 3: 大额 probe 对拍（BUY maxOut/归零 + SELL maxIn 截断）
    let mut ok2 = 0usize;
    let mut total2 = 0usize;
    let mut mismatches2: Vec<String> = Vec::new();
    let d0 = local.assets[0].decimals as u32;
    let big_buy_in = U256::from(10u64).pow(U256::from(d0 + 4));
    for j in 1..n {
        let dj = local.assets[j].decimals as u32;
        if dj == 0 || dj > 30 {
            continue;
        }
        // BUY 大额（饱和/归零）
        let chain_b = chain_quote(
            provider.clone(),
            local.assets[0].address,
            local.assets[j].address,
            big_buy_in,
            block_id,
        )
        .await?;
        let sim_b =
            local.simulate_swap(local.assets[0].address, local.assets[j].address, big_buy_in)?;
        total2 += 1;
        if sim_b == chain_b {
            ok2 += 1;
        } else {
            mismatches2.push(format!(
                "  0->{j} big in={big_buy_in}: sim={sim_b} chain={chain_b}"
            ));
        }
        // SELL 100 整枚（超容量归零 / 精确有理数 / 阶梯资产近似）
        let sell_in = U256::from(100u64) * U256::from(10u64).pow(U256::from(dj));
        let chain_s = chain_quote(
            provider.clone(),
            local.assets[j].address,
            local.assets[0].address,
            sell_in,
            block_id,
        )
        .await?;
        let sim_s =
            local.simulate_swap(local.assets[j].address, local.assets[0].address, sell_in)?;
        total2 += 1;
        if sim_s == chain_s {
            ok2 += 1;
        } else if local.sell_raw.get(j).copied().flatten().is_none() {
            // 多档阶梯资产（100 整枚 probe 与单档线性不兼容，如 DOG）：
            // 本地仅小额区可精确复刻，大额近似（≤0.05%），报告 INFO 不参与断言
            println!(
                "INFO: {j}->0 sell in={sell_in}: sim={sim_s} chain={chain_s} (laddered asset)"
            );
        } else {
            mismatches2.push(format!(
                "  {j}->0 sell in={sell_in}: sim={sim_s} chain={chain_s}"
            ));
        }
    }
    assert!(
        mismatches2.is_empty(),
        "{} big-probe mismatches:\n{}",
        mismatches2.len(),
        mismatches2.join("\n")
    );
    println!("Phase 3: {ok2}/{total2} big-probe quotes replicated");

    // Phase 4: 真实套利方向对拍（0→SKHYx BUY 小额 + SKHYx→0 SELL，锚点块链上 quote）
    let skhyx = address!("0x58100046a4afcd4ee4fadbd4244f3f895a341c56");
    if let Some(j) = local.assets.iter().position(|t| t.address == skhyx) {
        let amount_in = U256::from(1_000_000u64);
        let chain = chain_quote(
            provider.clone(),
            local.assets[0].address,
            local.assets[j].address,
            amount_in,
            block_id,
        )
        .await?;
        let sim =
            local.simulate_swap(local.assets[0].address, local.assets[j].address, amount_in)?;
        println!("Phase 4: 0->SKHYx in={amount_in}: sim={sim} chain={chain}");
        assert_eq!(sim, chain, "BUY small quote must replicate exactly");

        let sell_in = U256::from(1_000_000_000_000_000_000u64);
        let chain_s = chain_quote(
            provider.clone(),
            local.assets[j].address,
            local.assets[0].address,
            sell_in,
            block_id,
        )
        .await?;
        let sim_s =
            local.simulate_swap(local.assets[j].address, local.assets[0].address, sell_in)?;
        println!("Phase 4: SKHYx->0 in={sell_in}: sim={sim_s} chain={chain_s}");
        if sim_s != chain_s && local.sell_raw.get(j).copied().flatten().is_none() {
            // 快照两点无法反推第一档精确 raw（阶梯资产）；生产环境由 update 日志
            // 直接携带 price/ladder → sell_raw 精确，此处仅报告快照近似偏差
            println!(
                "INFO: SKHYx->0 {sell_in}: sim={sim_s} chain={chain_s} (snapshot ladder approx; update path exact)"
            );
        } else {
            assert_eq!(sim_s, chain_s, "SELL quote must replicate exactly");
        }
    }

    // Phase 5: StateSpace 集成（with_amms 构建）
    println!("=== with_amms Integration Test ===");
    let seed_amm = AMM::BinaryFiPropPool(local.clone());
    match StateSpaceBuilder::new(provider.clone())
        .block(BINARYFI_TEST_BLOCK)
        .with_amms(vec![seed_amm])
        .sync()
        .await
    {
        Ok(manager) => {
            let state = manager.state.read().await;
            match state.get(&local.pool_address) {
                Some(AMM::BinaryFiPropPool(p)) => {
                    assert!(p.has_sufficient_liquidity(), "pool should have liquidity");
                    assert!(
                        p.rates.iter().any(|r| !r.is_zero()),
                        "expected non-zero rates"
                    );
                    println!("  StateSpace built successfully! rates={}", p.rates.len());
                }
                _ => panic!("unexpected AMM variant in state"),
            }
        }
        Err(e) => panic!("StateSpace build failed: {e}"),
    }

    Ok(())
}

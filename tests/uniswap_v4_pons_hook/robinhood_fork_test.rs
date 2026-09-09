//! Pons V2 Meme Hook — Robinhood Chain 4663 主网 fork 对照测试
//!
//! 照 `tests/elfomo_prop/xlayer_fork_test.rs` 模式：固定历史块锚点，通过 RPC
//! `eth_call` / 历史块存储读取在锚点块执行，不依赖任何真实套利交易；把“本地
//! AMMS 模拟 + `V4HookFee` 白名单建模”与链上真实成交逐位对拍。
//!
//! ## 链上事实（2026-09-09 取证，均已核验）
//!
//! 真实池：`PoolRegistered` 池 0x8f9377be…（Pons V2 毕业池）
//! - currency0 = 原生 ETH（address(0)）、currency1 = 0x39af21c5…（meme，18 位）
//! - fee = 0、tickSpacing = 200、hooks = PonsV2MemeHook 0xE5e70264…
//! - 毕业块 0x3767270 = 58_094_192（PoolGraduated + 永久锁定流动性）
//! - launches(poolId)：hookFeeBps = 100（1%）、creatorTaxBps = 400（每池冻结）
//!
//! 真实首笔 swap（块 0x3767271 = 58_094_193，tx 0x91ebb0a9…）为 **exact-in**：
//! 用户支付 ETH 0x1c9749ad205365（Swap amount0 = −0x1c9749ad205365，caller 视角），
//! 池核心毛出 meme 0x52a5e1e8c81d184b62b0（Swap amount1 = 池核心毛出）。
//! PonsV2MemeHook 在 exact-in 的**输出侧**（meme）分开计提：
//! - HookFeeCollected feeAmount = 0xd3942dd90a87ec461b = floor(gross×100/10000)
//! - HookFeeCollected taxAmount = 0x34e50b7642a1fb1186d = floor(gross×400/10000)
//! - hook 实收 = feeAmount + taxAmount（0x421e4e53d34a79d5e88，单次 take）
//! 用户净得 meme = gross − fee − tax = 0x4e83fd038ae870ae0428（收据中
//! PoolManager → 用户 的 meme Transfer，逐位一致）。
//!
//! ## 运行
//!
//! ```bash
//! ROBINHOOD_RPC_URL=https://robinhood-mainnet.core.chainstack.com/<token> \
//!   cargo test --test uniswap_v4_pons_hook -- --nocapture
//! ```
//! 需要 archive 节点（历史块 `eth_call`/存储读取）；未设置环境变量时自动跳过。

use std::env;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use alloy::{
    eips::{BlockId, BlockNumberOrTag},
    hex,
    primitives::{
        address,
        aliases::{I24, U24},
        b256, Address, B256, U256,
    },
    providers::{Provider, ProviderBuilder},
};
use amms::amms::amm::AutomatedMarketMaker;
use amms::amms::uniswap_v4::{
    hooks::V4HookFee, IPoolManager::PoolKey, UniswapV4Factory, UniswapV4Pool,
};
use eyre::Result;

const ROBINHOOD_CHAIN_ID: u64 = 4663;
const POOL_MANAGER: Address = address!("0x8366a39cc670b4001a1121b8f6a443a643e40951");
const PONS_V2_MEME_HOOK: Address = address!("0xE5e702641Ea86F4ae6cC3cDaeD2B886f976Be044");
const MEME_TOKEN: Address = address!("0x39af21c586b13337d0b83d8fc1ae7598f3297e95");
/// 毕业/锁仓块（池创建完成的块后状态）。
const SNAPSHOT_BLOCK: u64 = 58_094_192; // 0x3767270
/// 真实首笔 swap 所在块。
const SWAP_BLOCK: u64 = 58_094_193; // 0x3767271
/// PoolRegistered 事件给出的真实 PoolId。
const POOL_ID: B256 = b256!("8f9377be6e04a87110c3d1da76669f2fc8d5c40dc4784b65041cc16faa00174a");

// 链上成交事实（tx 0x91ebb0a9ac11c3e23cc573bcd35d1f9b5049118d0d2064055ebf37a06784375b）。
/// 用户 exact-in 支付的 ETH（currency0，raw）。
const CHAIN_ETH_IN: u128 = 0x1c9749ad205365;
/// 池核心毛出 meme（Swap amount1 = caller 收 gross，raw）。
const CHAIN_GROSS_MEME_OUT: u128 = 0x52a5e1e8c81d184b62b0;
/// 用户净得 meme（gross − fee − tax；收据 PoolManager→用户 Transfer）。
const CHAIN_NET_MEME_OUT: u128 = 0x4e83fd038ae870ae0428;
/// HookFeeCollected feeAmount（hookFeeBps = 100）。
const CHAIN_FEE_AMOUNT: u128 = 0xd3942dd90a87ec461b;
/// HookFeeCollected taxAmount（creatorTaxBps = 400）。
const CHAIN_TAX_AMOUNT: u128 = 0x34e50b7642a1fb1186d;
/// swap 后链上 slot0 sqrtPriceX96 / tick / liquidity（Swap 事件）。
const CHAIN_POST_SP: U256 = U256::from_be_slice(&hex!(
    "0000000000000000000000000000000000001b2d623bf3de12dd28af03bc464f"
));
const CHAIN_POST_TICK: i32 = 176_960;
const CHAIN_POST_LIQ: u128 = 29277002188455995815305;

/// Robinhood archive RPC URL（历史块 eth_call 需要 archive）。
fn robinhood_provider_url() -> Option<String> {
    env::var("ROBINHOOD_PROVIDER")
        .or_else(|_| env::var("ROBINHOOD_RPC_URL"))
        .ok()
}

fn test_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

async fn connect_provider() -> Result<Option<(Arc<impl Provider + Clone>, u64)>> {
    let rpc_url = match robinhood_provider_url() {
        Some(url) => url,
        None => {
            println!("SKIP: ROBINHOOD_PROVIDER/ROBINHOOD_RPC_URL not set");
            return Ok(None);
        }
    };
    let provider = Arc::new(ProviderBuilder::new().connect_http(rpc_url.parse()?));
    let chain_id = provider.get_chain_id().await?;
    if chain_id != ROBINHOOD_CHAIN_ID {
        println!("SKIP: expected robinhood chain_id {ROBINHOOD_CHAIN_ID}, got {chain_id}");
        return Ok(None);
    }
    Ok(Some((provider, chain_id)))
}

fn pons_hook_fee() -> V4HookFee {
    // PonsV2MemeHook：双向同率、每池冻结，组件 = [hookFeeBps, creatorTaxBps]
    // （分开向下取整，与链上 feeAmount/taxAmount 分开计提一致）。
    V4HookFee::AfterSwapProportional {
        zero_for_one_bps: vec![100, 400],
        one_for_zero_bps: vec![100, 400],
    }
}

fn expected_pool_key() -> PoolKey {
    PoolKey {
        currency0: Address::ZERO,
        currency1: MEME_TOKEN,
        fee: U24::from(0u64),
        tickSpacing: I24::try_from(200).unwrap(),
        hooks: PONS_V2_MEME_HOOK,
    }
}

/// 主测试：真实 Pons V2 池 × 真实首笔 exact-in swap 逐位对拍。
#[tokio::test]
async fn test_pons_v2_hook_robinhood_fork_exact_in_replication() -> Result<()> {
    let _guard = test_guard();
    let Some((provider, _)) = connect_provider().await? else {
        return Ok(());
    };
    let snapshot = BlockId::Number(BlockNumberOrTag::Number(SNAPSHOT_BLOCK));

    // 池 key → PoolId 推导必须与链上 PoolRegistered 一致。
    let pool = UniswapV4Pool::new(POOL_MANAGER, expected_pool_key());
    assert_eq!(
        pool.pool_id, POOL_ID,
        "PoolId 推导与链上 PoolRegistered 不一致"
    );

    // 锚点块（毕业块后状态）快照：slot0 / 流动性 / tick bitmap / tick 数据。
    let mut pools = vec![pool];
    let failed = UniswapV4Factory::sync_slot_0(&mut pools, snapshot, provider.clone()).await?;
    assert!(failed.is_empty(), "sync_slot_0 failed: {failed:?}");
    let failed = UniswapV4Factory::sync_tick_bitmap(&mut pools, snapshot, provider.clone()).await?;
    assert!(failed.is_empty(), "sync_tick_bitmap failed: {failed:?}");
    let failed = UniswapV4Factory::sync_tick_data(&mut pools, snapshot, provider.clone()).await?;
    assert!(failed.is_empty(), "sync_tick_data failed: {failed:?}");
    let mut pool = pools.pop().unwrap();

    // 原生 ETH 与 meme 均为 18 位；仅影响展示价格，不影响 swap 数值模拟。
    pool.token_a.decimals = 18;
    pool.token_b.decimals = 18;

    println!(
        "=== PonsV2 hook fork verify @ block {SNAPSHOT_BLOCK} -> swap @ block {SWAP_BLOCK} ==="
    );
    println!(
        "pool_id={} tick={} liquidity={} sqrt_price={}",
        pool.pool_id, pool.tick, pool.liquidity, pool.sqrt_price
    );

    let eth_in = U256::from(CHAIN_ETH_IN);
    let gross_meme = U256::from(CHAIN_GROSS_MEME_OUT);
    let net_meme = U256::from(CHAIN_NET_MEME_OUT);
    let chain_fee_total = U256::from(CHAIN_FEE_AMOUNT + CHAIN_TAX_AMOUNT);

    // 1) 无 hook 模型：本地核心 exact-in（ETH→meme）毛输出必须与链上 core delta 逐位一致。
    pool.hook_fee = V4HookFee::None;
    let gross = pool.simulate_swap(Address::ZERO, MEME_TOKEN, eth_in)?;
    assert_eq!(
        gross, gross_meme,
        "本地核心 exact-in 毛输出与链上 Swap amount1 不一致"
    );

    // 1b) 池核心状态推进与链上 Swap 事件（swap 后 slot0/tick/liquidity）逐位一致。
    let mut mut_pool = pool.clone();
    let gross_mut = mut_pool.simulate_swap_mut(Address::ZERO, MEME_TOKEN, eth_in)?;
    assert_eq!(gross_mut, gross_meme, "simulate_swap_mut 毛输出偏差");
    assert_eq!(
        mut_pool.sqrt_price, CHAIN_POST_SP,
        "swap 后 sqrtPrice 与链上不一致"
    );
    assert_eq!(mut_pool.tick, CHAIN_POST_TICK, "swap 后 tick 与链上不一致");
    assert_eq!(
        mut_pool.liquidity, CHAIN_POST_LIQ as u128,
        "swap 后 liquidity 与链上不一致"
    );

    // 2) 白名单 hook 模型：exact-in 在输出侧扣费，用户净得 = gross − fee − tax，
    //    与链上收据 PoolManager→用户 的 meme Transfer（net）逐位一致。
    pool.hook_fee = pons_hook_fee();
    let net = pool.simulate_swap(Address::ZERO, MEME_TOKEN, eth_in)?;
    assert_eq!(
        net, net_meme,
        "exact-in 净得（gross−hook 费）与链上用户实收不一致"
    );
    assert_eq!(
        net,
        gross.saturating_sub(chain_fee_total),
        "净得必须等于 gross − (feeAmount+taxAmount)"
    );

    // 3) 组件级分开向下取整与链上 feeAmount/taxAmount 单独计提逐位一致。
    let fee = pons_hook_fee();
    // 用户支付 currency0(ETH)→zeroForOne=true，费率组件取 zero_for_one_bps。
    assert_eq!(
        fee.total_fee(true, gross),
        chain_fee_total,
        "V4HookFee 合计费与链上 feeAmount+taxAmount 不一致"
    );
    assert_eq!(
        gross * U256::from(100u64) / U256::from(10_000u64),
        U256::from(CHAIN_FEE_AMOUNT),
        "hookFeeBps(100) 计提与链上 feeAmount 不一致"
    );
    assert_eq!(
        gross * U256::from(400u64) / U256::from(10_000u64),
        U256::from(CHAIN_TAX_AMOUNT),
        "creatorTaxBps(400) 计提与链上 taxAmount 不一致"
    );

    println!("gross={gross} net={net} chain_fee_total={chain_fee_total} -> ALL OK");
    Ok(())
}

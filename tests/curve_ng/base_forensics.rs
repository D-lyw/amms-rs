//! Base TwoCrypto 真实故障回归用例（45405235 -> 45405236）。
//!
//! 目标：
//! 1) 用链上真实事件重放验证 `sync` 后本地状态与链上最终状态一致；
//! 2) 用 A/B 对照证明旧顺序（先算 D 再写 price_scale）会显著偏离；
//! 3) 用真实输入量验证本地报价与链上 `get_dy` 的一致性。
//!
//! 注意：
//! - 这是“定点取证回归”测试，强保证该故障场景被修复；
//! - 它不是所有 CurveNG 池/事件组合的完备性证明。

use alloy::{
    eips::BlockId,
    primitives::{address, Address, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::Filter,
    sol,
    sol_types::SolEvent,
};
use amms::amms::{
    amm::{AutomatedMarketMaker, SyncAction},
    curve_ng::{CurveNGPool, CurveNGPoolType, ICurveTwoCryptoEvent},
};
use eyre::{eyre, Result};
use std::env;

use crate::common::quotes::assert_diff_within_ppm;

sol! {
    #[sol(rpc)]
    interface ICurveTwoCryptoReadonly {
        function D() external view returns (uint256);
        function price_scale() external view returns (uint256);
        function get_dy(uint256 i, uint256 j, uint256 dx) external view returns (uint256);
    }
}

fn base_provider_url() -> Option<String> {
    dotenv::dotenv().ok();
    env::var("BASE_PROVIDER")
        .or_else(|_| env::var("BASE_RPC_URL"))
        .or_else(|_| env::var("BASE_MAINNET_RPC_URL"))
        .ok()
}

fn diff_ppm(local: U256, chain: U256) -> U256 {
    if local == chain {
        return U256::ZERO;
    }
    if chain.is_zero() {
        return U256::MAX;
    }
    let diff = if local > chain {
        local - chain
    } else {
        chain - local
    };
    diff * U256::from(1_000_000u64) / chain
}

#[tokio::test]
async fn test_ng_twocrypto_base_state_consistency_after_single_sync_event() -> Result<()> {
    let rpc_url = match base_provider_url() {
        Some(url) => url,
        None => {
            println!("Skipping test: BASE_PROVIDER or BASE_RPC_URL not set");
            return Ok(());
        }
    };

    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    let pool_addr = address!("ba0c274085a078d19c46f2d902698a841cbfb289");
    let mut pool = CurveNGPool::new(pool_addr, CurveNGPoolType::TwoCrypto)
        .init(BlockId::from(45_405_235u64), provider.clone())
        .await?;

    let pool_view = ICurveTwoCryptoReadonly::new(pool_addr, provider.clone());
    let pre_d = pool_view
        .D()
        .block(BlockId::from(45_405_235u64))
        .call()
        .await?;
    let pre_price_scale = pool_view
        .price_scale()
        .block(BlockId::from(45_405_235u64))
        .call()
        .await?;

    assert_eq!(pool.d, Some(pre_d), "init D mismatch at block 45405235");
    assert_eq!(
        pool.price_scale,
        Some(vec![pre_price_scale]),
        "init price_scale mismatch at block 45405235"
    );

    // 固定抓取故障区块内该池 TokenExchange 事件。
    let filter = Filter::new()
        .address(pool_addr)
        .event_signature(vec![ICurveTwoCryptoEvent::TokenExchange::SIGNATURE_HASH])
        .from_block(45_405_236u64)
        .to_block(45_405_236u64);
    let logs = provider.get_logs(&filter).await?;

    let target_log = logs
        .iter()
        .find(|log| {
            log.transaction_index.unwrap_or_default() == 26
                && log.log_index.unwrap_or_default() == 99
        })
        .cloned()
        .ok_or_else(|| eyre!("target TokenExchange log not found at txIndex=26/logIndex=99"))?;

    let event = ICurveTwoCryptoEvent::TokenExchange::decode_log(&target_log.inner)?;
    let mask = U256::from(2).pow(U256::from(128)) - U256::from(1);
    let expected_price_scale = event.packed_price_scale & mask;

    // 反事实对照（旧错误顺序）：
    // 先用更新后的 balances 计算 D，再写入 event 的 price_scale。
    // 这会制造“旧 price_scale 口径 D + 新 price_scale”的混合状态。
    // 下面将用它和链上最终状态做直接对照，证明该顺序错误。
    let mut old_order_pool = pool.clone();
    let sold_i = event.sold_id.try_into().unwrap_or(usize::MAX);
    let bought_j = event.bought_id.try_into().unwrap_or(usize::MAX);
    if sold_i < old_order_pool.balances.len() {
        old_order_pool.balances[sold_i] += event.tokens_sold;
    }
    if bought_j < old_order_pool.balances.len() {
        old_order_pool.balances[bought_j] = old_order_pool.balances[bought_j]
            .checked_sub(event.tokens_bought)
            .ok_or_else(|| eyre!("counterfactual balance underflow"))?;
    }
    old_order_pool.recalculate_d()?;
    old_order_pool.price_scale = Some(vec![expected_price_scale]);

    let action = pool.sync(&target_log)?;
    assert_eq!(action, SyncAction::AsyncUpdate, "unexpected sync action");
    assert_eq!(
        pool.price_scale,
        Some(vec![expected_price_scale]),
        "local price_scale should equal event packed_price_scale low 128 bits"
    );

    // 关键断言：现顺序（先 price_scale 再 recalculate_d）应在 sync 后立刻对齐链上 D。
    // 如果回退到旧顺序，此断言会失败（见下方 old_order_pool 对照断言）。
    let post_d = pool_view
        .D()
        .block(BlockId::from(45_405_236u64))
        .call()
        .await?;
    assert_eq!(
        pool.d,
        Some(post_d),
        "local D must match chain D immediately after sync event"
    );
    assert_ne!(
        old_order_pool.d,
        Some(post_d),
        "old order (recalculate D before price_scale) should diverge from chain D"
    );

    let token_in: Address = address!("cfa3ef56d303ae4faaba0592388f19d7c3399fb4");
    let token_out: Address = address!("4da9a0f397db1397902070f93a4d6ddbc0e0e6e8");
    let amount_in = U256::from_str_radix("52273177478588853236", 10).unwrap();

    let local_out = pool.simulate_swap(token_in, token_out, amount_in)?;
    let old_order_out = old_order_pool.simulate_swap(token_in, token_out, amount_in)?;
    let chain_out = pool_view
        .get_dy(U256::from(1u8), U256::from(0u8), amount_in)
        .block(BlockId::from(45_405_236u64))
        .call()
        .await?;

    // 现顺序报价应贴近链上（允许极小舍入误差）。
    assert_diff_within_ppm(local_out, chain_out, 100);
    // 旧顺序报价应明显偏离链上（这里设置宽阈值，仅用于证明“偏离很大”）。
    let old_order_ppm = diff_ppm(old_order_out, chain_out);
    assert!(
        old_order_ppm > U256::from(100_000u64),
        "old order quote deviation should be very large, got {} ppm",
        old_order_ppm
    );

    Ok(())
}

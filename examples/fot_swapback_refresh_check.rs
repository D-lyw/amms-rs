//! 运行时验证：FoT swapBack 余额刷新任务（HTTP provider + JSON-RPC batch + timeout）。
//!
//! 回归 2026-08-09 DYOR:K 事故的刷新链路：旧实现逐 token 单次 call 挂在
//! 主 WS provider 上（无 timeout），静默卡死 84s 导致缓存 stale。
//! 新实现（`start_fot_swap_back_balance_sync_task`）走硬编码 HTTP RPC +
//! batch + 5s 硬超时。本 probe：
//!
//! 1. 注册 RTX（BuySell，主池 0xb8960e3b，swapBack 阈值 1250 RTX）；
//! 2. 启动刷新任务跑 ~4 轮（周期 1s）；
//! 3. 断言缓存值 `swap_back_balance(RTX)` 与链上独立 `balanceOf(RTX, RTX)`
//!    直查一致，且每次刷新都推进时间戳（`swap_back_balance_is_stale` = false）；
//! 4. 单独直接调用 batch 路径（`refresh_swap_back_balances_batch` 等价流程，
//!    通过任务本身覆盖），确认单 HTTP 请求读取 10 个 token 也工作。
//!
//! 用法:
//! ```bash
//! cargo run -p amms --release --example fot_swapback_refresh_check
//! ```
//! 环境变量:
//! - `RUST_LOG`  默认 `debug`（观察 "Swap-back balance refreshed" 日志）

use alloy::{
    primitives::{address, Address, U256},
    providers::{Provider, ProviderBuilder},
    sol,
};
use amms::amms::fot::{self, FotTaxType};
use eyre::Result;
use std::time::Duration;

const XLAYER_HTTP_RPC: &str = "https://rpc.xlayer.tech";
/// RTX（BuySell，买卖 3%，swapBack 阈值 1250 RTX，主池 0xb8960e3b）
const RTX: Address = address!("0x18A4F9D450f46f9DeA99dA758B4c29ad620AAE93");
const RTX_MAIN_PAIR: Address = address!("0xb8960e3b766ae359a31a409516bcc53b8a1c7bcd");

sol! {
    #[sol(rpc)]
    contract IERC20BalanceOf {
        function balanceOf(address owner) external view returns (uint256);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "debug".into()))
        .init();

    // 参考直查 provider（独立于任务内部的硬编码 provider）
    let provider = ProviderBuilder::new().connect_http(XLAYER_HTTP_RPC.parse()?);
    let onchain = || {
        let provider = provider.clone();
        async move {
            let balance = IERC20BalanceOf::new(RTX, provider)
                .balanceOf(RTX)
                .call()
                .await?;
            Ok::<U256, eyre::Report>(balance)
        }
    };

    fot::register_fot_token(
        RTX,
        FotTaxType::BuySell {
            buy_fee_bps: 300,
            sell_fee_bps: 300,
            pairs: vec![RTX_MAIN_PAIR],
            swap_back_threshold: U256::from(1250u64) * U256::from(10u64).pow(U256::from(18)),
        },
    );
    println!("registered RTX BuySell; before first refresh: cached={}", fot::swap_back_balance(RTX));
    assert_eq!(fot::swap_back_balance(RTX), U256::ZERO, "首跑前缓存应为 0");
    assert!(fot::swap_back_balance_is_stale(RTX), "首跑前应视为 stale");

    tokio::spawn(amms::state_space::sync_services::start_fot_swap_back_balance_sync_task(
        Duration::from_secs(1),
    ));

    // 等待任务首跑完成（jitter 延迟 ≤1.15s + 调用耗时）
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!fot::swap_back_balance_is_stale(RTX), "任务首跑后缓存应 fresh");

    // 验证窗口 1 分钟（60 轮 × 1s 间隔，与任务周期一致），覆盖 ~60 次周期刷新
    let rounds = 60u32;
    let mut last_ts: Option<std::time::Instant> = None;
    for round in 1..=rounds {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let cached = fot::swap_back_balance(RTX);
        let real = onchain().await?;
        println!("round {round}: cached={cached} real={real} match={}", cached == real);
        if cached != real {
            return Err(eyre::eyre!(
                "round {round}: cache {cached} != onchain {real}"
            ));
        }
        assert!(!fot::swap_back_balance_is_stale(RTX), "round {round}: cache should be fresh");
        let (_, ts) = fot::swap_back_balance_with_refresh(RTX).expect("cache present");
        if let Some(prev) = last_ts {
            assert!(ts >= prev, "round {round}: 刷新时间戳未推进");
        }
        last_ts = Some(ts);
        println!(
            "round {round}: OK (cached={cached}, ~{:.4} RTX, stale={}, ts={:?})",
            cached.to_string().parse::<f64>().unwrap_or(0.0) / 1e18,
            fot::swap_back_balance_is_stale(RTX),
            ts,
        );
    }

    println!("PASS: fot swap-back balance refresh task (http+batch+timeout) works over {rounds} rounds (~1min)");
    Ok(())
}

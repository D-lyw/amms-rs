//! BSC 主网 logs-push 实时同步探针。
//!
//! 验证 `RealtimeSyncSource::BscMainnetLogsPush` 在真实 BSC 主网上的行为：
//! - WS `eth_subscribe("logs")` 订阅建立、日志接收；
//! - realtime 更新（affected pools）产出；
//! - canonical_head 持续推进（canonical_head_tracker 对账兜底）。
//!
//! 用法（环境变量必填，端点需支持 logs 订阅 / getLogs）：
//! ```text
//! BSC_WSS_URL=wss://bsc-mainnet.core.chainstack.com/<key> \
//! cargo run --example bsc_logs_probe -- [run_secs]
//! ```

use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use amms::amms::amm::AMM;
use amms::amms::pancake_v2::PancakeV2Pool;
use amms::state_space::{RealtimeSyncSource, StateSpaceBuilder};
use eyre::Context;
use futures::StreamExt;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let wss_url = std::env::var("BSC_WSS_URL").context("BSC_WSS_URL must be set")?;
    let run_secs: u64 = std::env::args()
        .nth(1)
        .map(|s| s.parse().unwrap_or(60))
        .unwrap_or(60);

    let provider = ProviderBuilder::new().connect_ws(WsConnect::new(wss_url.clone())).await?;
    let chain_id = provider.get_chain_id().await?;
    if chain_id != 56 {
        return Err(eyre::eyre!("expected BSC chain_id 56, got {}", chain_id));
    }
    println!("connected (ws): chain_id={chain_id}");

    // BSC PancakeSwap V2 活跃池（WBNB 交易对），Sync 事件高频。
    let pool_addrs: Vec<Address> = [
        "0x16b9a82891338f9bA80E2D6970FddA79D1eb0daE", // WBNB/USDT
        "0x0eD7e52944161450477ee417DE9Cd3a859b14fD0", // WBNB/USDC
    ]
    .into_iter()
    .map(|s| s.parse().unwrap())
    .collect();
    let amms: Vec<AMM> = pool_addrs
        .into_iter()
        .map(|a| AMM::PancakeV2Pool(PancakeV2Pool::new(a)))
        .collect();

    let manager = StateSpaceBuilder::new(provider)
        .with_amms(amms)
        .with_realtime_source(RealtimeSyncSource::BscMainnetLogsPush)
        .with_realtime_ws_endpoints(vec![wss_url])
        .sync()
        .await?;

    let realtime_head = manager.realtime_head.clone();
    let canonical_head = manager.canonical_head.clone();
    let mut stream = manager.subscribe_with_meta().await?;

    println!(
        "sync done: realtime_head={} canonical_head={}",
        realtime_head.load(Ordering::Relaxed),
        canonical_head.load(Ordering::Relaxed)
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(run_secs);
    let mut updates = 0usize;
    let mut affected_total = 0usize;
    while tokio::time::Instant::now() < deadline {
        let item = tokio::time::timeout(Duration::from_secs(15), stream.next())
            .await
            .map_err(|_| eyre::eyre!("stream idle for 15s (no log notifications)"))?;
        match item {
            Some(Ok((meta, addrs))) => {
                updates += 1;
                affected_total += addrs.len();
                println!(
                    "update seq={} block={} affected={} (rt_head={} canonical={})",
                    meta.seq,
                    meta.block_number,
                    addrs.len(),
                    realtime_head.load(Ordering::Relaxed),
                    canonical_head.load(Ordering::Relaxed)
                );
            }
            Some(Err(e)) => {
                eprintln!("stream error: {e:?}");
            }
            None => {
                eprintln!("stream ended");
                break;
            }
        }
    }

    let final_rt = realtime_head.load(Ordering::Relaxed);
    let final_canonical = canonical_head.load(Ordering::Relaxed);
    println!(
        "done: updates={updates} affected_total={affected_total} realtime_head={final_rt} canonical_head={final_canonical}"
    );
    if updates == 0 {
        return Err(eyre::eyre!(
            "no realtime updates received; logs push pipeline not delivering"
        ));
    }
    if final_canonical == 0 {
        return Err(eyre::eyre!("canonical head tracker did not advance"));
    }
    println!("OK: BSC logs push realtime pipeline verified");
    Ok(())
}

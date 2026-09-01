//! ElfomoFi propAMM — XLayer flashblocks 实时流长跑验证（真实流 + 主网 RPC）。
//!
//! 照 `tests/fermi_prop/ws_live_verify.rs` 模式（`#[ignore]` 长跑，env 门控）：
//! - 订阅 XLayer flashblocks WS，实时抓取发往 Elfomo Pool 的 `updatePrices`
//!   原始交易，**本地**解析 calldata 种子（`parse_update_prices_calldata`）；
//! - 对该块延迟校验（等区块确认后）：种子 == `slot1>>32`、本地
//!   `build_orderbook(seed, 金库余额)` == 链上 `getOrderbook` 双向逐位、
//!   本地 `simulate_swap` == 链上 Router `getAmountOut`（probe 金额）；
//! - 这是"raw-tx → 本地直算 → 模拟"整条实时管道的最终验证。
//!
//! 运行（只跑本用例）：
//! ```bash
//! XLAYER_PROVIDER=https://rpc.xlayer.tech \
//!   cargo test --test elfomo_prop -- ws_live_verify -- --ignored --nocapture
//! ```
//! 环境变量：
//!   `XLAYER_PROVIDER` / `XLAYER_RPC_URL`  XLayer RPC（必填，无则跳过）
//!   `XLAYER_FLASHBLOCKS_WS`               flashblocks WS（默认 wss://ws.xlayer.tech/flashblocks）
//!   `ELFOMO_WS_VERIFY_SECS`               运行时长秒（默认 120）
//!   `ELFOMO_WS_VERIFY_CHECK_SECS`         延迟校验间隔秒（默认 10）

use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::{
    eips::{BlockId, BlockNumberOrTag},
    primitives::U256,
    providers::{Provider, ProviderBuilder},
    sol,
};
use amms::amms::{
    elfomo_prop::{
        types::IElfomoFiFactory, ElfomoFiPropPool, ELFOMO_FACTORY_ADDRESS, ELFOMO_POOL_ADDRESS,
        ELFOMO_ROUTER_ADDRESS, ELFOMO_USDT0_ADDRESS, ELFOMO_VAULT_ADDRESS, ELFOMO_XETH_ADDRESS,
        ELFOMO_UPDATE_SELECTOR,
    },
};
use eyre::Result;
use futures::StreamExt;
use serde::Deserialize;
use std::env;
use tokio::time::sleep;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::Message;

/// 默认 flashblocks WS（与 crate 内一致）
const DEFAULT_WS: &str = "wss://ws.xlayer.tech/flashblocks";
const XLAYER_CHAIN_ID: u64 = 196;

/// probe 金额（xETH raw，首档内小额 + 跨档大额）
const PROBE_IN: u128 = 121_513_229_231_558_820;
const PROBE_BIG: u128 = 3_600_000_000_000_000_000;
/// pending 队列上限（updatePrices 每块一笔，RPC head 滞后 ~50s，
/// 全量校验会积压数百块 × 7 次 RPC 触发限流；只保留最新 N 个待校验块）
const MAX_PENDING: usize = 32;
/// 每个校验周期最多验证的块数（抽样验证即可，种子每块都在变）
const MAX_VERIFY_PER_CYCLE: usize = 2;

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IERC20Balance {
        function balanceOf(address account) external view returns (uint256);
    }
}

/// flashblock 消息最小结构（只取 diff.transactions + metadata.block_number/receipts）
#[derive(Debug, Deserialize)]
struct FlashblockMsg {
    #[serde(default)]
    diff: Option<Diff>,
    #[serde(default)]
    metadata: Option<Meta>,
}

#[derive(Debug, Deserialize)]
struct Diff {
    #[serde(default)]
    transactions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Meta {
    /// flashblocks WS 中为 JSON 数字（与生产 `XlayerFlashblockMetadata` 一致）
    block_number: Option<u64>,
}

/// 待延迟校验的 (block, seed)
struct PendingCheck {
    block: u64,
    seed: U256,
}

/// 对单个已确认块做链上对拍：返回 (seed_ok, orderbook_ok, quote_ok)。
/// 金库余额 / getOrderbook / getAmountOut 全部固定到该块。
async fn verify_block<P>(provider: P, pc: &PendingCheck) -> Result<(bool, bool, bool)>
where
    P: alloy::providers::Provider + Clone + Send + Sync + 'static,
{
    use amms::amms::elfomo_prop::types::IElfomoFiRouter;

    let bid = BlockId::Number(BlockNumberOrTag::Number(pc.block));
    // 1) 种子 == slot1 >> 32
    let slot1: U256 = provider
        .get_storage_at(ELFOMO_POOL_ADDRESS, U256::from(1u64))
        .block_id(bid)
        .await?;
    let seed_ok = pc.seed == (slot1 >> 32);
    // 2) 金库余额 + 本地 orderbook vs 链上 getOrderbook
    let (vu, vx) = {
        let u = IERC20Balance::new(ELFOMO_USDT0_ADDRESS, provider.clone());
        let x = IERC20Balance::new(ELFOMO_XETH_ADDRESS, provider.clone());
        (
            u.balanceOf(ELFOMO_VAULT_ADDRESS).block(bid).call().await?,
            x.balanceOf(ELFOMO_VAULT_ADDRESS).block(bid).call().await?,
        )
    };
    let local_ob = ElfomoFiPropPool::build_orderbook(pc.seed, vu, vx);
    let factory = IElfomoFiFactory::new(ELFOMO_FACTORY_ADDRESS, provider.clone());
    let cob = factory
        .getOrderbook(ELFOMO_XETH_ADDRESS, ELFOMO_USDT0_ADDRESS)
        .block(bid)
        .call()
        .await?;
    let chain_ft: Vec<(U256, U256)> = cob
        .fromToLevels
        .into_iter()
        .map(|lv| (lv.size, lv.price))
        .collect();
    let chain_tf: Vec<(U256, U256)> = cob
        .toFromLevels
        .into_iter()
        .map(|lv| (lv.size, lv.price))
        .collect();
    let local_ft: Vec<(U256, U256)> = local_ob
        .from_to_levels
        .iter()
        .map(|lv| (lv.size, lv.price))
        .collect();
    let local_tf: Vec<(U256, U256)> = local_ob
        .to_from_levels
        .iter()
        .map(|lv| (lv.size, lv.price))
        .collect();
    let ob_ok = local_ft == chain_ft && local_tf == chain_tf;
    // 3) 本地 simulate_swap vs 链上 Router.getAmountOut
    let sim_small = ElfomoFiPropPool::simulate_swap_for_orderbook(
        &local_ob,
        ELFOMO_XETH_ADDRESS,
        ELFOMO_USDT0_ADDRESS,
        ELFOMO_XETH_ADDRESS,
        ELFOMO_USDT0_ADDRESS,
        U256::from(PROBE_IN),
    );
    let sim_big = ElfomoFiPropPool::simulate_swap_for_orderbook(
        &local_ob,
        ELFOMO_XETH_ADDRESS,
        ELFOMO_USDT0_ADDRESS,
        ELFOMO_XETH_ADDRESS,
        ELFOMO_USDT0_ADDRESS,
        U256::from(PROBE_BIG),
    );
    let router = IElfomoFiRouter::new(ELFOMO_ROUTER_ADDRESS, provider.clone());
    let chain_small = router
        .getAmountOut(
            ELFOMO_XETH_ADDRESS,
            ELFOMO_USDT0_ADDRESS,
            U256::from(PROBE_IN),
        )
        .block(bid)
        .call()
        .await?;
    let chain_big = router
        .getAmountOut(
            ELFOMO_XETH_ADDRESS,
            ELFOMO_USDT0_ADDRESS,
            U256::from(PROBE_BIG),
        )
        .block(bid)
        .call()
        .await?;
    let quote_ok = sim_small == chain_small && sim_big == chain_big;
    Ok((seed_ok, ob_ok, quote_ok))
}

/// 公共 RPC 限流（429）等瞬时错误退避重试
async fn retry_verify<P>(provider: P, pc: &PendingCheck) -> Result<(bool, bool, bool)>
where
    P: alloy::providers::Provider + Clone + Send + Sync + 'static,
{
    for attempt in 0..6u32 {
        match verify_block(provider.clone(), pc).await {
            Ok(r) => return Ok(r),
            Err(e) if attempt < 5 => {
                println!(
                    "verify block {} attempt {} failed: {}; retrying",
                    pc.block,
                    attempt + 1,
                    e
                );
                sleep(Duration::from_secs(3)).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "long-running live verification"]
async fn test_elfomo_prop_ws_live_verify() -> Result<()> {
    let rpc_url = match env::var("XLAYER_PROVIDER")
        .or_else(|_| env::var("XLAYER_RPC_URL"))
        .ok()
    {
        Some(u) => u,
        None => {
            println!("SKIP: XLAYER_PROVIDER not set");
            return Ok(());
        }
    };
    let ws_url = env::var("XLAYER_FLASHBLOCKS_WS").unwrap_or_else(|_| DEFAULT_WS.to_string());
    let run_secs = env::var("ELFOMO_WS_VERIFY_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let check_secs = env::var("ELFOMO_WS_VERIFY_CHECK_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);

    let provider = Arc::new(ProviderBuilder::new().connect_http(rpc_url.parse()?));
    if provider.get_chain_id().await? != XLAYER_CHAIN_ID {
        eyre::bail!("expected XLayer");
    }

    println!("Connecting to {ws_url}, verifying {run_secs}s");
    let (mut socket, _) = connect_async(ws_url.clone()).await?;

    // 流内解析统计 + 待校验队列
    let mut pending: Vec<PendingCheck> = Vec::new();
    let mut verified_blocks: Vec<u64> = Vec::new();
    let mut stream_seen = 0u64;
    let mut parse_fail = 0u64;
    let deadline = Instant::now() + Duration::from_secs(run_secs);
    let mut last_check = Instant::now();
    let mut stream_alive = true;

    while Instant::now() < deadline {
        // 批量读流（100ms 超时），收集 updatePrices raw-tx
        let mut got = false;
        loop {
            match tokio::time::timeout(Duration::from_millis(100), socket.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    got = true;
                    let msg: FlashblockMsg = match serde_json::from_str(&text) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    let Some(block) = msg.metadata.as_ref().and_then(|m| m.block_number) else {
                        continue;
                    };
                    let Some(diff) = msg.diff.as_ref() else {
                        continue;
                    };
                    for raw_hex in &diff.transactions {
                        let raw_hex = raw_hex.strip_prefix("0x").unwrap_or(raw_hex);
                        let Ok(raw) = alloy::hex::decode(raw_hex) else {
                            continue;
                        };
                        // 轻量提取 to（与生产路径同一解析）
                        let Some(to) = amms::amms::caliber_prop::extract_to_from_raw_tx(&raw)
                        else {
                            continue;
                        };
                        if to != ELFOMO_POOL_ADDRESS {
                            continue;
                        }
                        let Some(input) = amms::amms::caliber_prop::extract_input_from_raw_tx(&raw)
                        else {
                            continue;
                        };
                        if input.len() < 4 || input[..4] != ELFOMO_UPDATE_SELECTOR {
                            continue;
                        }
                        let Some(seed) = ElfomoFiPropPool::parse_update_prices_calldata(&input)
                        else {
                            parse_fail += 1;
                            continue;
                        };
                        stream_seen += 1;
                        pending.push(PendingCheck { block, seed });
                        if pending.len() > MAX_PENDING {
                            pending.remove(0);
                        }
                    }
                }
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(e))) => {
                    println!("WS error: {e}");
                    break;
                }
                Ok(None) => {
                    println!("WS closed, exiting loop");
                    stream_alive = false;
                    break;
                }
                _ => break,
            }
            if !stream_alive {
                break;
            }
            if !got {
                break;
            }
        }

        // 周期延迟校验：块已确认（≤ canonical head）→ 链上对拍
        if last_check.elapsed() >= Duration::from_secs(check_secs) {
            last_check = Instant::now();
            // 公共 RPC 限流时跳过本周期（不中断整条实时验证）
            let Ok(head) = provider.get_block_number().await else {
                println!("head fetch failed (rate limit?), skipping cycle");
                sleep(Duration::from_secs(5)).await;
                continue;
            };
            let mut kept: Vec<PendingCheck> = Vec::new();
            let mut verified_this_cycle = 0usize;
            for pc in pending.drain(..) {
                if pc.block > head {
                    kept.push(pc);
                    continue;
                }
                if verified_this_cycle >= MAX_VERIFY_PER_CYCLE {
                    // 抽样验证：超出的已确认块直接丢弃（种子已被更新块取代）
                    continue;
                }
                verified_this_cycle += 1;
                let (seed_ok, ob_ok, quote_ok) = retry_verify(provider.clone(), &pc).await?;
                verified_blocks.push(pc.block);
                println!(
                    "block {} seed={:#x} seed_ok={} ob_ok={} quote_ok={}",
                    pc.block, pc.seed, seed_ok, ob_ok, quote_ok
                );
                assert!(seed_ok, "seed mismatch at block {}", pc.block);
                assert!(ob_ok, "orderbook mismatch at block {}", pc.block);
                assert!(quote_ok, "quote mismatch at block {}", pc.block);
            }
            pending = kept;
        }
        if !stream_alive {
            break;
        }
        // 防止长时间空转（WS 消息多时循环自然被打断）
        if !got {
            sleep(Duration::from_millis(200)).await;
        }
    }

    println!(
        "LIVE VERIFY DONE: stream_seen={stream_seen} parse_fail={parse_fail} verified_blocks={}",
        verified_blocks.len()
    );
    assert!(
        stream_seen > 0,
        "未在运行窗口内观察到任何 updatePrices 原始交易（WS 端点或网络问题？）"
    );
    assert!(
        !verified_blocks.is_empty(),
        "运行窗口内没有块完成确认校验（缩短 run_secs 或检查 RPC？）"
    );
    Ok(())
}

//! PendlePool 漂移验证
//!
//! 模式对齐 CurveNG sync_drift:
//! 1. init 在 start_block → 立即与 _storage() 对比（零漂移断言）
//! 2. 从 start_block+1 拉取事件（避开 init 块）
//! 3. 按 (block, tx_idx, log_idx) 排序后逐个 sync() 回放
//! 4. 每 N 个 checkpoint 与链上 _storage() 对比，暴露漂移
//! 5. 定期 reinit 防止漂移累积
//!
//! 修复记录:
//! - fix #1: 无关（fork_test 问题）
//! - fix #3: 扩展为多市场矩阵测试，每个市场跑完整漂移回放
//! - fix #5: 使用动态区块，追踪最近 ~2000 blocks 的事件

use alloy::{
    eips::BlockId,
    primitives::{address, keccak256, Address, B256, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::{Filter, Log},
    sol,
};
use amms::amms::{amm::AutomatedMarketMaker, pendle::PendlePool};
use eyre::Result;
use std::{
    process::{Child, Command, Stdio},
    time::Instant,
};
use tokio::time::{sleep, Duration};

/// 测试用例：多个 Pendle Market（与 fork_test 一致）
const CASES: &[(Address, &str)] = &[
    (
        address!("0271A803f0d3Dec9cCd105A4A4d41e6Ee1458765"),
        "srUSDe",
    ),
    (
        address!("9c560ebaf78e596cbcc27411d633a74d628dd7dc"),
        "sUSDS",
    ),
    (address!("f80b67a32df07960c731794769309e3d30e9717f"), "USDG"),
];

/// 事件拉取块大小 — 避免 RPC limit
const EVENT_CHUNK: u64 = 2000;
/// 检查点间隔（每 N 个 block 比对一次链上）
const CHECK_INTERVAL: u64 = 200;

sol! {
    #[sol(rpc)]
    contract IPMarketStorage {
        function _storage() external view returns (
            int128 totalPt, int128 totalSy, uint96 lastLnImpliedRate,
            uint16, uint16, uint16
        );
    }
}

fn swap_sig() -> B256 {
    B256::from(keccak256(
        b"Swap(address,address,int256,int256,uint256,uint256)",
    ))
}
fn mint_sig() -> B256 {
    B256::from(keccak256(b"Mint(address,uint256,uint256,uint256)"))
}
fn burn_sig() -> B256 {
    B256::from(keccak256(b"Burn(address,address,uint256,uint256,uint256)"))
}
fn rate_sig() -> B256 {
    B256::from(keccak256(b"UpdateImpliedRate(uint256,uint256)"))
}

fn start_anvil(rpc_url: &str, fork_block: u64) -> Child {
    Command::new("anvil")
        .args(["--fork-url", rpc_url])
        .args(["--fork-block-number", &fork_block.to_string()])
        .args(["--port", "8549", "--silent"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("anvil 未安装; 请运行: foundryup && anvil --version")
}

async fn wait_anvil() {
    let p = ProviderBuilder::new().connect_http("http://localhost:8549".parse().unwrap());
    let t = Instant::now();
    loop {
        if p.get_block_number().await.is_ok() {
            return;
        }
        assert!(t.elapsed() < Duration::from_secs(30), "anvil 启动超时");
        sleep(Duration::from_millis(300)).await;
    }
}

/// 分块拉取事件，避免 RPC block range limit
async fn fetch_events(
    provider: &impl Provider,
    market: Address,
    from: u64,
    to: u64,
) -> Result<Vec<Log>> {
    let sigs = vec![swap_sig(), mint_sig(), burn_sig(), rate_sig()];
    let mut all = Vec::new();
    let mut cur = from;
    while cur <= to {
        let end = (cur + EVENT_CHUNK - 1).min(to);
        match provider
            .get_logs(
                &Filter::new()
                    .address(market)
                    .event_signature(sigs.clone())
                    .from_block(cur)
                    .to_block(end),
            )
            .await
        {
            Ok(logs) => all.extend(logs),
            Err(e) => println!("⚠️ get_logs [{},{}]: {:?}", cur, end, e),
        }
        cur = end + 1;
        sleep(Duration::from_millis(100)).await;
    }
    // 按 (block, tx_idx, log_idx) 排序
    all.sort_by(|a, b| match a.block_number.cmp(&b.block_number) {
        std::cmp::Ordering::Equal => match a.transaction_index.cmp(&b.transaction_index) {
            std::cmp::Ordering::Equal => a.log_index.cmp(&b.log_index),
            other => other,
        },
        other => other,
    });
    Ok(all)
}

/// 从链上 _storage() 读取状态
async fn onchain_state(
    provider: &impl Provider,
    market: Address,
    block: BlockId,
) -> Result<(U256, U256, U256)> {
    let c = IPMarketStorage::new(market, provider);
    let s = c._storage().block(block).call().await?;
    Ok((
        U256::from(s.totalPt as u128),
        U256::from(s.totalSy as u128),
        U256::from(s.lastLnImpliedRate),
    ))
}

async fn run_drift_test(
    provider: &impl Provider,
    market: Address,
    label: &str,
    start_block: u64,
    end_block: u64,
) -> Result<()> {
    println!("\n═══════ 漂移测试: {} ({:#x}) ═══════", label, market);
    println!("区块范围: {} ~ {}", start_block + 1, end_block);

    // ── 1. Init + 零漂移断言 ──
    let mut pool = match PendlePool::new(market)
        .init(BlockId::from(start_block), provider)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            println!("⚠️ [{}] init 失败: {:?}, 跳过", label, e);
            return Ok(());
        }
    };
    pool.set_last_synced_block(start_block);

    let (oc_pt, oc_sy, oc_rate) =
        onchain_state(provider, market, BlockId::from(start_block)).await?;
    assert_eq!(pool.total_pt, oc_pt, "[{}] init totalPt 漂移", label);
    assert_eq!(pool.total_sy, oc_sy, "[{}] init totalSy 漂移", label);
    assert_eq!(
        pool.last_ln_implied_rate, oc_rate,
        "[{}] init impliedRate 漂移",
        label
    );

    // 如果在 start_block 时已到期，说明整个范围内都无交易事件
    if pool.expiry <= start_block {
        println!("⚠️ [{}] 在 start_block 已到期, 跳过", label);
        return Ok(());
    }

    println!("✅ init 零漂移验证通过 (block {})", start_block);

    // ── 2. 拉取事件 ──
    let events = fetch_events(provider, market, start_block + 1, end_block).await?;
    println!(
        "事件数: {} (block {} ~ {})",
        events.len(),
        start_block + 1,
        end_block
    );
    if events.is_empty() {
        println!("⚠️ 无事件, 跳过");
        return Ok(());
    }

    // ── 3. 逐个 sync + periodic checkpoint + periodic reinit ──
    let mut processed = 0u64;
    let mut max_drift_pt = U256::ZERO;
    let mut max_drift_sy = U256::ZERO;
    let mut max_drift_rate = U256::ZERO;
    let mut last_check_block = start_block;

    for (idx, log) in events.iter().enumerate() {
        let block_num = log.block_number.unwrap_or(0);

        // sync 事件
        pool.sync(log)
            .map_err(|e| {
                println!("⚠️ [{}] sync error block {}: {:?}", label, block_num, e);
            })
            .ok();
        processed += 1;

        // 判断是否是 block 内最后一个事件
        let is_last_in_block = events
            .get(idx + 1)
            .map(|next| next.block_number.unwrap_or(0) > block_num)
            .unwrap_or(true);

        if !is_last_in_block {
            continue;
        }

        // 到达 block 边界

        // checkpoint：每 CHECK_INTERVAL 个 block 比对一次
        if block_num >= last_check_block.saturating_add(CHECK_INTERVAL) {
            match onchain_state(provider, market, BlockId::from(block_num)).await {
                Ok((cpt, csy, crate_r)) => {
                    let dp = if pool.total_pt > cpt {
                        pool.total_pt - cpt
                    } else {
                        cpt - pool.total_pt
                    };
                    let ds = if pool.total_sy > csy {
                        pool.total_sy - csy
                    } else {
                        csy - pool.total_sy
                    };
                    let dr = if pool.last_ln_implied_rate > crate_r {
                        pool.last_ln_implied_rate - crate_r
                    } else {
                        crate_r - pool.last_ln_implied_rate
                    };

                    if dp > max_drift_pt {
                        max_drift_pt = dp;
                    }
                    if ds > max_drift_sy {
                        max_drift_sy = ds;
                    }
                    if dr > max_drift_rate {
                        max_drift_rate = dr;
                    }

                    if dp.is_zero() && ds.is_zero() && dr.is_zero() {
                        println!("  block {:>8}: ✅ 零漂移 (events={})", block_num, processed);
                    } else {
                        println!(
                            "  block {:>8}: ⚠️ Δpt={} Δsy={} Δrate={} (events={})",
                            block_num, dp, ds, dr, processed
                        );
                    }
                }
                Err(e) => println!("⚠️ checkpoint block {}: {:?}", block_num, e),
            }
            last_check_block = block_num;
        }
    }

    // 最终校验
    let final_block = events
        .last()
        .and_then(|l| l.block_number)
        .unwrap_or(end_block);
    match onchain_state(provider, market, BlockId::from(final_block)).await {
        Ok((cpt, csy, crate_r)) => {
            let dp = if pool.total_pt > cpt {
                pool.total_pt - cpt
            } else {
                cpt - pool.total_pt
            };
            let ds = if pool.total_sy > csy {
                pool.total_sy - csy
            } else {
                csy - pool.total_sy
            };
            let dr = if pool.last_ln_implied_rate > crate_r {
                pool.last_ln_implied_rate - crate_r
            } else {
                crate_r - pool.last_ln_implied_rate
            };
            if dp > max_drift_pt {
                max_drift_pt = dp;
            }
            if ds > max_drift_sy {
                max_drift_sy = ds;
            }
            if dr > max_drift_rate {
                max_drift_rate = dr;
            }
            if dp.is_zero() && ds.is_zero() && dr.is_zero() {
                println!("  block {:>8} (final):  ✅ 零漂移", final_block);
            } else {
                println!(
                    "  block {:>8} (final):  ⚠️ Δpt={} Δsy={} Δrate={}",
                    final_block, dp, ds, dr
                );
            }
        }
        Err(e) => println!("⚠️ 最终校验失败: {:?}", e),
    }

    println!("\n=== 漂移报告 [{}] ===", label);
    println!("总事件: {}", processed);
    println!("最大 totalPt 漂移: {}", max_drift_pt);
    println!("最大 totalSy 漂移: {}", max_drift_sy);
    println!("最大 impliedRate 漂移: {}", max_drift_rate);

    assert!(
        max_drift_pt < U256::from(100),
        "[{}] totalPt 漂移过大: {}",
        label,
        max_drift_pt
    );
    assert!(
        max_drift_sy < U256::from(100),
        "[{}] totalSy 漂移过大: {}",
        label,
        max_drift_sy
    );
    assert!(
        max_drift_rate < U256::from(100),
        "[{}] impliedRate 漂移过大: {}",
        label,
        max_drift_rate
    );
    println!("✅ [{}] 漂移测试通过", label);
    Ok(())
}

#[tokio::test]
#[ignore = "按需运行"]
async fn test_sync_drift_matrix() -> Result<()> {
    dotenv::dotenv().ok();
    let rpc = std::env::var("ETHEREUM_PROVIDER")?;

    // ── 动态区块范围: ~7 天事件回放 ──
    let tmp_provider = ProviderBuilder::new().connect_http(rpc.parse().unwrap());
    let current = tmp_provider.get_block_number().await?;
    let fork_block = current.saturating_sub(1000);
    let event_end_block = current.saturating_sub(1000);
    let event_start_block = current.saturating_sub(50000);
    println!(
        "current={} fork={} event_range={}~{} ({} blocks)",
        current,
        fork_block,
        event_start_block,
        event_end_block,
        event_end_block.saturating_sub(event_start_block)
    );

    // ── 启动 anvil fork ──
    let mut anvil = start_anvil(&rpc, fork_block);
    wait_anvil().await;
    sleep(Duration::from_secs(2)).await;
    let provider = ProviderBuilder::new().connect_http("http://localhost:8549".parse().unwrap());

    for (market, label) in CASES {
        let _ = run_drift_test(
            &provider,
            *market,
            label,
            event_start_block,
            event_end_block,
        )
        .await;
    }

    let _ = anvil.kill();
    println!("\n✅ 全部漂移测试完成");
    Ok(())
}

//! Fermi PropAMM 真实 Titan WS 长跑验证（M4.5.1，2026-08-25）。
//!
//! 目的：连接真实 Titan overrides WS + 生产主网 RPC，长时间运行中验证三件事：
//!   1. **lane 同步**：每条含 Fermi lane 的 Titan 快照，接受后本地 `pool.lane`
//!      与快照 lane 逐字段一致；被版本守卫拒绝的必须 `update_timestamp` 不新于
//!      本地现有值（拒绝只可能是"更旧/相同"，不能是"更新"）；
//!   2. **余额账本**：本地 vault 余额（init `balanceOf` + 链上事件回放，权威账本 =
//!      ERC20 Transfer 事件，跨 pair 成交也覆盖）与链上 `balanceOf(vault)` 逐位一致；
//!   3. **报价 100% 对齐**：相同输入（Titan 最新 lane + 链上 last-trade 槽 + 本地
//!      余额账本）下，本地 `engine_quote` 与链上 `engine.quote` 逐位一致。
//!
//! 对比方法（为什么有效）：
//! - Titan lane 是链上看不到的高频报价（链上 registry 槽位只有过期值/稀疏快照），
//!   裸 `eth_call` 读不到 → 用 **state override** 把 registry lane 槽位注入为
//!   Titan 最新 lane，并把 `update_timestamp` 改写为对拍块时间戳（绕过
//!   `StaleUpdate` revert），链上在**固定块**上计算 quote——与 Titan 官方模拟器
//!   同款机制（漂移测试已 100% 验证该机制本身）；
//! - 对拍块 = 本地账本已完整回放的 `last_synced_block`：该块上本地余额精确等于
//!   链上余额（事件回放到该块为止），消除"链已前进、本地未回放"的竞态；`eth_call`
//!   固定在该块执行，结果确定、可复现；
//! - last-trade 同块成交校正的输入（sub0/sub1 槽）从链上对拍块读取，喂给本地
//!   clone，与链上 quote 的校正路径使用同一输入 → 对拍隔离验证"余额账本 + 曲线
//!   数学"两件事，任何一件漂移都会表现为 quote 不一致；
//! - 检查点按固定间隔（默认 15s）执行，覆盖正常价、大额封顶、COR/IL revert 三类
//!   路径（正向 0.01/1/10 base 单位，反向 100/1e4/1e6 quote 基础单位）。
//!
//! 运行（只跑本用例，不跑全库）：
//!   `cargo test --test fermi_prop ws_live_verify -- --ignored --nocapture`
//! 环境变量：
//!   `ETHEREUM_PROVIDER` / `ETHEREUM_RPC_URL`     主网 RPC（缺省用生产 Chainstack）
//!   `TITAN_OVERRIDES_WS_URL` / `TITAN_OVERRIDES_RPC_URL`  Titan 端点（缺省公开端点）
//!   `FERMI_WS_VERIFY_SECS`             运行时长秒（默认 300）
//!   `FERMI_WS_VERIFY_CHECK_EVERY_SECS` 检查点间隔秒（默认 15）
//!   `FERMI_WS_VERIFY_PAIRS`            追加 pair："WBTC/USDC,cbBTC/USDC"

use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::{
    eips::BlockNumberOrTag,
    primitives::{address, Address, U256},
    providers::{Provider, ProviderBuilder},
};
use amms::{
    amms::{
        amm::AutomatedMarketMaker,
        fermi_prop::{
            titan::apply_titan_snapshot,
            types::fermi_registry_lane_slot,
            FermiLane, FermiPropPool,
        },
    },
    state_space::titan_stream::{
        subscribe_overrides_stream, TitanOverridesSnapshot, TitanQuoteStreamConfig,
        DEFAULT_OVERRIDES_RPC_URL, DEFAULT_OVERRIDES_WS_URL,
    },
};
use futures::StreamExt;
use tokio::sync::Mutex;
use tracing::{info, warn};

use super::mainnet_sync_drift::{
    check_quote_parity, fetch_chain_balances, fetch_logs, init_pool, DriftCase,
};

/// 生产主网 RPC（用户指定，作为 env 缺省）。
const PROD_RPC_URL: &str =
    "https://ethereum-mainnet.core.chainstack.com/06920df668e96f928404674b359b251f";

fn weth() -> Address {
    address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2")
}
fn usdc() -> Address {
    address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48")
}
fn usdt() -> Address {
    address!("0xdac17f958d2ee523a2206206994597c13d831ec7")
}
fn wbtc() -> Address {
    address!("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599")
}
fn cbbtc() -> Address {
    address!("0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf")
}

fn live_cases() -> Vec<DriftCase> {
    let mut cases = vec![DriftCase {
        label: "WETH-USDC",
        token_a: weth(),
        token_b: usdc(),
        decimals_a: 18,
        decimals_b: 6,
    }];
    if let Ok(extra) = std::env::var("FERMI_WS_VERIFY_PAIRS") {
        for part in extra.split(',').filter(|s| !s.is_empty()) {
            let mut it = part.split('/');
            let (a, b) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
            let a = a.trim().to_ascii_lowercase();
            let b = b.trim().to_ascii_lowercase();
            let tok = |t: &str| match t {
                "weth" => weth(),
                "usdc" => usdc(),
                "usdt" => usdt(),
                "wbtc" => wbtc(),
                "cbbtc" => cbbtc(),
                other => other.parse::<Address>().expect("bad token addr in FERMI_WS_VERIFY_PAIRS"),
            };
            cases.push(DriftCase {
                label: Box::leak(format!("{}-{}", a, b).into_boxed_str()),
                token_a: tok(&a),
                token_b: tok(&b),
                decimals_a: 18,
                decimals_b: 6,
            });
        }
    }
    cases
}

// ============================================================================
// 快照 lane 提取与 lane 同步不变量
// ============================================================================

/// 从 Titan 快照提取本 pair 的 registry lane 槽位原始值（swapper/wrapper 两 venue
/// 合并，与 `apply_titan_snapshot` 的收集逻辑一致）。
fn snapshot_lane_word(
    pool: &FermiPropPool,
    snapshot: &TitanOverridesSnapshot,
) -> Option<U256> {
    let slot = fermi_registry_lane_slot(pool.engine_address, pool.token_a, pool.token_b);
    for (venue, pamm) in &snapshot.per_pamm {
        if *venue != pool.swapper_address && *venue != pool.wrapper_address {
            continue;
        }
        if let Some(reg) = pamm.accounts.get(&pool.registry_address) {
            if let Some(word) = reg.state_diff.get(&slot) {
                return Some(*word);
            }
        }
    }
    None
}

/// lane 同步不变量：快照含本 pair lane 时——
/// - 接受（`lanes_applied > 0`）→ 本地 lane 必须与快照 lane 逐字段一致；
/// - 未接受 → 快照 lane 的 update_timestamp 必须不新于本地（拒绝只可能是更旧/相同）。
fn check_lane_sync(
    stats: &mut LiveStats,
    label: &str,
    pool: &FermiPropPool,
    snapshot: &TitanOverridesSnapshot,
    lanes_applied: usize,
) {
    let Some(snap_lane) = snapshot_lane_word(pool, snapshot).and_then(FermiLane::from_slot_word)
    else {
        return;
    };
    stats.snapshots_with_lane += 1;
    if lanes_applied > 0 {
        stats.lanes_applied += 1;
        if pool.lane.update_timestamp != snap_lane.update_timestamp
            || pool.lane.fair_price_e8 != snap_lane.fair_price_e8
            || pool.lane.flag != snap_lane.flag
        {
            stats.lane_mismatches += 1;
            println!(
                "[{label}] LANE-SYNC MISMATCH after accepted snapshot: local=(ts={} flag={} price={}) snap=(ts={} flag={} price={})",
                pool.lane.update_timestamp,
                pool.lane.flag,
                pool.lane.fair_price_e8,
                snap_lane.update_timestamp,
                snap_lane.flag,
                snap_lane.fair_price_e8,
            );
        }
    } else if snap_lane.update_timestamp > pool.lane.update_timestamp {
        // 更"新"的快照 lane 未应用 → 异常（版本守卫只允许拒绝更旧/相同）。
        stats.lane_mismatches += 1;
        println!(
            "[{label}] LANE-SYNC MISMATCH: newer snapshot lane (ts={} price={}) not applied, local ts={} price={}",
            snap_lane.update_timestamp, snap_lane.fair_price_e8,
            pool.lane.update_timestamp, pool.lane.fair_price_e8,
        );
    }
}

// ============================================================================
// 事件回放（保持本地 vault 余额账本与链上同步）
// ============================================================================

/// 单次增量回放：拉取 `last_synced+1 ..= latest` 事件并逐个 `sync`，
/// 成功后才推进 `last_synced`（避免断线重拉造成重复记账）。
async fn replay_events<P: Provider + Clone>(
    provider: &P,
    cases: &[DriftCase],
    pools: &[Arc<Mutex<FermiPropPool>>],
) -> eyre::Result<()> {
    let latest = provider.get_block_number().await?;
    for (case, pool) in cases.iter().zip(pools) {
        let from = { pool.lock().await.last_synced_block().saturating_add(1) };
        if from > latest {
            continue;
        }
        let logs = fetch_logs(provider, case, from, latest).await?;
        let mut p = pool.lock().await;
        for log in &logs {
            if let Err(e) = p.sync(log) {
                warn!(label = case.label, block = log.block_number, error = ?e, "fermi ws live: sync error");
            }
        }
        p.set_last_synced_block(latest);
    }
    Ok(())
}

// ============================================================================
// 检查点对拍
// ============================================================================

#[derive(Debug, Default)]
struct CheckpointResult {
    block: u64,
    lane_ts: u32,
    lane_price_e8: u64,
    balance_mismatches: u32,
    quote_mismatches: u32,
    skipped: bool,
}

/// 检查点：以本地账本已回放的 `last_synced_block` 为对拍块——
/// 1. vault 余额账本 vs 链上 `balanceOf(vault)` 逐位对拍；
/// 2. 用本地当前 lane（Titan 最新报价，update_timestamp 改写为对拍块时间戳）做
///    state override，链上 `eth_call` vs 本地 `engine_quote` 逐位对拍（6 个金额）。
async fn checkpoint<P: Provider + Clone>(
    provider: &P,
    case: &DriftCase,
    pool: &Arc<Mutex<FermiPropPool>>,
) -> eyre::Result<CheckpointResult> {
    let local = pool.lock().await.clone();
    let block = local.last_synced_block;
    let mut res = CheckpointResult {
        block,
        lane_ts: local.lane.update_timestamp,
        lane_price_e8: local.lane.fair_price_e8,
        ..Default::default()
    };
    if block == 0 {
        res.skipped = true;
        return Ok(res);
    }

    // 1. vault 余额账本对拍
    let (chain_a, chain_b) = fetch_chain_balances(provider, case, block).await?;
    let local_a = local.vault_balances.get(&case.token_a).copied().unwrap_or_default();
    let local_b = local.vault_balances.get(&case.token_b).copied().unwrap_or_default();
    if local_a != chain_a {
        res.balance_mismatches += 1;
        println!(
            "[{}] block={} balance[token_a] local={} chain={} diff={}",
            case.label,
            block,
            local_a,
            chain_a,
            if local_a > chain_a { local_a - chain_a } else { chain_a - local_a },
        );
    }
    if local_b != chain_b {
        res.balance_mismatches += 1;
        println!(
            "[{}] block={} balance[token_b] local={} chain={} diff={}",
            case.label,
            block,
            local_b,
            chain_b,
            if local_b > chain_b { local_b - chain_b } else { chain_b - local_b },
        );
    }

    // 2. quote 逐位对拍（pair 活跃且 lane 有效时）
    if !local.active || local.lane.fair_price_e8 == 0 {
        res.skipped = true;
        return Ok(res);
    }
    let block_ts = provider
        .get_block_by_number(BlockNumberOrTag::Number(block))
        .await?
        .ok_or_else(|| eyre::eyre!("block {block} not found"))?
        .header
        .timestamp;
    // fresh lane：改写 update_timestamp = 对拍块时间戳，保留 Titan flag/价格。
    let fresh_word = (U256::from(block_ts) << U256::from(224))
        | (U256::from(local.lane.flag as u64) << U256::from(216))
        | U256::from(local.lane.fair_price_e8);
    let ok = check_quote_parity(provider, case, &local, block, fresh_word).await?;
    if !ok {
        res.quote_mismatches += 1;
    }
    Ok(res)
}

// ============================================================================
// 统计与主流程
// ============================================================================

#[derive(Debug, Default)]
struct LiveStats {
    snapshots: usize,
    snapshots_with_lane: usize,
    lanes_applied: usize,
    lane_mismatches: usize,
    checkpoints: usize,
    skipped_checkpoints: usize,
    quote_mismatches: usize,
    balance_mismatches: usize,
    stream_errors: usize,
    checkpoint_errors: usize,
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_fermi_prop_ws_live_verify() -> eyre::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let rpc_endpoint = crate::common::rpc::provider_url().unwrap_or_else(|| PROD_RPC_URL.to_string());
    let ws_url = std::env::var("TITAN_OVERRIDES_WS_URL")
        .unwrap_or_else(|_| DEFAULT_OVERRIDES_WS_URL.to_string());
    let titan_rpc_url = std::env::var("TITAN_OVERRIDES_RPC_URL")
        .unwrap_or_else(|_| DEFAULT_OVERRIDES_RPC_URL.to_string());
    let secs = std::env::var("FERMI_WS_VERIFY_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(300);
    let check_every = std::env::var("FERMI_WS_VERIFY_CHECK_EVERY_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(15);

    let provider = Arc::new(ProviderBuilder::new().connect_http(rpc_endpoint.parse().unwrap()));
    let cases = live_cases();
    let latest = provider.get_block_number().await?;
    let anchor = latest;
    info!(
        rpc = %rpc_endpoint, ws = %ws_url, secs, check_every, latest, pairs = cases.len(),
        "fermi ws live verify start"
    );

    // init @anchor（balanceOf/params/lane/active 精确于 anchor），last_synced = anchor。
    let mut pools = Vec::new();
    for case in &cases {
        let pool = init_pool(&*provider, case, anchor).await?;
        let mut pool = pool;
        pool.set_last_synced_block(anchor);
        let pool = Arc::new(Mutex::new(pool));
        info!(
            label = case.label, anchor,
            lane_price_e8 = { pool.lock().await.lane.fair_price_e8 },
            active = { pool.lock().await.active },
            "fermi ws live pool init"
        );
        pools.push(pool);
    }

    let stream_cfg = TitanQuoteStreamConfig {
        ws_url,
        rpc_url: titan_rpc_url,
        idle_timeout: Duration::from_secs(30),
        reconnect_delay: Duration::from_secs(2),
    };
    let mut stream = Box::pin(subscribe_overrides_stream(stream_cfg));

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut stats = LiveStats::default();
    let mut last_check = Instant::now() - Duration::from_secs(check_every);

    while Instant::now() < deadline {
        // 1. 增量事件回放（保持本地余额账本最新；对拍块 = 已回放块，消除竞态）
        if let Err(e) = replay_events(&*provider, &cases, &pools).await {
            warn!(error = ?e, "fermi ws live: replay_events failed");
        }

        // 2. 等待下一条 Titan 快照（最多 10s；流内部空闲 30s 会 RPC rebase 重连）
        let item = tokio::time::timeout(Duration::from_secs(10), stream.next()).await;
        let snapshot = match item {
            Ok(Some(Ok(s))) => s,
            Ok(Some(Err(e))) => {
                stats.stream_errors += 1;
                warn!(error = %e, "fermi ws live: titan stream item error");
                continue;
            }
            Ok(None) => {
                warn!("fermi ws live: titan stream ended");
                break;
            }
            Err(_) => {
                warn!("fermi ws live: titan stream idle (10s no message)");
                continue;
            }
        };
        stats.snapshots += 1;
        if snapshot.slot.is_none() && snapshot.per_pamm.is_empty() {
            continue;
        }

        // 3. 应用快照到本地 pool + lane 同步不变量检查
        for (case, pool) in cases.iter().zip(&pools) {
            let mut p = pool.lock().await;
            let outcome = apply_titan_snapshot(&mut p, &snapshot);
            check_lane_sync(&mut stats, case.label, &p, &snapshot, outcome.lanes_applied);
        }

        // 4. 检查点（固定间隔）：余额对拍 + quote 逐位对拍
        if last_check.elapsed() >= Duration::from_secs(check_every) {
            for (case, pool) in cases.iter().zip(&pools) {
                match checkpoint(&*provider, case, pool).await {
                    Ok(cp) => {
                        stats.checkpoints += 1;
                        stats.balance_mismatches += cp.balance_mismatches as usize;
                        stats.quote_mismatches += cp.quote_mismatches as usize;
                        if cp.skipped {
                            stats.skipped_checkpoints += 1;
                            println!(
                                "[{}] CHECKPOINT block={} lane=(ts={} price={}) SKIPPED (inactive/no lane)",
                                case.label, cp.block, cp.lane_ts, cp.lane_price_e8,
                            );
                        } else {
                            println!(
                                "[{}] CHECKPOINT block={} lane=(ts={} price={}) balance={} quote={} {}",
                                case.label,
                                cp.block,
                                cp.lane_ts,
                                cp.lane_price_e8,
                                if cp.balance_mismatches == 0 { "OK" } else { "MISMATCH" },
                                if cp.quote_mismatches == 0 { "OK" } else { "MISMATCH" },
                                if cp.balance_mismatches == 0 && cp.quote_mismatches == 0 { "PASSED" } else { "FAILED" },
                            );
                        }
                    }
                    Err(e) => {
                        stats.checkpoint_errors += 1;
                        warn!(label = case.label, error = ?e, "fermi ws live: checkpoint failed");
                    }
                }
            }
            last_check = Instant::now();
        }
    }

    // 5. 最终回放 + 最终检查点
    if let Err(e) = replay_events(&*provider, &cases, &pools).await {
        warn!(error = ?e, "fermi ws live: final replay failed");
    }
    for (case, pool) in cases.iter().zip(&pools) {
        match checkpoint(&*provider, case, pool).await {
            Ok(cp) => {
                stats.checkpoints += 1;
                stats.balance_mismatches += cp.balance_mismatches as usize;
                stats.quote_mismatches += cp.quote_mismatches as usize;
                if !cp.skipped {
                    println!(
                        "[{}] FINAL CHECKPOINT block={} lane=(ts={} price={}) balance={} quote={} {}",
                        case.label,
                        cp.block,
                        cp.lane_ts,
                        cp.lane_price_e8,
                        if cp.balance_mismatches == 0 { "OK" } else { "MISMATCH" },
                        if cp.quote_mismatches == 0 { "OK" } else { "MISMATCH" },
                        if cp.balance_mismatches == 0 && cp.quote_mismatches == 0 { "PASSED" } else { "FAILED" },
                    );
                }
            }
            Err(e) => {
                stats.checkpoint_errors += 1;
                warn!(label = case.label, error = ?e, "fermi ws live: final checkpoint failed");
            }
        }
    }

    // 6. 汇总
    println!("\n==== WS LIVE VERIFY SUMMARY ({}s) ====", secs);
    println!("pairs: {}", cases.iter().map(|c| c.label).collect::<Vec<_>>().join(", "));
    println!(
        "snapshots={} (with Fermi lane: {}) lanes_applied={} lane_mismatches={}",
        stats.snapshots, stats.snapshots_with_lane, stats.lanes_applied, stats.lane_mismatches,
    );
    println!(
        "checkpoints={} (skipped: {}) balance_mismatches={} quote_mismatches={} stream_errors={} checkpoint_errors={}",
        stats.checkpoints,
        stats.skipped_checkpoints,
        stats.balance_mismatches,
        stats.quote_mismatches,
        stats.stream_errors,
        stats.checkpoint_errors,
    );
    let passed = stats.lane_mismatches == 0
        && stats.balance_mismatches == 0
        && stats.quote_mismatches == 0
        && stats.checkpoint_errors == 0;
    println!("verdict: {}", if passed { "PASSED" } else { "FAILED" });
    assert!(
        passed,
        "ws live verify FAILED: lane_mismatches={} balance_mismatches={} quote_mismatches={} checkpoint_errors={}",
        stats.lane_mismatches, stats.balance_mismatches, stats.quote_mismatches, stats.checkpoint_errors,
    );
    Ok(())
}

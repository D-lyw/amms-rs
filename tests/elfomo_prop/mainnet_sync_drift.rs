//! ElfomoFi propAMM — XLayer **本地同步漂移巡检**（真实 AMMS 实例 + 历史区块回放 / 实时 flashblocks）。
//!
//! 与同目录 `ws_live_verify.rs`（WS 实时长跑，验 raw-tx → 本地直算 → 模拟整条链路）
//! 互补：本用例是**逐块对拍**的 drift 巡检，既能回放历史（复盘事故块附近本地状态是否
//! 漂移），也能实时跑（验生产帧流下通道是否丢消息）。
//!
//! 形态照 `tests/fermi_prop/mainnet_sync_drift.rs`：env 门控生产 RPC，未设置则跳过。
//!
//! ```bash
//! # 历史回放（默认）：在 START 块把真实例初始化成链上状态，然后逐块拉全量 logs 驱动它
//! XLAYER_RPC_URL=https://rpc.xlayer.tech \
//!   DRIFT_START_BLOCK=70372400 DRIFT_END_BLOCK=70372960 \
//!   cargo test --test elfomo_prop -- mainnet_sync_drift --nocapture
//!
//! # 实时：DRIFT_MODE=live 用 flashblocks 推流驱动（只能验证"现在"，无法回放历史）
//! XLAYER_RPC_URL=https://rpc.xlayer.tech DRIFT_MODE=live \
//!   cargo test --test elfomo_prop -- mainnet_sync_drift --nocapture
//! ```
//!
//! ## 为什么以"历史回放"为主
//!
//! XLayer 的 flashblocks 推流**不提供历史**，只能验当下。要复盘事故块附近本地状态
//! 是否漂移，必须回放：在 `START` 块把真实例初始化成链上状态，然后逐块把
//! **该区块的完整 logs**（`eth_getLogs`，规范序）喂给实例自身的
//! `StateSpace::sync()`，让**模块自己的同步逻辑**推进本地状态，再逐块与链上对拍。
//!
//! 实例构造与引擎一致（同 `AMM::ElfomoFiPropPool` / 同周期通道开关），
//! 只有"事件从哪来"不同：引擎来自 flashblocks 推流，回放来自逐块 logs。
//!
//! ## 两个通道都要回放
//!
//! Elfomo 的价格种子走 **flashblocks raw-tx** 通道（`updatePrices` 交易 calldata），
//! 这条通道没有日志、也没有历史端点，回放时用**模块自己的解析器**
//! `ElfomoFiPropPool::parse_update_prices_calldata` 从历史区块的原始交易里解析出
//! 同一个事件、按同块同序喂给 `apply_price_seed`（生产同块顺序：先 Router trade
//! 日志、后种子）。金库增量则完全走 `StateSpace::sync()` 的日志通道。
//!
//! ## 对拍四项（逐块，任一不等即漂移）
//!
//! 1. `price_seed` ←→ 链上 `slot1 >> 32`
//! 2. `levels.vault_xeth / vault_usdt0`（账本派生）←→ `token.balanceOf(vault)`
//! 3. `levels.profile_word` ←→ 链上 profile 槽
//! 4. `levels.{from_to,to_from}_levels` ←→ 链上 `getOrderbook` 逐位
//!
//! 漂移行附带**幻影报价**：同一探针金额下本地报价与链上报价的 bps 差（>0 = 本地报高）。
//!
//! ## 环境变量
//!
//! `XLAYER_RPC_URL`（或 `XLAYER_PROVIDER`）XLayer HTTP RPC（必填，未设置则跳过）
//! `XLAYER_FLASHBLOCKS_WS`               flashblocks WS（默认 `wss://ws.xlayer.tech/flashblocks`）
//! `DRIFT_MODE=replay|live`（默认 replay）、`DRIFT_START_BLOCK`、
//! `DRIFT_END_BLOCK`（默认 start+500）、`DRIFT_OK_EVERY`（默认 25，0=只打漂移）、
//! `DRIFT_ISOLATE_REALTIME`（默认 1：周期/对账通道拉到 6h，只测事件驱动通道）

use std::time::Duration;

use alloy::eips::BlockId;
use alloy::primitives::{Address, U256};
use alloy::providers::{Network, Provider, ProviderBuilder};
use alloy::rpc::types::Filter;
use amms::amms::amm::AMM;
use amms::amms::elfomo_prop::types::OrderbookSnapshot;
use amms::amms::elfomo_prop::{
    ElfomoFiPropPool, ELFOMO_FACTORY_ADDRESS, ELFOMO_POOL_ADDRESS, ELFOMO_ROUTER_ADDRESS,
    ELFOMO_TRADE_EVENT, ELFOMO_USDT0_ADDRESS, ELFOMO_VAULT_ADDRESS, ELFOMO_XETH_ADDRESS,
};
use amms::amms::Token;
use amms::state_space::{RealtimeSyncSource, StateSpaceBuilder};
use eyre::Result;
use tracing::{info, warn};

/// 正向探针：0.1 xETH → USDT0
const PROBE_XETH_IN: u128 = 100_000_000_000_000_000;
/// 反向探针：300 USDT0 → xETH
const PROBE_USDT0_IN: u128 = 300_000_000;
const ISOLATED_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// 非隔离模式复刻引擎的真实周期节奏。XLayer 生产配置（`configs/chains/196.toml`）
/// 未覆盖 `[sync_intervals]`，即用 `crates/config` 的默认值。
const NON_EVENT_SYNC_SECS: u64 = 221;
const CALIBER_LADDER_SYNC_SECS: u64 = 25;
const BINARYFI_SYNC_SECS: u64 = 15;
const CURVE_SYNC_SECS: u64 = 115;
const MAINTENANCE_SECS: u64 = 360;

fn provider_url() -> Option<String> {
    std::env::var("XLAYER_RPC_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("XLAYER_PROVIDER")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

fn env_flag(key: &str, default: bool) -> bool {
    std::env::var(key)
        .ok()
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            v == "1" || v == "true" || v == "yes" || v == "on"
        })
        .unwrap_or(default)
}

fn delta_bps(local: U256, chain: U256) -> Option<i128> {
    if chain.is_zero() {
        return None;
    }
    let diff = local.to::<i128>().saturating_sub(chain.to::<i128>());
    Some(diff.saturating_mul(10_000) / chain.to::<i128>().max(1))
}

fn u256(v: U256, decimals: u32) -> String {
    if decimals == 0 {
        return v.to_string();
    }
    let scale = U256::from(10u64).pow(U256::from(decimals));
    let int = v / scale;
    let frac = v % scale;
    format!(
        "{}.{}",
        int,
        format!("{:0>width$}", frac.to_string(), width = decimals as usize).trim_end_matches('0')
    )
}

fn target_pool() -> ElfomoFiPropPool {
    ElfomoFiPropPool {
        pool_address: ELFOMO_POOL_ADDRESS,
        token_x: ELFOMO_XETH_ADDRESS,
        token_y: ELFOMO_USDT0_ADDRESS,
        factory_address: ELFOMO_FACTORY_ADDRESS,
        router_address: ELFOMO_ROUTER_ADDRESS,
        vault_address: ELFOMO_VAULT_ADDRESS,
        tokens: vec![
            Token::from(ELFOMO_XETH_ADDRESS),
            Token::from(ELFOMO_USDT0_ADDRESS),
        ],
        ..Default::default()
    }
}

#[tokio::test]
async fn test_elfomo_prop_mainnet_sync_drift() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let Some(rpc_url) = provider_url() else {
        println!("Skipping test: XLAYER_RPC_URL/XLAYER_PROVIDER not set");
        return Ok(());
    };
    let ws_url = std::env::var("XLAYER_FLASHBLOCKS_WS")
        .unwrap_or_else(|_| "wss://ws.xlayer.tech/flashblocks".to_string());

    let provider = ProviderBuilder::new().connect_http(rpc_url.parse().unwrap());
    let chain_id = provider.get_chain_id().await?;

    let mode = std::env::var("DRIFT_MODE").unwrap_or_else(|_| "replay".to_string());
    let isolate = env_flag("DRIFT_ISOLATE_REALTIME", true);
    let ok_every = env_u64("DRIFT_OK_EVERY", 25);

    let head = provider.get_block_number().await?;
    let (start_block, end_block, replay) = if mode == "live" {
        (head, head, false)
    } else {
        let start = env_u64("DRIFT_START_BLOCK", head.saturating_sub(200));
        let end = env_u64("DRIFT_END_BLOCK", start.saturating_add(500));
        (start, end.min(head), true)
    };
    info!(
        chain_id, mode = %mode, start_block, end_block, head,
        "elfomo sync drift check starting"
    );

    let mut builder = StateSpaceBuilder::new(provider.clone())
        .with_amms(vec![AMM::ElfomoFiPropPool(target_pool())])
        .with_realtime_source(if replay {
            RealtimeSyncSource::WsLogs
        } else {
            RealtimeSyncSource::XlayerFlashblocksRaw
        })
        .with_realtime_ws_endpoints(vec![ws_url])
        .with_init_http_endpoint(rpc_url.clone())
        .block(start_block);
    if isolate {
        info!("DRIFT_ISOLATE_REALTIME=1: 周期/对账通道拉到 6h，本地状态只由事件驱动");
        builder = builder
            .with_non_event_sync_interval(ISOLATED_INTERVAL)
            .with_caliber_ladder_sync_interval(ISOLATED_INTERVAL)
            .with_caliber_reconcile_interval(ISOLATED_INTERVAL)
            .with_binaryfi_sync_interval(ISOLATED_INTERVAL)
            .with_elfomo_sync_interval(ISOLATED_INTERVAL)
            .with_curve_sync_interval(ISOLATED_INTERVAL)
            .with_maintenance_interval(ISOLATED_INTERVAL)
            .with_maintenance_coverage_interval(ISOLATED_INTERVAL)
            .with_pending_sync_worker_interval(ISOLATED_INTERVAL)
            .with_drift_probe_interval(ISOLATED_INTERVAL);
    } else {
        builder = builder
            .with_non_event_sync_interval(Duration::from_secs(NON_EVENT_SYNC_SECS))
            .with_caliber_ladder_sync_interval(Duration::from_secs(CALIBER_LADDER_SYNC_SECS))
            .with_binaryfi_sync_interval(Duration::from_secs(BINARYFI_SYNC_SECS))
            .with_maintenance_interval(Duration::from_secs(MAINTENANCE_SECS))
            .with_curve_sync_interval(Duration::from_secs(CURVE_SYNC_SECS));
    }

    let manager = builder.sync().await?;
    info!(pool = %ELFOMO_POOL_ADDRESS, "real AMMS instance initialized");

    if replay {
        run_replay(&provider, &manager, start_block, end_block, ok_every).await
    } else {
        run_live(&provider, &manager, chain_id, ok_every).await
    }
}

/// 历史回放：逐块把该块完整 logs（+ 同块 raw-tx 种子）喂给实例，再与链上对拍。
async fn run_replay<N, P>(
    provider: &P,
    manager: &amms::state_space::StateSpaceManager<N, P>,
    start_block: u64,
    end_block: u64,
    ok_every: u64,
) -> Result<()>
where
    N: Network,
    P: Provider<N> + Clone + 'static,
{
    let mut stats = Stats::default();
    let filter = Filter::new()
        .address(ELFOMO_ROUTER_ADDRESS)
        .event_signature(ELFOMO_TRADE_EVENT);

    for block in (start_block + 1)..=end_block {
        // 1) 该区块的完整 ElfomoTrade logs（规范序）→ 实例自身的日志驱动
        let logs = provider
            .get_logs(&filter.clone().from_block(block).to_block(block))
            .await?;

        let mut resync = false;
        if !logs.is_empty() {
            let mut guard = manager.state.write().await;
            match guard.sync(&logs) {
                Ok((_affected, needs_resync, _async)) => {
                    resync = !needs_resync.is_empty();
                }
                Err(e) => {
                    warn!(block, error = %e, "state.sync failed");
                }
            }
        }

        // 2) 同块 raw-tx 种子通道（flashblocks 专用通道的历史等价物）：
        //    用模块自己的解析器从历史区块原始交易里取 updatePrices calldata。
        let seeds = fetch_block_seeds(provider, block).await?;
        if !seeds.is_empty() {
            let mut guard = manager.state.write().await;
            if let Some(amm) = guard.get_mut(&ELFOMO_POOL_ADDRESS) {
                if let AMM::ElfomoFiPropPool(p) = amm {
                    for seed in &seeds {
                        p.apply_price_seed(*seed, block);
                    }
                }
            }
        }

        // 3) 与链上同块对拍
        let local = {
            let guard = manager.state.read().await;
            guard.state.values().find_map(|amm| match amm.as_ref() {
                AMM::ElfomoFiPropPool(p) => Some(p.clone()),
                _ => None,
            })
        };
        let Some(local) = local else {
            warn!(block, "elfomo pool missing from state");
            continue;
        };

        stats.blocks += 1;
        stats.seeds += seeds.len() as u64;
        stats.logs += logs.len() as u64;
        if resync {
            stats.resyncs += 1;
            println!("RESYNC block={block} logs={}", logs.len());
        }

        match check_at_block(provider, &local, block).await {
            Ok(report) => {
                if report.drifted {
                    stats.drifts += 1;
                    println!("DRIFT {}", report.line());
                } else {
                    stats.clean += 1;
                    if ok_every > 0 && stats.blocks % ok_every == 0 {
                        println!("OK    {}", report.line());
                    }
                }
            }
            Err(e) => warn!(block, error = %e, "chain truth read failed"),
        }
    }

    println!(
        "SUMMARY blocks={} logs={} seeds={} resyncs={} ok={} drift={}",
        stats.blocks, stats.logs, stats.seeds, stats.resyncs, stats.clean, stats.drifts
    );
    Ok(())
}

/// 实时：消费 flashblocks 推流；块切换时拿该块最后一份本地快照与链上同块对拍。
async fn run_live<N, P>(
    provider: &P,
    manager: &amms::state_space::StateSpaceManager<N, P>,
    chain_id: u64,
    ok_every: u64,
) -> Result<()>
where
    N: Network,
    P: Provider<N> + Clone + 'static,
{
    use futures::StreamExt;
    let mut stream = manager.subscribe_with_meta().await?;
    info!("realtime flashblocks stream consumed; block-anchored sampling started");

    let mut stats = Stats::default();
    let mut cur_block: u64 = 0;
    let mut cur_local: Option<ElfomoFiPropPool> = None;

    while let Some(item) = stream.next().await {
        let (meta, _affected) = match item {
            Ok(v) => v,
            Err(e) => {
                warn!(error = ?e, "realtime stream item error");
                continue;
            }
        };

        if meta.block_number != cur_block {
            if cur_block != 0 {
                if meta.block_number > cur_block + 1 {
                    println!(
                        "GAP   chain={chain_id} block={cur_block} next_block={} missed_blocks={}",
                        meta.block_number,
                        meta.block_number - cur_block - 1
                    );
                }
                if let Some(local) = cur_local.take() {
                    stats.blocks += 1;
                    match check_at_block(provider, &local, cur_block).await {
                        Ok(report) => {
                            if report.drifted {
                                stats.drifts += 1;
                                println!("DRIFT {}", report.line());
                            } else {
                                stats.clean += 1;
                                if ok_every > 0 && stats.blocks % ok_every == 0 {
                                    println!("OK    {}", report.line());
                                }
                            }
                        }
                        Err(e) => warn!(block = cur_block, error = %e, "chain truth read failed"),
                    }
                }
            }
            cur_block = meta.block_number;
            cur_local = None;
        }

        let local = {
            let guard = manager.state.read().await;
            guard.state.values().find_map(|amm| match amm.as_ref() {
                AMM::ElfomoFiPropPool(p) => Some(p.clone()),
                _ => None,
            })
        };
        if let Some(p) = local {
            cur_local = Some(p);
        }
    }
    println!(
        "SUMMARY blocks={} ok={} drift={}",
        stats.blocks, stats.clean, stats.drifts
    );
    Ok(())
}

#[derive(Default)]
struct Stats {
    blocks: u64,
    clean: u64,
    drifts: u64,
    logs: u64,
    seeds: u64,
    resyncs: u64,
}

/// 从历史区块的原始交易里解析 `updatePrices` 种子（模块自己的解析器）。
///
/// 用原始 JSON 而不是 alloy 解码：XLayer 的 OP Stack deposit 交易会让强类型
/// 反序列化整体失败（同 `tests/elfomo_prop/xlayer_fork_test.rs` 的注释）。
async fn fetch_block_seeds<N, P>(provider: &P, block: u64) -> Result<Vec<U256>>
where
    N: Network,
    P: Provider<N> + Clone,
{
    let raw: serde_json::Value = provider
        .client()
        .request("eth_getBlockByNumber", (format!("0x{block:x}"), true))
        .await?;
    let mut seeds = Vec::new();
    let Some(txs) = raw.get("transactions").and_then(|v| v.as_array()) else {
        return Ok(seeds);
    };
    for tx in txs {
        let to = tx.get("to").and_then(|v| v.as_str()).unwrap_or_default();
        if !to.eq_ignore_ascii_case(&format!("{ELFOMO_POOL_ADDRESS:#x}")) {
            continue;
        }
        let Some(input_hex) = tx.get("input").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(input) = alloy::hex::decode(input_hex.trim_start_matches("0x")) else {
            continue;
        };
        if let Some(seed) = ElfomoFiPropPool::parse_update_prices_calldata(&input) {
            seeds.push(seed);
        }
    }
    Ok(seeds)
}

#[derive(Debug)]
struct Report {
    pool: Address,
    block: u64,
    drifted: bool,
    details: Vec<String>,
}

impl Report {
    fn line(&self) -> String {
        format!(
            "kind=elfomo pool={} block={} {}",
            self.pool,
            self.block,
            self.details.join(" ")
        )
    }
}

/// 本地状态（块 B）vs 链上块 B 的完整状态。
async fn check_at_block<N, P>(provider: &P, local: &ElfomoFiPropPool, block: u64) -> Result<Report>
where
    N: Network,
    P: Provider<N> + Clone,
{
    let truth: OrderbookSnapshot = local
        .fetch_orderbook_snapshot(provider.clone(), BlockId::from(block))
        .await?;

    let mut details: Vec<String> = Vec::new();
    let mut drifted = false;

    if local.price_seed != truth.price_seed {
        drifted = true;
        details.push(format!(
            "seed: local={} chain={} local_seed_block={}",
            local.price_seed, truth.price_seed, local.price_seed_block
        ));
    }
    if local.levels.vault_xeth != truth.vault_xeth {
        drifted = true;
        details.push(format!(
            "vault_xeth: local={} chain={} delta={}",
            u256(local.levels.vault_xeth, 18),
            u256(truth.vault_xeth, 18),
            u256(local.levels.vault_xeth.abs_diff(truth.vault_xeth), 18)
        ));
    }
    if local.levels.vault_usdt0 != truth.vault_usdt0 {
        drifted = true;
        details.push(format!(
            "vault_usdt0: local={} chain={} delta={}",
            u256(local.levels.vault_usdt0, 6),
            u256(truth.vault_usdt0, 6),
            u256(local.levels.vault_usdt0.abs_diff(truth.vault_usdt0), 6)
        ));
    }
    if local.levels.profile_word != truth.profile_word {
        drifted = true;
        details.push(format!(
            "profile_word: local={:#x} chain={:#x}",
            local.levels.profile_word, truth.profile_word
        ));
    }
    if local.levels.from_to_levels != truth.from_to_levels
        || local.levels.to_from_levels != truth.to_from_levels
    {
        drifted = true;
        details.push(format!(
            "levels: local(fwd={}, rev={}) chain(fwd={}, rev={}) local[0]={:?} chain[0]={:?}",
            local.levels.from_to_levels.len(),
            local.levels.to_from_levels.len(),
            truth.from_to_levels.len(),
            truth.to_from_levels.len(),
            local.levels.from_to_levels.first(),
            truth.from_to_levels.first(),
        ));
    }

    let (xeth, usdt0) = (local.token_x, local.token_y);
    let local_fwd = ElfomoFiPropPool::simulate_swap_for_orderbook(
        &local.levels,
        xeth,
        usdt0,
        xeth,
        usdt0,
        U256::from(PROBE_XETH_IN),
    );
    let chain_fwd = ElfomoFiPropPool::simulate_swap_for_orderbook(
        &truth,
        xeth,
        usdt0,
        xeth,
        usdt0,
        U256::from(PROBE_XETH_IN),
    );
    let local_rev = ElfomoFiPropPool::simulate_swap_for_orderbook(
        &local.levels,
        xeth,
        usdt0,
        usdt0,
        xeth,
        U256::from(PROBE_USDT0_IN),
    );
    let chain_rev = ElfomoFiPropPool::simulate_swap_for_orderbook(
        &truth,
        xeth,
        usdt0,
        usdt0,
        xeth,
        U256::from(PROBE_USDT0_IN),
    );
    if let Some(bps) = delta_bps(local_fwd, chain_fwd) {
        if bps.abs() >= 1 {
            drifted = true;
            details.push(format!("quote_xeth_to_usdt0: delta_bps={bps}"));
        }
    }
    if let Some(bps) = delta_bps(local_rev, chain_rev) {
        if bps.abs() >= 1 {
            drifted = true;
            details.push(format!("quote_usdt0_to_xeth: delta_bps={bps}"));
        }
    }

    let (seed_block, missing_blocks) = local.seed_coverage();
    details.push(format!(
        "seed_block={seed_block} consecutive_missing_blocks={missing_blocks} last_synced_block={}",
        local.last_synced_block
    ));

    Ok(Report {
        pool: local.pool_address,
        block,
        drifted,
        details,
    })
}

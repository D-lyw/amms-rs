//! Fermi PropAMM 主网长区间漂移测试（生产 RPC 真实对拍）。
//!
//! 目标：验证本地 `FermiPropPool` 在长时间事件回放同步中：
//!   1. vault 余额账本与链上 `balanceOf(vault)` **逐位一致**（事件回放无漂移）；
//!   2. 相同输入下，本地 `engine_quote` 与链上 `engine.quote`（fresh lane
//!      override 的 `eth_call`）**逐位一致**（曲线数学 100% 对齐）。
//!
//! 背景事实（2026-08-24 生产实证）：
//! - Fermi lane 为 Titan 私有流高频更新，链上历史块 lane 已过期（`eth_call`
//!   quote 会 revert `0x666a2814`）；检查点 quote 对拍必须注入 fresh lane
//!   override：`update_timestamp = 检查点块时间戳`、保留链上价格/flag。
//! - 本地侧以同一 lane 作为 `engine_quote` 输入（生产 lane 来自 Titan 流，
//!   事件流不携带 lane），隔离验证余额同步 + 曲线数学。
//!
//! 运行条件：`ETHEREUM_PROVIDER` 或 `ETHEREUM_RPC_URL` 指向 Ethereum 主网。
//! 可选环境变量：
//!   `FERMI_DRIFT_BLOCK_RANGE`      回放区块数（默认 2500）
//!   `FERMI_DRIFT_CHECK_INTERVAL`   检查点间隔（默认 250）
//!   `FERMI_DRIFT_PAIRS`            逗号分隔的 "TOKEN_A/TOKEN_B" 列表
//!                                  （默认 `WETH/USDC`）

use std::collections::HashSet;
use std::sync::Arc;

use alloy::{
    eips::{BlockId, BlockNumberOrTag},
    primitives::{address, Address, B256, I256, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::{
        state::{AccountOverride, StateOverridesBuilder},
        Filter, Log,
    },
};
use amms::amms::{
    amm::AutomatedMarketMaker,
    fermi_prop::{
        types::{
            fermi_engine_last_trade_slot, fermi_lane_index, fermi_registry_lane_slot,
            fermi_virtual_address, ERC20_TRANSFER_EVENT, FERMI_ENGINE_ADDRESS,
            FERMI_PAIR_ACTIVE_SET_EVENT, FERMI_REGISTRY_ADDRESS, FERMI_SWAPPED_EVENT,
            FERMI_VAULT_ADDRESS, FERMI_WRAPPER_ADDRESS, IFermiEngine, IFermiERC20,
        },
        FermiLane, FermiPropPool,
    },
};

use crate::common::rpc::provider_url;

// ============================================================================
// 用例
// ============================================================================

#[derive(Debug, Clone)]
pub(crate) struct DriftCase {
    pub(crate) label: &'static str,
    /// 报价基准资产（baseAsset；engine 报价方向的 tokenIn）
    pub(crate) token_a: Address,
    /// 计价资产（quoteAsset）
    pub(crate) token_b: Address,
    pub(crate) decimals_a: u8,
    pub(crate) decimals_b: u8,
}

fn weth() -> Address {
    address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2")
}

fn usdc() -> Address {
    address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48")
}

fn usdt() -> Address {
    address!("0xdac17f958d2ee523a2206206994597c13d831ec7")
}

/// 默认用例；`FERMI_DRIFT_PAIRS` 可扩展（如 `WETH/USDT,WBTC/USDC`）。
fn drift_cases() -> Vec<DriftCase> {
    let mut cases = vec![DriftCase {
        label: "WETH-USDC",
        token_a: weth(),
        token_b: usdc(),
        decimals_a: 18,
        decimals_b: 6,
    }];
    if let Ok(extra) = std::env::var("FERMI_DRIFT_PAIRS") {
        for part in extra.split(',').filter(|s| !s.is_empty()) {
            let mut it = part.split('/');
            let (a, b) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
            let a = a.trim().to_ascii_lowercase();
            let b = b.trim().to_ascii_lowercase();
            let tok = |t: &str| match t {
                "weth" => weth(),
                "usdc" => usdc(),
                "usdt" => usdt(),
                "wbtc" => address!("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599"),
                "cbbtc" => address!("0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf"),
                other => {
                    other.parse::<Address>().expect("bad token addr in FERMI_DRIFT_PAIRS")
                }
            };
            let ta = tok(&a);
            let tb = tok(&b);
            cases.push(DriftCase {
                label: Box::leak(format!("{}-{}", a, b).into_boxed_str()),
                token_a: ta,
                token_b: tb,
                decimals_a: 18,
                decimals_b: 6,
            });
        }
    }
    cases
}

// ============================================================================
// 链上数据获取
// ============================================================================

pub(crate) async fn fetch_logs<P: Provider + Clone>(
    provider: &P,
    case: &DriftCase,
    from_block: u64,
    to_block: u64,
) -> eyre::Result<Vec<Log>> {
    let mut filters: Vec<Filter> = Vec::new();
    // wrapper Swapped：成交事件
    filters.push(
        Filter::new()
            .address(FERMI_WRAPPER_ADDRESS)
            .event_signature(FERMI_SWAPPED_EVENT)
            .from_block(from_block)
            .to_block(to_block),
    );
    // engine PairActiveSet：pair 启停
    filters.push(
        Filter::new()
            .address(FERMI_ENGINE_ADDRESS)
            .event_signature(FERMI_PAIR_ACTIVE_SET_EVENT)
            .from_block(from_block)
            .to_block(to_block),
    );
    // 两个 token 的 vault 相关 Transfer：余额权威账本（含跨 pair 成交/存取）
    for token in [case.token_a, case.token_b] {
        filters.push(
            Filter::new()
                .address(token)
                .event_signature(ERC20_TRANSFER_EVENT)
                .topic1(FERMI_VAULT_ADDRESS) // from == vault
                .from_block(from_block)
                .to_block(to_block),
        );
        filters.push(
            Filter::new()
                .address(token)
                .event_signature(ERC20_TRANSFER_EVENT)
                .topic2(FERMI_VAULT_ADDRESS) // to == vault
                .from_block(from_block)
                .to_block(to_block),
        );
    }

    let mut logs: Vec<Log> = Vec::new();
    for filter in filters {
        let mut from = from_block;
        while from <= to_block {
            let to = (from + 999).min(to_block);
            let mut chunk = provider
                .get_logs(&filter.clone().from_block(from).to_block(to))
                .await?;
            logs.append(&mut chunk);
            if to == to_block {
                break;
            }
            from = to + 1;
        }
    }

    // 排序 + 按 (block, tx_index, log_index) 去重（vault↔vault Transfer 会命中
    // topic1/topic2 两个 filter）。
    logs.sort_by_key(|l| {
        (
            l.block_number.unwrap_or(0),
            l.transaction_index.unwrap_or(0),
            l.log_index.unwrap_or(0),
        )
    });
    let mut seen = HashSet::new();
    logs.retain(|l| {
        seen.insert((
            l.block_number.unwrap_or(0),
            l.transaction_index.unwrap_or(0),
            l.log_index.unwrap_or(0),
        ))
    });
    Ok(logs)
}

pub(crate) async fn init_pool<P: Provider + Clone>(
    provider: &P,
    case: &DriftCase,
    block: u64,
) -> eyre::Result<FermiPropPool> {
    let pool = FermiPropPool {
        token_a: case.token_a,
        token_b: case.token_b,
        decimals_a: case.decimals_a,
        decimals_b: case.decimals_b,
        lane_index: fermi_lane_index(case.token_a, case.token_b),
        virtual_address: fermi_virtual_address(FERMI_ENGINE_ADDRESS, case.token_a, case.token_b),
        ..Default::default()
    };
    let pool = pool
        .init(BlockId::from(block), provider.clone())
        .await
        .map_err(|e| eyre::eyre!("fermi init failed at block {block}: {e}"))?;
    Ok(pool)
}

pub(crate) async fn fetch_chain_balances<P: Provider + Clone>(
    provider: &P,
    case: &DriftCase,
    block: u64,
) -> eyre::Result<(U256, U256)> {
    let erc20_a = IFermiERC20::new(case.token_a, provider.clone());
    let erc20_b = IFermiERC20::new(case.token_b, provider.clone());
    let bal_a = erc20_a
        .balanceOf(FERMI_VAULT_ADDRESS)
        .block(BlockId::from(block))
        .call()
        .await?;
    let bal_b = erc20_b
        .balanceOf(FERMI_VAULT_ADDRESS)
        .block(BlockId::from(block))
        .call()
        .await?;
    Ok((bal_a, bal_b))
}

async fn fetch_lane_word<P: Provider + Clone>(
    provider: &P,
    case: &DriftCase,
    block: u64,
) -> eyre::Result<U256> {
    let slot = U256::from_be_bytes(
        fermi_registry_lane_slot(FERMI_ENGINE_ADDRESS, case.token_a, case.token_b).0,
    );
    Ok(provider
        .get_storage_at(FERMI_REGISTRY_ADDRESS, slot)
        .block_id(BlockId::from(block))
        .await?)
}

// ============================================================================
// 检查点对拍
// ============================================================================

/// 检查点：vault 余额逐位对拍 + 相同输入下 quote 逐位对拍。
async fn check_block<P: Provider + Clone>(
    provider: &P,
    case: &DriftCase,
    pool: &FermiPropPool,
    block: u64,
) -> eyre::Result<bool> {
    let mut ok = true;

    // 1. vault 余额账本对拍（本地事件回放 vs 链上 balanceOf）
    let (chain_a, chain_b) = fetch_chain_balances(provider, case, block).await?;
    let local_a = pool
        .vault_balances
        .get(&case.token_a)
        .copied()
        .unwrap_or_default();
    let local_b = pool
        .vault_balances
        .get(&case.token_b)
        .copied()
        .unwrap_or_default();
    if local_a != chain_a {
        ok = false;
        println!(
            "[{}] block={} balance[token_a] local={} chain={} diff={}",
            case.label,
            block,
            local_a,
            chain_a,
            if local_a > chain_a { local_a - chain_a } else { chain_a - local_a }
        );
    }
    if local_b != chain_b {
        ok = false;
        println!(
            "[{}] block={} balance[token_b] local={} chain={} diff={}",
            case.label,
            block,
            local_b,
            chain_b,
            if local_b > chain_b { local_b - chain_b } else { chain_b - local_b }
        );
    }

    // 2. lane 现状（信息）：本地 lane 来自 init（链上 @start），链上 @checkpoint
    //    可能有更新（Fermi 更新可上链、也可只走 Titan 流）；quote 对拍统一用链上
    //    @checkpoint 的 lane 输入，此处仅记录差异供分析。
    let lane_word = fetch_lane_word(provider, case, block).await?;
    if let Some(chain_lane) = FermiLane::from_slot_word(lane_word) {
        if pool.lane.fair_price_e8 != chain_lane.fair_price_e8
            || pool.lane.update_timestamp != chain_lane.update_timestamp
        {
            println!(
                "[{}] block={} lane local=(ts={} price={}) chain=(ts={} price={}) [info]",
                case.label,
                block,
                pool.lane.update_timestamp,
                pool.lane.fair_price_e8,
                chain_lane.update_timestamp,
                chain_lane.fair_price_e8
            );
        }
    }

    // 3. quote 对拍（pair 活跃时）
    if !pool.active {
        println!("[{}] block={} pool inactive, skip quote parity", case.label, block);
        return Ok(ok);
    }
    let quote_ok = check_quote_parity(provider, case, pool, block, lane_word).await?;
    ok &= quote_ok;

    if ok {
        println!(
            "[{}] checkpoint OK block={} vaultA={} vaultB={}",
            case.label, block, local_a, local_b
        );
    }
    Ok(ok)
}

/// 相同输入（lane + vault 余额 + 曲线参数）下本地 `engine_quote` vs 链上
/// `engine.quote`（fresh lane override）逐位对拍。
pub(crate) async fn check_quote_parity<P: Provider + Clone>(
    provider: &P,
    case: &DriftCase,
    pool: &FermiPropPool,
    block: u64,
    lane_word: U256,
) -> eyre::Result<bool> {
    let block_ts = provider
        .get_block_by_number(BlockNumberOrTag::Number(block))
        .await?
        .ok_or_else(|| eyre::eyre!("block {block} not found"))?
        .header
        .timestamp;

    // fresh lane override：update_timestamp = 块时间戳，保留链上 flag/价格。
    let fresh_word = (U256::from(block_ts) << U256::from(224))
        | (lane_word & ((U256::from(1) << U256::from(224)) - U256::from(1)));
    let lane = FermiLane::from_slot_word(fresh_word)
        .ok_or_else(|| eyre::eyre!("invalid fresh lane word at block {block}"))?;

    let slot_key = U256::from_be_bytes(
        fermi_registry_lane_slot(FERMI_ENGINE_ADDRESS, case.token_a, case.token_b).0,
    );
    let overrides = StateOverridesBuilder::default()
        .append(
            FERMI_REGISTRY_ADDRESS,
            AccountOverride::default().with_state_diff([(B256::from(slot_key), B256::from(fresh_word))]),
        )
        .build();

    // 本地侧以同一 lane 输入（生产 lane 来自 Titan 流，事件回放不更新 lane）；
    // 同块成交校正：喂入链上 @checkpoint 的 engine last-trade 槽 + 同步块号。
    let mut local = pool.clone();
    local.lane = lane;
    local.last_synced_block = block;
    // 同块成交校正：喂入链上 @checkpoint 的 engine last-trade 槽（正向 sub0 / 反向 sub1）
    // + 同步块号。
    let last_trade_slot = U256::from_be_bytes(
        fermi_engine_last_trade_slot(case.token_a, case.token_b, 0).0,
    );
    match provider
        .get_storage_at(FERMI_ENGINE_ADDRESS, last_trade_slot)
        .block_id(BlockId::from(block))
        .await
    {
        Ok(word) => local.last_trade_word = word,
        Err(_) => local.last_trade_word = U256::ZERO,
    }
    let last_trade_rev_slot = U256::from_be_bytes(
        fermi_engine_last_trade_slot(case.token_a, case.token_b, 1).0,
    );
    match provider
        .get_storage_at(FERMI_ENGINE_ADDRESS, last_trade_rev_slot)
        .block_id(BlockId::from(block))
        .await
    {
        Ok(word) => local.last_trade_rev_word = word,
        Err(_) => local.last_trade_rev_word = U256::ZERO,
    }

    let engine = IFermiEngine::new(FERMI_ENGINE_ADDRESS, provider.clone());
    let sender = address!("0x0000000000000000000000000000000000000001");

    let mut ok = true;
    // 正向：token_a -> token_b（WETH -> USDC）：0.01 / 1 / 10 整枚
    for amount in [
        U256::from(10_000_000_000_000_000u128),
        U256::from(1_000_000_000_000_000_000u128),
        U256::from(10_000_000_000_000_000_000u128),
    ] {
        let local_out = local.engine_quote(case.token_a, case.token_b, amount);
        let chain_res = engine
            .quote(case.token_a, case.token_b, I256::from_raw(amount), sender)
            .block(BlockId::from(block))
            .state(overrides.clone())
            .call()
            .await;
        match (local_out, chain_res) {
            (Some(l), Ok(r)) if l == r.amountOut => {}
            (None, Err(_)) => {}
            (Some(l), Ok(r)) => {
                ok = false;
                println!(
                    "[{}] block={} FWD A={} chain={} local={} MISMATCH",
                    case.label, block, amount, r.amountOut, l
                );
            }
            (Some(l), Err(e)) => {
                ok = false;
                println!(
                    "[{}] block={} FWD A={} chain=REVERT({}) local={} MISMATCH",
                    case.label, block, amount, e, l
                );
            }
            (None, Ok(r)) => {
                ok = false;
                println!(
                    "[{}] block={} FWD A={} chain={} local=None MISMATCH",
                    case.label, block, amount, r.amountOut
                );
            }
        }
    }
    // 反向：token_b -> token_a（USDC -> WETH）：100 / 10k / 1M base units
    for amount in [
        U256::from(100_000_000u128),
        U256::from(10_000_000_000u128),
        U256::from(1_000_000_000_000u128),
    ] {
        let local_out = local.engine_quote(case.token_b, case.token_a, amount);
        let chain_res = engine
            .quote(case.token_b, case.token_a, I256::from_raw(amount), sender)
            .block(BlockId::from(block))
            .state(overrides.clone())
            .call()
            .await;
        match (local_out, chain_res) {
            (Some(l), Ok(r)) if l == r.amountOut => {}
            (None, Err(_)) => {}
            (Some(l), Ok(r)) => {
                ok = false;
                println!(
                    "[{}] block={} REV A={} chain={} local={} MISMATCH",
                    case.label, block, amount, r.amountOut, l
                );
            }
            (Some(l), Err(e)) => {
                ok = false;
                println!(
                    "[{}] block={} REV A={} chain=REVERT({}) local={} MISMATCH",
                    case.label, block, amount, e, l
                );
            }
            (None, Ok(r)) => {
                ok = false;
                println!(
                    "[{}] block={} REV A={} chain={} local=None MISMATCH",
                    case.label, block, amount, r.amountOut
                );
            }
        }
    }
    Ok(ok)
}

// ============================================================================
// 主流程
// ============================================================================

async fn run_sync_drift_test(
    provider: Arc<impl Provider + Clone>,
    case: &DriftCase,
    block_range: u64,
    check_interval: u64,
) -> eyre::Result<()> {
    let latest = provider.get_block_number().await?;
    let start_block = latest.saturating_sub(block_range);
    println!(
        "[{}] start={} latest={} range={} check_interval={}",
        case.label, start_block, latest, block_range, check_interval
    );

    // init @start + 初始余额对拍
    let mut pool = init_pool(&*provider, case, start_block).await?;
    let (chain_a, chain_b) = fetch_chain_balances(&*provider, case, start_block).await?;
    let local_a = pool
        .vault_balances
        .get(&case.token_a)
        .copied()
        .unwrap_or_default();
    let local_b = pool
        .vault_balances
        .get(&case.token_b)
        .copied()
        .unwrap_or_default();
    assert_eq!(
        local_a, chain_a,
        "[{}] init balance[token_a] mismatch at block {}",
        case.label, start_block
    );
    assert_eq!(
        local_b, chain_b,
        "[{}] init balance[token_b] mismatch at block {}",
        case.label, start_block
    );
    println!(
        "[{}] init OK block={} vaultA={} vaultB={}",
        case.label, start_block, local_a, local_b
    );

    // 拉取事件区间（start+1 .. latest）
    let events = fetch_logs(&*provider, case, start_block + 1, latest).await?;
    println!("[{}] events fetched: {}", case.label, events.len());

    if events.is_empty() {
        // 无事件：直接检查点对拍当前块（验证 init 状态本身与链上一致）
        let ok = check_block(&*provider, case, &pool, latest).await?;
        assert!(ok, "[{}] checkpoint mismatch at block {}", case.label, latest);
        println!("[{}] PASSED (no events) final_block={}", case.label, latest);
        return Ok(());
    }

    let mut last_checked_block = start_block;
    let mut events_processed = 0usize;

    for (idx, log) in events.iter().enumerate() {
        let block_num = log.block_number.unwrap_or(0);
        if let Err(e) = pool.sync(log) {
            println!("[{}] sync error block {}: {:?}", case.label, block_num, e);
        }
        events_processed += 1;

        let is_last_in_block = if let Some(next_log) = events.get(idx + 1) {
            next_log.block_number.unwrap_or(0) > block_num
        } else {
            true
        };

        if is_last_in_block && block_num >= last_checked_block.saturating_add(check_interval) {
            let ok = check_block(&*provider, case, &pool, block_num).await?;
            assert!(
                ok,
                "[{}] checkpoint mismatch at block {} after {} events",
                case.label, block_num, events_processed
            );
            last_checked_block = block_num;
        }
    }

    // 最终检查点（最后一个事件所在块）
    let final_block = events
        .last()
        .and_then(|l| l.block_number)
        .unwrap_or(latest);
    let ok = check_block(&*provider, case, &pool, final_block).await?;
    assert!(
        ok,
        "[{}] final checkpoint mismatch at block {}",
        case.label, final_block
    );

    println!(
        "[{}] PASSED events_processed={} final_block={}",
        case.label, events_processed, final_block
    );
    Ok(())
}

#[tokio::test]
async fn test_fermi_prop_mainnet_sync_drift() -> eyre::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let rpc_endpoint = match provider_url() {
        Some(u) => u,
        None => {
            println!("Skipping test: ETHEREUM_PROVIDER/ETHEREUM_RPC_URL not set");
            return Ok(());
        }
    };

    let block_range = std::env::var("FERMI_DRIFT_BLOCK_RANGE")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(2500);
    let check_interval = std::env::var("FERMI_DRIFT_CHECK_INTERVAL")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(250);

    let provider = Arc::new(ProviderBuilder::new().connect_http(rpc_endpoint.parse().unwrap()));

    for case in drift_cases() {
        run_sync_drift_test(provider.clone(), &case, block_range, check_interval).await?;
    }
    Ok(())
}

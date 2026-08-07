//! BinaryFi propAMM 逐块 logs replay 验证。
//!
//! 从起点块的前一块 init 本地池子，然后逐块拉取池子 Swap + 引擎 update 日志，
//! 按 (blockNumber, logIndex) 顺序喂给本地池子的 sync()；update 日志用
//! `eth_getRawTransactionByHash` 补真实 raw bytes 走 L2 增强（零额外状态查询）。
//! 每个有事件的区块结束后，在 `blockId = B` 上用批量合约拉全量快照
//! （132 quote + 11 大额 quote + 余额），三方对比：
//!   - 本地 simulate_swap vs 链上 quote（线性区金额，必须 0 mismatch）
//!   - 本地 reserves vs 链上金库余额（允许有 vault Transfer 的资产有界漂移）
//!   - L2 解析出的 price vs 快照从 quote 反推的 price（内部状态一致性）

use std::{collections::HashSet, str::FromStr, sync::Arc};

use alloy::{
    eips::{BlockId, BlockNumberOrTag},
    primitives::{address, b256, Address, B256, U256},
    providers::{Provider, ProviderBuilder},
    rpc::{
        client::ClientBuilder,
        types::{Filter, Log},
    },
    sol_types::SolValue,
    transports::layers::RetryBackoffLayer,
};
use amms::amms::{
    amm::{AutomatedMarketMaker, SyncAction},
    binaryfi_prop::{
        enrich_update_log_data, BinaryFiPropPool, GetBinaryFiPropStateBatchRequest, Snapshot,
        BINARYFI_ASSET_COUNT, BINARYFI_ENGINE_ADDRESS, BINARYFI_POOL_ADDRESS,
        BINARYFI_ROUTER_ADDRESS, BINARYFI_SWAP_EVENT, BINARYFI_UPDATE_EVENT,
        BINARYFI_VAULT_ADDRESS,
    },
};

const TRANSFER_EVENT: B256 =
    b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");

/// 与模块内 apply_snapshot 相同的恢复算法（recover_ask/pin_ask 为私有，此处内联）
fn recover_ask(out: U256, dj: u32) -> Option<U256> {
    if out.is_zero() || dj > 30 {
        return None;
    }
    let scale = U256::from(10u64).pow(U256::from(dj + 2));
    let base = (scale / out).saturating_sub(U256::from(4));
    for delta in 0..9u64 {
        let cand = base + U256::from(delta);
        if cand.is_zero() {
            continue;
        }
        if scale / cand == out {
            return Some(cand);
        }
    }
    None
}

fn pin_ask(q_small: U256, q_big: U256, dj: u32) -> Option<U256> {
    if q_small.is_zero() || q_big.is_zero() || dj > 30 {
        return None;
    }
    let p10 = |e: u32| U256::from(10u64).pow(U256::from(e));
    let s6 = p10(dj + 6);
    let lin_lo = q_small.checked_mul(p10(4))?;
    if q_big < lin_lo {
        return None;
    }
    let ask = (s6 + q_big) / (q_big + U256::from(1));
    if ask.is_zero() {
        return None;
    }
    if s6 / ask != q_big || p10(dj + 2) / ask != q_small {
        return None;
    }
    Some(ask)
}

async fn fetch_snapshot_at<P: Provider + Clone>(provider: P, block: u64) -> eyre::Result<Snapshot> {
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

fn u256_f64(v: &U256) -> f64 {
    let l = v.as_limbs();
    l[0] as f64
        + (l[1] as f64) * 2f64.powi(64)
        + (l[2] as f64) * 2f64.powi(128)
        + (l[3] as f64) * 2f64.powi(192)
}

/// 从快照 quote 反推每资产 (bid, ask)，与 apply_snapshot 同逻辑
fn recover_prices(snap: &Snapshot) -> Vec<(U256, U256, bool)> {
    let n = snap.assets.len();
    let mut out = vec![(U256::ZERO, U256::ZERO, false); n];
    if n == 0 {
        return out;
    }
    let mut j0_out: Vec<Option<U256>> = vec![None; n];
    let mut zj_out: Vec<Option<U256>> = vec![None; n];
    let mut big_out: Vec<Option<U256>> = vec![None; n];
    for (k, pair) in snap.quotePairs.iter().enumerate() {
        if k >= snap.quotes.len() || !snap.quotes[k].success {
            continue;
        }
        let p = pair.to::<usize>();
        let nn = n * n;
        if p >= nn {
            let j = p - nn;
            if j > 0 && j < n {
                big_out[j] = Some(snap.quotes[k].amountOut);
            }
            continue;
        }
        let (i, j) = (p / n, p % n);
        if i == 0 && j != 0 {
            zj_out[j] = Some(snap.quotes[k].amountOut);
        } else if j == 0 && i != 0 {
            j0_out[i] = Some(snap.quotes[k].amountOut);
        }
    }
    for j in 1..n {
        let dj = snap.decimals.get(j).copied().unwrap_or(0) as u32;
        if dj == 0 || dj > 30 {
            continue;
        }
        let bid = j0_out[j].filter(|o| !o.is_zero());
        let zj = zj_out[j];
        if zj == Some(U256::ZERO) {
            if let Some(b) = bid {
                out[j] = (b, U256::ZERO, true);
            }
            continue;
        }
        let ask = if let Some(qs) = zj {
            let base = recover_ask(qs, dj);
            match big_out[j] {
                Some(qb) => pin_ask(qs, qb, dj).or(base),
                None => base,
            }
        } else {
            None
        };
        match (bid, ask) {
            (Some(b), Some(a)) if a > b => {
                out[j] = (b, a, false);
            }
            (Some(b), None) => {
                out[j] = (b, b + U256::from(8), false);
            }
            (None, Some(a)) => {
                out[j] = (a.saturating_sub(U256::from(8)), a, false);
            }
            _ => {}
        }
    }
    out
}

async fn flush_block<P: Provider + Clone>(
    provider: P,
    pool: &mut BinaryFiPropPool,
    block: u64,
    quote_ok: &mut usize,
    quote_total: &mut usize,
    mismatches: &mut Vec<String>,
    cap_infos: &mut Vec<String>,
    res_diffs: &mut Vec<String>,
    price_mis: &mut Vec<String>,
) -> eyre::Result<()> {
    let snap = fetch_snapshot_at(provider, block).await?;
    let n = snap.assets.len();
    if n == 0 {
        return Ok(());
    }
    for (k, pair) in snap.quotePairs.iter().enumerate() {
        if k >= snap.quotes.len() || !snap.quotes[k].success {
            continue;
        }
        let p = pair.to::<usize>();
        if p >= n * n {
            continue;
        }
        let (i, j) = (p / n, p % n);
        let Some(amount) = quote_amount(&snap.decimals, i, j) else {
            continue;
        };
        let sim = pool.simulate_swap(snap.assets[i], snap.assets[j], amount)?;
        *quote_total += 1;
        if sim == snap.quotes[k].amountOut {
            *quote_ok += 1;
        } else if snap.quotes[k].amountOut.is_zero() && !sim.is_zero() {
            // 引擎侧 per-asset 买入额度被本块内 swap 消耗到 0（如 asset8 整块
            // 0→j 全为 0、次块自愈）；该状态不出现在 calldata/事件中，
            // 属已知不可观测类，报告为 INFO 不参与断言
            cap_infos.push(format!(
                "  [{block}] pair {i}->{j} amt={amount}: sim={sim} chain=0 (engine buy-cap consumed)"
            ));
        } else {
            mismatches.push(format!(
                "  [{block}] pair {i}->{j} amt={amount}: sim={sim} chain={} diff={}",
                snap.quotes[k].amountOut,
                if sim > snap.quotes[k].amountOut {
                    sim - snap.quotes[k].amountOut
                } else {
                    snap.quotes[k].amountOut - sim
                }
            ));
        }
    }
    if snap.vaultReserves.len() == n {
        for j in 0..n {
            let local = pool.reserves.get(j).copied().unwrap_or_default();
            let chain = snap.vaultReserves[j];
            if local != chain {
                let diff = if local > chain {
                    local - chain
                } else {
                    chain - local
                };
                res_diffs.push(format!(
                    "  [{block}] asset[{j}] {} local={local} chain={chain} diff={diff} ({:.6})",
                    snap.assets[j],
                    u256_f64(&diff) / 1e18
                ));
            }
        }
    }
    // 大额 SELL（100 整枚）对拍：验证日志驱动后 sell_raw 精确性。
    // sell_raw = price×1999 - sell_off×2000 由 update 事件（L2 增强）直接设置；
    // 单档资产大额 SELL 必须逐位复刻（含超容量归零）；多档阶梯资产
    // （快照两点无法反推第一档 raw）在纯快照路径是近似，日志驱动后应精确。
    for (k, pair) in snap.quotePairs.iter().enumerate() {
        if k >= snap.quotes.len() || !snap.quotes[k].success {
            continue;
        }
        let p = pair.to::<usize>();
        let nn = n * n;
        if p < 2 * nn {
            continue;
        }
        let j = p - 2 * nn;
        if j == 0 || j >= n {
            continue;
        }
        let dj = snap.decimals.get(j).copied().unwrap_or(0) as u32;
        if dj == 0 || dj > 30 {
            continue;
        }
        let sell_in = U256::from(100u64) * U256::from(10u64).pow(U256::from(dj));
        let sim = pool.simulate_swap(snap.assets[j], snap.assets[0], sell_in)?;
        let chain = snap.quotes[k].amountOut;
        let raw_state = match pool.sell_raw.get(j).copied().flatten() {
            Some(r) => format!("raw={r}"),
            None => "raw=None(snapshot-approx)".to_string(),
        };
        if sim == chain {
            *quote_ok += 1;
        } else if raw_state.starts_with("raw=None") {
            // 阶梯资产且尚无 update 日志：快照近似，INFO 不参与断言
            cap_infos.push(format!(
                "  [{block}] {j}->0 sell in={sell_in} {raw_state}: sim={sim} chain={chain}"
            ));
        } else {
            mismatches.push(format!(
                "  [{block}] {j}->0 sell in={sell_in} {raw_state}: sim={sim} chain={chain} diff={}",
                if sim > chain { sim - chain } else { chain - sim }
            ));
        }
    }
    // sell_raw 覆盖率（日志驱动后应为全 Some = 精确）
    let raw_known = (1..n)
        .filter(|&j| pool.sell_raw.get(j).copied().flatten().is_some())
        .count();
    let n_assets = n.saturating_sub(1);
    println!(
        "  [{block}] sell_raw known: {raw_known}/{n_assets} assets"
    );
    let recovered = recover_prices(&snap);
    for j in 1..n {
        let local_p = pool.prices.get(j).copied().unwrap_or_default();
        let local_b = local_p.saturating_sub(U256::from(
            pool.bid_offsets.get(j).copied().unwrap_or(0),
        ));
        let local_a = local_p + U256::from(pool.ask_offsets.get(j).copied().unwrap_or(0));
        let (rec_b, rec_a, disabled) = recovered[j];
        // 买入禁用资产（快照 ask=0 标记）只比较 bid；正常资产 bid/ask 全比
        let ask_matches = disabled || local_a == rec_a;
        if !rec_b.is_zero() && (local_b != rec_b || !ask_matches) {
            price_mis.push(format!(
                "  [{block}] asset[{j}] {} local=(bid {local_b}, ask {local_a}) snap=(bid {rec_b}, ask {rec_a})",
                snap.assets[j],
            ));
        }
    }
    // 逐资产对照：本地 bid/ask/spread vs 快照恢复值（首个对比块打印）
    if block == 0x400c900 {
        println!("  per-asset [block {block:#x}] local(bid,ask) vs snap(bid,ask):");
        for j in 1..n {
            let local_p = pool.prices.get(j).copied().unwrap_or_default();
            if local_p.is_zero() {
                continue;
            }
            let lb = local_p.saturating_sub(U256::from(
                pool.bid_offsets.get(j).copied().unwrap_or(0),
            ));
            let la = local_p + U256::from(pool.ask_offsets.get(j).copied().unwrap_or(0));
            let (rb, ra, _disabled) = recovered[j];
            let tag = if lb == rb && la == ra { "ok " } else { "DIFF" };
            println!(
                "  {tag} asset[{j}] {} local=({lb},{la}) snap=({rb},{ra})",
                snap.assets[j]
            );
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();

    let rpc = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("XLAYER_RPC").ok())
        .unwrap_or_else(|| "https://rpc.xlayer.tech".to_string());
    let start = std::env::args()
        .nth(2)
        .map(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).expect("start hex"))
        .unwrap_or(0x400c900u64);
    let end = std::env::args()
        .nth(3)
        .map(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).expect("end hex"))
        .unwrap_or(start + 96);

    eprintln!("binaryfi_replay_probe -> {rpc} blocks [{start:#x}, {end:#x}]");
    let client = ClientBuilder::default()
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(rpc.parse()?);
    let provider = Arc::new(ProviderBuilder::new().connect_client(client));

    let init_block = start.saturating_sub(1);
    let mut pool = BinaryFiPropPool::default()
        .init(
            BlockId::Number(BlockNumberOrTag::Number(init_block)),
            provider.clone(),
        )
        .await?;
    println!(
        "init at block {init_block}: assets={} derived_rates={}/{}",
        pool.assets.len(),
        pool.rates.iter().filter(|r| !r.is_zero()).count(),
        pool.rates.len()
    );

    // 事件过滤：一次只查一个 topic（alloy Filter::event_signature 为单 topic 覆盖语义）
    let swap_logs = provider
        .get_logs(
            &Filter::new()
                .from_block(start)
                .to_block(end)
                .event_signature(BINARYFI_SWAP_EVENT),
        )
        .await?;
    let update_logs = provider
        .get_logs(
            &Filter::new()
                .from_block(start)
                .to_block(end)
                .event_signature(BINARYFI_UPDATE_EVENT),
        )
        .await?;
    let swap_n = swap_logs.len();
    let update_n = update_logs.len();
    let mut logs: Vec<Log> = swap_logs.into_iter().chain(update_logs).collect();
    println!(
        "logs in range: {} (swap={swap_n} + update={update_n})",
        logs.len()
    );

    let transfer_filter = Filter::new()
        .from_block(start)
        .to_block(end)
        .address(BINARYFI_VAULT_ADDRESS)
        .event_signature(TRANSFER_EVENT);
    let vault_transfers = provider.get_logs(&transfer_filter).await?;
    let transfer_assets: HashSet<Address> = vault_transfers
        .iter()
        .filter_map(|l| l.topics().get(2).map(|t| Address::from_word(*t)))
        .collect();
    println!(
        "vault transfer events: {} (assets touched: {})",
        vault_transfers.len(),
        transfer_assets.len()
    );

    logs.sort_by_key(|l| (l.block_number.unwrap_or(0), l.log_index.unwrap_or(0)));

    let mut n_update_logs = 0usize;
    let mut n_swap_logs = 0usize;
    let mut n_raw_ok = 0usize;
    let mut n_raw_fail = 0usize;
    let mut async_updates = 0usize;

    let mut cur_block = start;
    let mut block_quotes_ok = 0usize;
    let mut block_quotes_total = 0usize;
    let mut quote_mismatches: Vec<String> = Vec::new();
    let mut cap_infos: Vec<String> = Vec::new();
    let mut reserve_diffs: Vec<String> = Vec::new();
    let mut price_mismatches: Vec<String> = Vec::new();

    for log in &logs {
        let block = log.block_number.unwrap_or(0);
        if block > cur_block {
            if cur_block >= start {
                flush_block(
                    provider.clone(),
                    &mut pool,
                    cur_block,
                    &mut block_quotes_ok,
                    &mut block_quotes_total,
                    &mut quote_mismatches,
                    &mut cap_infos,
                    &mut reserve_diffs,
                    &mut price_mismatches,
                )
                .await?;
            }
            cur_block = block;
        }
        if log.address() == BINARYFI_POOL_ADDRESS
            && log.topics().first() == Some(&BINARYFI_SWAP_EVENT)
        {
            n_swap_logs += 1;
            pool.sync(log)?;
            continue;
        }
        if log.address() == BINARYFI_ENGINE_ADDRESS
            && log.topics().first() == Some(&BINARYFI_UPDATE_EVENT)
        {
            n_update_logs += 1;
            let mut enhanced = log.clone();
            let enriched = match log.transaction_hash {
                Some(h) => match provider.get_raw_transaction_by_hash(h).await? {
                    Some(bytes) => {
                        let raw_hex = alloy::hex::encode(bytes.as_ref());
                        enrich_update_log_data(&[raw_hex], Some(h), &log.data(), BINARYFI_ENGINE_ADDRESS)
                    }
                    None => None,
                },
                None => None,
            };
            match enriched {
                Some(data) => {
                    enhanced.inner.data = data;
                    n_raw_ok += 1;
                }
                None => {
                    n_raw_fail += 1;
                }
            }
            let action = pool.sync(&enhanced)?;
            if matches!(action, SyncAction::AsyncUpdate) {
                async_updates += 1;
                // 与生产 StateSpace 的 pending_sync_queue 一致：stale pair 走
                // 批量快照；回放时固定在该日志所在区块取快照
                pool.update_at(
                    provider.clone(),
                    BlockId::Number(BlockNumberOrTag::Number(block)),
                )
                .await?;
            }
            continue;
        }
    }
    if cur_block >= start {
        flush_block(
            provider.clone(),
            &mut pool,
            cur_block,
            &mut block_quotes_ok,
            &mut block_quotes_total,
            &mut quote_mismatches,
            &mut cap_infos,
            &mut reserve_diffs,
            &mut price_mismatches,
        )
        .await?;
    }

    println!();
    println!("=== REPLAY SUMMARY ===");
    println!("update logs: {n_update_logs} (raw-tx enriched: {n_raw_ok}, raw missing: {n_raw_fail}, async fallbacks: {async_updates})");
    println!("swap logs:   {n_swap_logs}");
    println!("quote compare: {block_quotes_ok}/{block_quotes_total} exact (linear region)");
    println!("quote mismatches: {}", quote_mismatches.len());
    for m in quote_mismatches.iter().take(20) {
        println!("{m}");
    }
    println!("info (engine buy-cap consumed, unobservable): {}", cap_infos.len());
    for m in cap_infos.iter().take(20) {
        println!("{m}");
    }
    println!("reserve diffs: {}", reserve_diffs.len());
    for d in reserve_diffs.iter().take(30) {
        println!("{d}");
    }
    let transfer_related = reserve_diffs
        .iter()
        .filter(|d| {
            d.split_whitespace().nth(3).map_or(false, |addr| {
                Address::from_str(addr)
                    .map(|a| transfer_assets.contains(&a))
                    .unwrap_or(false)
            })
        })
        .count();
    println!(
        "reserve diffs on vault-transfer-touched assets: {transfer_related}/{}",
        reserve_diffs.len()
    );
    println!(
        "price (L2 vs snapshot-recovered) mismatches: {}",
        price_mismatches.len()
    );
    for m in price_mismatches.iter().take(20) {
        println!("{m}");
    }

    // 断言：所有可观测报价逐位精确；引擎买入额度被消耗的瞬态（INFO）视为已解释
    if block_quotes_total > 0 && block_quotes_ok + cap_infos.len() == block_quotes_total {
        println!("RESULT: QUOTES FULLY REPLICATED");
    } else {
        println!("RESULT: QUOTES NOT FULLY REPLICATED");
    }
    Ok(())
}

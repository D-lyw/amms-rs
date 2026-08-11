//! Caliber propAMM — 周期对账批量快照真实链验证 probe（生产 RPC 验证）。
//!
//! 验证周期对账用的 JSON-RPC 批量 `eth_getStorageAt`
//! （`CaliberPropPool::batch_refresh_snapshots`，即
//! `start_caliber_prop_ladder_sync_task` 内部调用的函数）与逐槽
//! `eth_getStorageAt` 在同一区块上完全一致：
//!
//! 1. 固定一个区块号 N（取最新块）；
//! 2. `getAllPairIds` 取少量 pair → 手动构造 pool 骨架；
//! 3. 直接调用 `batch_refresh_snapshots`（周期对账批量路径）；
//! 4. 对每个 pool 用逐槽 `eth_getStorageAt`（节流 + 429 重试）读取
//!    cfg/data/ladder 槽位，
//!    按本地相同解析逻辑推导 reserve/ladder/fee/window/field0/field1/pos/deadline，
//!    逐字段对比；任何不匹配即退出码 1。
//!
//! 用法:
//! ```bash
//! cargo run -p amms --release --example caliber_prop_batch_probe
//! ```
//!
//! 环境变量:
//! - `XLAYER_RPC`  XLayer HTTP RPC（默认生产 `https://rpc.xlayer.tech`）

use alloy::{
    eips::BlockId,
    primitives::{address, keccak256, Address, B256, U256},
    providers::{Provider, ProviderBuilder},
};
use amms::amms::{
    amm::AMM,
    caliber_prop::{batch_refresh_snapshots, CaliberPropPool, ICaliberPropAMM},
    Token,
};
use eyre::Result;
use std::env;
use std::time::Duration;

const XLAYER_CHAIN_ID: u64 = 196;
const CALIBER_CONTRACT: Address = address!("154586b2479b9a11e3d4db90024dc0e26f097312");
/// 公共 RPC 对连续调用限流较严，probe 只抽查前若干个 pair
const PROBE_MAX_PAIRS: usize = 6;
/// 逐槽参考读取之间的间隔（避免公共 RPC 429）
const PACE_MS: u64 = 60;

// ============================================================================
// 逐槽参考读取（槽位布局与 src/amms/caliber_prop/mod.rs 完全一致）
// ============================================================================

fn pair_slot(pair_id: B256, index: u64) -> B256 {
    let mut input = [0u8; 64];
    input[..32].copy_from_slice(pair_id.as_ref());
    input[32..].copy_from_slice(&U256::from(index).to_be_bytes::<32>());
    B256::from(keccak256(input))
}

fn b256_add(base: B256, add: u64) -> B256 {
    B256::from((U256::from_be_bytes(base.0) + U256::from(add)).to_be_bytes::<32>())
}

/// 读取 pair 的 token_x/token_y（cfg 基址 +0/+1 槽，取低 20 字节）
async fn read_token_addresses<P: Provider>(
    provider: &P,
    contract: Address,
    pair_id: B256,
    block: BlockId,
) -> Result<(Address, Address)> {
    let cfg_base = pair_slot(pair_id, 6);
    let raw0 = slot_at(provider, contract, cfg_base, block).await?;
    let raw1 = slot_at(provider, contract, b256_add(cfg_base, 1), block).await?;
    let mut addr0 = [0u8; 20];
    addr0.copy_from_slice(&raw0.to_be_bytes::<32>()[12..]);
    let mut addr1 = [0u8; 20];
    addr1.copy_from_slice(&raw1.to_be_bytes::<32>()[12..]);
    Ok((Address::from(addr0), Address::from(addr1)))
}

async fn slot_at<P: Provider>(
    provider: &P,
    address: Address,
    slot: B256,
    block: BlockId,
) -> Result<U256> {
    // 公共 RPC 限流（HTTP 429）时退避重试；其余错误直接上抛（保留可读错误信息）
    let mut attempt = 0usize;
    loop {
        match provider
            .get_storage_at(address, U256::from_be_bytes(slot.0))
            .block_id(block)
            .await
        {
            Ok(v) => {
                tokio::time::sleep(Duration::from_millis(PACE_MS)).await;
                return Ok(v);
            }
            Err(e) if attempt < 5 => {
                eprintln!(
                    "[probe] getStorageAt failed (attempt {attempt}, slot {slot}): {e}; retrying"
                );
                tokio::time::sleep(Duration::from_millis(500 * (attempt as u64 + 1))).await;
                attempt += 1;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn pow10(exp: u8) -> U256 {
    let mut r = U256::from(1u64);
    for _ in 0..exp {
        r *= U256::from(10u64);
    }
    r
}

struct RefSnapshot {
    reserve_a: U256,
    reserve_b: U256,
    ladder: Vec<(U256, U256)>,
    fee: U256,
    window: U256,
    scale: U256,
    pos_reverse: U256,
    pos_forward: U256,
    deadline: u64,
    field0: U256,
    field1: U256,
}

async fn reference_snapshot<P: Provider>(
    provider: &P,
    contract: Address,
    pair_id: B256,
    token_x: Address,
    token_y: Address,
    block: BlockId,
) -> Result<RefSnapshot> {
    let cfg_base = pair_slot(pair_id, 6);
    let data_base = pair_slot(pair_id, 7);

    let cfg1 = slot_at(provider, contract, b256_add(cfg_base, 1), block).await?;
    let n = slot_at(provider, contract, b256_add(cfg_base, 2), block).await?;
    let window = slot_at(provider, contract, b256_add(cfg_base, 3), block).await?;
    let reserve_x = slot_at(provider, contract, b256_add(cfg_base, 4), block).await?;
    let reserve_y = slot_at(provider, contract, b256_add(cfg_base, 5), block).await?;
    let cfg6 = slot_at(provider, contract, b256_add(cfg_base, 6), block).await?;
    let cfg7 = slot_at(provider, contract, b256_add(cfg_base, 7), block).await?;
    let data0 = slot_at(provider, contract, data_base, block).await?;
    let validity = slot_at(
        provider,
        contract,
        B256::from(U256::from(2u64).to_be_bytes::<32>()),
        block,
    )
    .await?
        & U256::from(u64::MAX);
    let paused_raw = slot_at(
        provider,
        contract,
        B256::from(U256::from(3u64).to_be_bytes::<32>()),
        block,
    )
    .await?;

    let block_info = provider
        .get_block(block)
        .await?
        .ok_or_else(|| eyre::eyre!("block not found"))?;
    let block_ts = U256::from(block_info.header.timestamp);
    let cur_block = U256::from(block_info.header.number);

    let n_usize: usize = n.to::<usize>();
    if n_usize == 0 || n_usize > 1024 {
        eyre::bail!("invalid ladder length {n_usize}");
    }

    // decimals / scale
    let dec_x = ((cfg1 >> U256::from(0xa0)) & U256::from(0xff)).to::<u8>();
    let dec_y = ((cfg1 >> U256::from(0xa8)) & U256::from(0xff)).to::<u8>();
    let scale = pow10(dec_x) / pow10(dec_y);

    // fee / field0 / field1
    let fee = cfg6 & U256::from(u64::MAX);
    let field0 = data0 & U256::from(u64::MAX);
    let field1 = (data0 >> U256::from(64)) & U256::from(u32::MAX);
    let deadline = ((data0 >> U256::from(96)) & U256::from(u32::MAX)).to::<u64>();

    // 有效 pos（cfg+7 = [block:32][0:64][mid96:96][low96:96]）：
    // 反向读 mid96、正向读 low96，各自仅当 block == 当前块时有效
    let pos_block = cfg7 >> U256::from(192);
    let pos_mask = (U256::from(1) << U256::from(96)) - U256::from(1);
    let pos_reverse = if pos_block == cur_block {
        (cfg7 >> U256::from(96)) & pos_mask
    } else {
        U256::ZERO
    };
    let pos_forward = if pos_block == cur_block {
        cfg7 & pos_mask
    } else {
        U256::ZERO
    };

    // 过期/暂停
    let paused = !(paused_raw & U256::from(0xff)).is_zero()
        || !((cfg6 >> U256::from(0x40)) & U256::from(0xff)).is_zero();
    let ts_xy = (((data0 >> U256::from(128)) & U256::from(u32::MAX)) << U256::from(32))
        | ((data0 >> U256::from(96)) & U256::from(u32::MAX));
    let expired = block_ts > ts_xy + validity;
    let stale = paused || expired;

    // ladder
    let mut ladder = Vec::new();
    if !stale {
        let ladder_base =
            keccak256((U256::from_be_bytes(cfg_base.0) + U256::from(2)).to_be_bytes::<32>());
        for i in 0..n_usize {
            let raw = slot_at(provider, contract, b256_add(ladder_base, i as u64), block).await?;
            ladder.push((raw >> U256::from(128), raw & U256::from(u128::MAX)));
        }
    }

    // 映射到 token_a/token_b 视角
    let (reserve_a, reserve_b) = if token_x < token_y {
        (reserve_x, reserve_y)
    } else {
        (reserve_y, reserve_x)
    };

    Ok(RefSnapshot {
        reserve_a,
        reserve_b,
        ladder,
        fee,
        window,
        scale,
        pos_reverse,
        pos_forward,
        deadline,
        field0,
        field1,
    })
}

fn compare_pool(pool: &CaliberPropPool, r: &RefSnapshot) -> (bool, Vec<String>) {
    let mut diffs = Vec::new();
    if pool.reserve_a != r.reserve_a {
        diffs.push(format!(
            "reserve_a: batch={} ref={}",
            pool.reserve_a, r.reserve_a
        ));
    }
    if pool.reserve_b != r.reserve_b {
        diffs.push(format!(
            "reserve_b: batch={} ref={}",
            pool.reserve_b, r.reserve_b
        ));
    }
    if pool.ladder.ladder_a_to_b.len() != r.ladder.len() {
        diffs.push(format!(
            "ladder len: batch={} ref={}",
            pool.ladder.ladder_a_to_b.len(),
            r.ladder.len()
        ));
    } else {
        for (i, (p, (amount_in, amount_out))) in pool
            .ladder
            .ladder_a_to_b
            .iter()
            .zip(r.ladder.iter())
            .enumerate()
        {
            if p.amount_in != *amount_in || p.amount_out != *amount_out {
                diffs.push(format!(
                    "ladder[{i}]: batch=({},{}) ref=({},{})",
                    p.amount_in, p.amount_out, amount_in, amount_out
                ));
            }
        }
    }
    if pool.ladder.ladder_b_to_a.len() != r.ladder.len() {
        diffs.push("ladder_b_to_a len mismatch".to_string());
    }
    if pool.ladder.fee_rate != r.fee {
        diffs.push(format!("fee: batch={} ref={}", pool.ladder.fee_rate, r.fee));
    }
    if pool.ladder.window != r.window {
        diffs.push(format!(
            "window: batch={} ref={}",
            pool.ladder.window, r.window
        ));
    }
    if pool.ladder.scale != r.scale {
        diffs.push(format!(
            "scale: batch={} ref={}",
            pool.ladder.scale, r.scale
        ));
    }
    if pool.ladder.pos_reverse != r.pos_reverse {
        diffs.push(format!(
            "pos_reverse: batch={} ref={}",
            pool.ladder.pos_reverse, r.pos_reverse
        ));
    }
    if pool.ladder.pos_forward != r.pos_forward {
        diffs.push(format!(
            "pos_forward: batch={} ref={}",
            pool.ladder.pos_forward, r.pos_forward
        ));
    }
    if pool.ladder.deadline != r.deadline {
        diffs.push(format!(
            "deadline: batch={} ref={}",
            pool.ladder.deadline, r.deadline
        ));
    }
    if pool.ladder.field0 != r.field0 {
        diffs.push(format!(
            "field0: batch={} ref={}",
            pool.ladder.field0, r.field0
        ));
    }
    if pool.ladder.field1 != r.field1 {
        diffs.push(format!(
            "field1: batch={} ref={}",
            pool.ladder.field1, r.field1
        ));
    }
    (diffs.is_empty(), diffs)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let rpc_url = env::var("XLAYER_RPC").unwrap_or_else(|_| "https://rpc.xlayer.tech".to_string());
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    let latest = provider.get_block_number().await?;
    let block = BlockId::number(latest);
    println!("[probe] pinned block: {latest}");

    // 1. getAllPairIds 取少量 pair，手动构造 pool 骨架（避免 discover 全量 80+ 次连续 RPC）
    let caliber = ICaliberPropAMM::new(CALIBER_CONTRACT, provider.clone());
    let all_ids = caliber
        .getAllPairIds(U256::ZERO, U256::from(100u64))
        .block(block)
        .call()
        .await?;
    let total_pairs = all_ids.len();
    let pair_ids: Vec<B256> = all_ids.into_iter().take(PROBE_MAX_PAIRS).collect();
    println!(
        "[probe] got {total_pairs} pairs (checking first {})",
        pair_ids.len()
    );

    let mut skeletons: Vec<AMM> = Vec::with_capacity(pair_ids.len());
    for pair_id in pair_ids {
        let (token_x, token_y) =
            read_token_addresses(&provider, CALIBER_CONTRACT, pair_id, block).await?;
        let (token_a_addr, token_b_addr) = if token_x < token_y {
            (token_x, token_y)
        } else {
            (token_y, token_x)
        };
        let virtual_address =
            CaliberPropPool::virtual_address_from_pair_id(pair_id, CALIBER_CONTRACT);
        skeletons.push(AMM::CaliberPropPool(CaliberPropPool {
            contract_address: CALIBER_CONTRACT,
            pair_id,
            virtual_address,
            token_x,
            token_y,
            created_block: 0,
            last_synced_block: 0,
            token_a: Token {
                address: token_a_addr,
                decimals: 0,
                symbol: String::new(),
                chain_id: XLAYER_CHAIN_ID,
                fot_tax: None,
            },
            token_b: Token {
                address: token_b_addr,
                decimals: 0,
                symbol: String::new(),
                chain_id: XLAYER_CHAIN_ID,
                fot_tax: None,
            },
            reserve_a: U256::ZERO,
            reserve_b: U256::ZERO,
            ladder: Default::default(),
            price_a_in_b: 0.0,
            price_b_in_a: 0.0,
        }));
    }

    // 2. 周期对账批量路径：直接调用 start_caliber_prop_ladder_sync_task 内部
    //    使用的 batch_refresh_snapshots
    let mut pools: Vec<CaliberPropPool> = skeletons
        .into_iter()
        .filter_map(|a| match a {
            AMM::CaliberPropPool(p) => Some(p),
            _ => None,
        })
        .collect();
    let flags =
        batch_refresh_snapshots::<alloy::network::Ethereum, _>(&provider, &mut pools, block)
            .await?;
    let failed = flags.iter().filter(|f| !**f).count();
    if pools.is_empty() {
        eyre::bail!("batch_refresh_snapshots returned 0 pools (batch path failed)");
    }
    println!(
        "[probe] batch_refresh_snapshots refreshed {}/{} pools (failed={failed})",
        pools.len() - failed,
        pools.len()
    );
    if failed > 0 {
        // 具体失败原因由 batch_refresh_snapshots 内的 tracing::error! 输出
        eyre::bail!("{failed} pools failed to refresh in batch path");
    }

    // 3. 逐槽参考读取 + 逐字段对比
    let mut checked = 0usize;
    let mut mismatched = 0usize;
    for pool in &pools {
        let r = reference_snapshot(
            &provider,
            pool.contract_address,
            pool.pair_id,
            pool.token_x,
            pool.token_y,
            block,
        )
        .await?;
        let (ok, diffs) = compare_pool(pool, &r);
        checked += 1;
        if !ok {
            mismatched += 1;
            println!("[probe] MISMATCH pool={}", pool.virtual_address);
            for d in diffs {
                println!("        {d}");
            }
        }
    }

    println!("[probe] checked={checked} pools, mismatches={mismatched} (block {latest})");
    if mismatched > 0 {
        eyre::bail!("{mismatched}/{checked} pools mismatched");
    }
    println!("[probe] OK: batch snapshot == per-slot eth_getStorageAt");
    Ok(())
}

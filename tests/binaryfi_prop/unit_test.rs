//! BinaryFi propAMM 单元测试（报价公式 / L2 calldata 解析 / 三层同步）。
//!
//! 由 `src/amms/binaryfi_prop/mod.rs` 的 `#[cfg(test)] mod tests` 迁移而来，
//! 通过 `tests/binaryfi_prop.rs` 入口编译运行。

use alloy::hex;
use alloy::primitives::Log as AlloyLog;
use alloy::primitives::{address, keccak256, Address, Bytes, LogData, B256, U256};
use alloy::rpc::types::Log;

use amms::amms::amm::{AutomatedMarketMaker, SyncAction};
use amms::amms::binaryfi_prop::*;
use amms::amms::Token;

fn address_topic(addr: Address) -> B256 {
    addr.into_word()
}

fn asset_topic(idx: usize) -> B256 {
    B256::from(U256::from(idx))
}

fn update_asset_topic() -> B256 {
    asset_topic(1)
}

fn rlp_item(bytes: &[u8]) -> Vec<u8> {
    if bytes.len() == 1 && bytes[0] < 0x80 {
        return vec![bytes[0]];
    }
    if bytes.len() <= 55 {
        let mut out = vec![0x80 + bytes.len() as u8];
        out.extend_from_slice(bytes);
        out
    } else if bytes.len() <= 0xff {
        let mut out = vec![0xb8, bytes.len() as u8];
        out.extend_from_slice(bytes);
        out
    } else {
        let mut out = vec![0xb9, (bytes.len() >> 8) as u8, bytes.len() as u8];
        out.extend_from_slice(bytes);
        out
    }
}

fn rlp_list(items: &[Vec<u8>]) -> Vec<u8> {
    let payload: Vec<u8> = items.iter().flatten().copied().collect();
    if payload.len() <= 55 {
        let mut out = vec![0xc0 + payload.len() as u8];
        out.extend_from_slice(&payload);
        out
    } else if payload.len() <= 0xff {
        let mut out = vec![0xf8, payload.len() as u8];
        out.extend_from_slice(&payload);
        out
    } else {
        let mut out = vec![0xf9, (payload.len() >> 8) as u8, payload.len() as u8];
        out.extend_from_slice(&payload);
        out
    }
}

/// 构造一条真实的 legacy update 交易（calldata 来自链上样本）
fn real_update_tx() -> Vec<u8> {
    // update(0x0b, 0x60, 0x400c944, 15005(0x3a9d), 2, 1, data96, sig65)
    let mut calldata = vec![0x02, 0x4b, 0x94, 0xf6];
    // index = 0x0b
    let mut index = [0u8; 32];
    index[31] = 0x0b;
    // offset = 0x60
    let mut offset = [0u8; 32];
    offset[31] = 0x60;
    // blockNumber = 0x400c944
    let mut block_number = [0u8; 32];
    block_number[28] = 0x04;
    block_number[30] = 0xc9;
    block_number[31] = 0x44;
    // price = 15005 = 0x3a9d
    let mut price = [0u8; 32];
    price[30] = 0x3a;
    price[31] = 0x9d;
    // a = 2, b = 1
    let mut a = [0u8; 32];
    a[31] = 2;
    let mut b = [0u8; 32];
    b[31] = 1;
    calldata.extend_from_slice(&index);
    calldata.extend_from_slice(&offset);
    calldata.extend_from_slice(&block_number);
    calldata.extend_from_slice(&price);
    calldata.extend_from_slice(&a);
    calldata.extend_from_slice(&b);
    // data0 = sellLadder(0x0040a0004024) / data1 = buyLadder(0x00403d00)，
    // 左对齐；data2 = 0
    let mut data0 = [0u8; 32];
    data0[..6].copy_from_slice(&[0x00, 0x40, 0xa0, 0x00, 0x40, 0x24]);
    let mut data1 = [0u8; 32];
    data1[..4].copy_from_slice(&[0x00, 0x40, 0x3d, 0x00]);
    let data2 = [0u8; 32];
    calldata.extend_from_slice(&data0);
    calldata.extend_from_slice(&data1);
    calldata.extend_from_slice(&data2);
    // data_len = 0x60 / sig_len = 0x41 / sig(96B)
    let mut data_len = [0u8; 32];
    data_len[31] = 0x60;
    let mut sig_len = [0u8; 32];
    sig_len[31] = 0x41;
    let sig = [0x22u8; 96];
    calldata.extend_from_slice(&data_len);
    calldata.extend_from_slice(&sig_len);
    calldata.extend_from_slice(&sig);
    assert_eq!(calldata.len(), BINARYFI_UPDATE_CALLDATA_LEN);

    // legacy tx: nonce=1, gasPrice=1, gas=0x5208, to=engine, value=0, data, v=0x1c, r=1, s=1
    let to: [u8; 20] = BINARYFI_ENGINE_ADDRESS.into_array();
    let tx = rlp_list(&[
        rlp_item(&[1]),
        rlp_item(&[1]),
        rlp_item(&[0x52, 0x08]),
        rlp_item(&to),
        rlp_item(&[]),
        rlp_item(&calldata),
        rlp_item(&[0x1c]),
        rlp_item(&[1]),
        rlp_item(&[1]),
    ]);
    tx
}

/// 构造一条真实 legacy 批量 update 交易（`0x34f7f748`，2026-08-19 引擎升级后格式）。
/// calldata 含 asset 1（xETH）与 asset 3（xSOL）两条记录，价格/ladder 来自
/// 链上样本（块 69328955，tx 0xe25b47…）。
fn real_batch_update_tx() -> Vec<u8> {
    let mut calldata = vec![0x34, 0xf7, 0xf7, 0x48];
    let w = |v: u64| -> [u8; 32] {
        let mut b = [0u8; 32];
        b[24..].copy_from_slice(&v.to_be_bytes());
        b
    };
    // head 6 words：dataOffset=0xc0 / sigOffset=0x920 / timestamp / blockNumber=0x421e03b / 0 / 0xaa0
    calldata.extend_from_slice(&w(0xc0));
    calldata.extend_from_slice(&w(0x920));
    calldata.extend_from_slice(&w(0x6a9435ca));
    calldata.extend_from_slice(&w(0x421e03b));
    calldata.extend_from_slice(&w(0));
    calldata.extend_from_slice(&w(0xaa0));
    calldata.extend_from_slice(&w(2)); // count = 2
                                       // record = (asset_idx, price, askOff, bidOff, data0, data1)
    let record = |idx: u64, price: u64, askf: u64, bidf: u64, d0hi: u16, d1hi: u16| {
        let mut b = Vec::new();
        b.extend_from_slice(&w(idx));
        b.extend_from_slice(&w(price));
        b.extend_from_slice(&w(askf));
        b.extend_from_slice(&w(bidf));
        let mut d0 = [0u8; 32];
        d0[..2].copy_from_slice(&d0hi.to_be_bytes());
        b.extend_from_slice(&d0);
        let mut d1 = [0u8; 32];
        d1[..2].copy_from_slice(&d1hi.to_be_bytes());
        b.extend_from_slice(&d1);
        b
    };
    // asset 1（xETH）：price=247334，d0hi=0x0327、d1hi=0x032f → raw 50/50
    calldata.extend_from_slice(&record(1, 247334, 5, 3, 0x0327, 0x032f));
    // asset 3（xSOL）：price=10706，d0hi=0x0035、d1hi=0x0048 → raw 3/4
    calldata.extend_from_slice(&record(3, 10706, 2, 1, 0x0035, 0x0048));
    // sig 区（96B 占位）
    calldata.extend_from_slice(&[0u8; 96]);

    let to: [u8; 20] = BINARYFI_ENGINE_ADDRESS.into_array();
    rlp_list(&[
        rlp_item(&[1]),
        rlp_item(&[1]),
        rlp_item(&[0x52, 0x08]),
        rlp_item(&to),
        rlp_item(&[]),
        rlp_item(&calldata),
        rlp_item(&[0x1c]),
        rlp_item(&[1]),
        rlp_item(&[1]),
    ])
}

#[test]
fn test_enrich_batch_update_log_data() {
    let raw = real_batch_update_tx();
    let tx_hash = keccak256(&raw);
    let mk_log = |idx: usize| {
        LogData::new(vec![BINARYFI_UPDATE_EVENT, asset_topic(idx)], Bytes::new()).unwrap()
    };

    // asset 3（xSOL）：price=10706，blockNumber=0x421e03b，raw 点差 4/3
    let enriched = enrich_update_log_data(
        &[hex::encode(&raw)],
        Some(tx_hash),
        &mk_log(3),
        BINARYFI_ENGINE_ADDRESS,
    )
    .expect("batch enrich should succeed");
    let data = enriched.data.as_ref();
    assert_eq!(data.len(), 224);
    assert_eq!(U256::from_be_slice(&data[..32]), U256::from(10706u64));
    assert_eq!(U256::from_be_slice(&data[32..64]).to::<u64>(), 0x421e03b);
    assert_eq!(&data[64..66], &[0x00, 0x35]); // data0 高 16 位
    assert_eq!(&data[96..98], &[0x00, 0x48]); // data1 高 16 位
    assert_eq!(U256::from_be_slice(&data[160..192]).to::<u64>(), 4); // ask_raw
    assert_eq!(U256::from_be_slice(&data[192..224]).to::<u64>(), 3); // bid_raw
    assert_eq!(enriched.topics(), &[BINARYFI_UPDATE_EVENT, asset_topic(3)]);

    // asset 1（xETH）：price=247334 → raw 50/50
    let enriched1 = enrich_update_log_data(
        &[hex::encode(&raw)],
        Some(tx_hash),
        &mk_log(1),
        BINARYFI_ENGINE_ADDRESS,
    )
    .expect("batch enrich asset1 should succeed");
    let data1 = enriched1.data.as_ref();
    assert_eq!(U256::from_be_slice(&data1[..32]), U256::from(247334u64));
    assert_eq!(U256::from_be_slice(&data1[160..192]).to::<u64>(), 50);
    assert_eq!(U256::from_be_slice(&data1[192..224]).to::<u64>(), 50);

    // 未知资产索引 → None（保留原始日志走 L3 兜底）
    assert!(enrich_update_log_data(
        &[hex::encode(&raw)],
        Some(tx_hash),
        &mk_log(99),
        BINARYFI_ENGINE_ADDRESS
    )
    .is_none());
}

/// 批量 update 记录的 L2 应用：注入值喂入 `apply_l2_update_full` 后，0→xSOL /
/// xSOL→0（in=1e6，fee=400）报价与链上块 69328955 逐位一致
/// （9,333,333 / 106,987），即幻影套利方向的本地价格与链上对齐。
#[test]
fn test_batch_update_l2_quote_matches_chain() {
    let mut pool = BinaryFiPropPool::default();
    pool.assets = vec![
        Token::new_with_decimals(address!("0x779ded0c9e1022225f8e0630b35a9b54be713736"), 6),
        Token::new_with_decimals(address!("0xe7b000003a45145decf8a28fc755ad5ec5ea025a"), 18),
        Token::new_with_decimals(address!("0x68fa48b1c2fe52b3d776e1953e0e782b5044ce28"), 8),
        Token::new_with_decimals(address!("0x505000008de8748dbd4422ff4687a4fc9beba15b"), 9),
    ];
    let n = pool.assets.len();
    pool.prices = vec![U256::ZERO; n];
    pool.spreads = vec![0; n];
    pool.bid_offsets = vec![0; n];
    pool.ask_offsets = vec![0; n];
    pool.q0j = vec![None; n];
    pool.sell_raw = vec![None; n];
    pool.buy_zero_over_vault = vec![false; n];
    pool.max_outputs = vec![None; n];
    pool.max_inputs = vec![None; n];
    pool.reserves = vec![U256::ZERO; n];
    pool.rates = vec![Rate::zero(); n * n];
    pool.price_updated_block = vec![0; n];
    pool.sell_ladders = vec![None; n];
    pool.buy_ladders = vec![None; n];
    pool.ladder_reserves = vec![None; n];
    pool.buy_ladder_remaining = vec![None; n];
    pool.prices[0] = U256::from(BINARYFI_PRICE0_DEFAULT);
    pool.fee_ppm = 400;

    // 链上批量 tx（块 69328955）asset 3 记录：price=10706、d0hi=0x0035、
    // d1hi=0x0048 → ask_raw=4、bid_raw=3（scale=10000 → 实际偏移 4/3）
    let mut d0 = [0u8; 32];
    d0[..2].copy_from_slice(&[0x00, 0x35]);
    let mut d1 = [0u8; 32];
    d1[..2].copy_from_slice(&[0x00, 0x48]);
    pool.apply_l2_update_full(
        3,
        U256::from(10706u64),
        0x421e03b,
        4,
        3,
        U256::from_be_slice(&d0),
        U256::from_be_slice(&d1),
    );
    assert_eq!(pool.ask_price(3).unwrap(), U256::from(10710u64));
    assert_eq!(pool.bid_price(3).unwrap(), U256::from(10703u64));
    // 链上 0→3 in=1e6（fee=400）：9,333,333
    assert_eq!(
        pool.engine_quote(0, 3, U256::from(1_000_000u64)).unwrap(),
        U256::from(9_333_333u64)
    );
    // 链上 3→0 in=1e6：106,987（线性回退 raw=10703，fee 输入侧扣）
    assert_eq!(
        pool.engine_quote(3, 0, U256::from(1_000_000u64)).unwrap(),
        U256::from(106_987u64)
    );
}

#[test]
fn test_enrich_update_log_data() {
    let raw = real_update_tx();
    let tx_hash = keccak256(&raw);
    let topics = vec![BINARYFI_UPDATE_EVENT, update_asset_topic()];
    let log_data = LogData::new(topics, Bytes::new()).unwrap();

    let enriched = enrich_update_log_data(
        &[hex::encode(&raw)],
        Some(tx_hash),
        &log_data,
        BINARYFI_ENGINE_ADDRESS,
    )
    .expect("enrich should succeed");
    let data = enriched.data.as_ref();
    assert_eq!(data.len(), 224);
    let price = U256::from_be_slice(&data[..32]);
    let block_number = U256::from_be_slice(&data[32..64]);
    assert_eq!(price, U256::from(15005));
    assert_eq!(block_number.to::<u64>(), 0x400c944);
    // data0 = sellLadder(0x0040a0004024)，data1 = buyLadder(0x00403d00)
    assert_eq!(&data[64..70], &[0x00, 0x40, 0xa0, 0x00, 0x40, 0x24]);
    assert_eq!(&data[96..100], &[0x00, 0x40, 0x3d, 0x00]);
    // 点差偏移字段：ladder 前 16 位 = 0x0040（64），raw = 64/16 = 4
    let ask_raw = U256::from_be_slice(&data[160..192]);
    let bid_raw = U256::from_be_slice(&data[192..224]);
    assert_eq!(ask_raw.to::<u64>(), 4);
    assert_eq!(bid_raw.to::<u64>(), 4);
    assert_eq!(
        enriched.topics(),
        &[BINARYFI_UPDATE_EVENT, update_asset_topic()]
    );
}

/// L2 完整应用：price + ladder 点差（无费口径，fee=1000 时报价输入侧扣费）：
///   - SKHYx（index 1）：price=13984，ask 字段=3、sell 字段=3 →
///     bid = price − sell_off = 13981；ask=13987
///   - asset2（index 2，scale=100000）：price=640774，raw 252/252 →
///     ask_off=2520、sell_off=2520 → bid = 6,405,220
#[test]
fn test_apply_l2_update_spread_and_scale() {
    let mut pool = test_pool();
    // SKHYx（index 1，dj=18，scale=10000 默认）
    pool.apply_l2_update(1, U256::from(13_984u64), 100, 3, 3);
    assert_eq!(pool.prices[1], U256::from(13_984u64));
    assert_eq!(pool.bid_price(1).unwrap(), U256::from(13_981u64));
    assert_eq!(pool.ask_price(1).unwrap(), U256::from(13_987u64));
    // q0j = floor(1e20/13987)（无费）；链上 0→SKHYx in=1e6 报价 = fee_rem 后逐位对拍
    assert_eq!(
        pool.q0j[1],
        Some(U256::from_str_radix("7149495960534782", 10).unwrap())
    );
    let out = pool.engine_quote(0, 1, U256::from(1_000_000u64)).unwrap();
    assert_eq!(out, U256::from_str_radix("7142346464574247", 10).unwrap());
    // SELL：SKHYx→0 in=1e15 → 139,670（链上逐位对拍）
    assert_eq!(
        pool.engine_quote(1, 0, U256::from(1_000_000_000_000_000u64))
            .unwrap(),
        U256::from(139_670u64)
    );

    // asset2：scale=100000，raw 252 → 实际 2520
    pool.assets.push(Token::new_with_decimals(
        address!("0xb7C00000bcDEeF966b20B3D884B98E64d2b06b4f"),
        8,
    ));
    let n = pool.assets.len();
    pool.prices.push(U256::ZERO);
    pool.spreads.push(0);
    pool.bid_offsets.push(0);
    pool.ask_offsets.push(0);
    pool.q0j.push(None);
    pool.price_scales = vec![10_000; n];
    pool.price_scales[2] = 100_000;
    pool.rates = vec![Rate::zero(); n * n];
    pool.price_updated_block.push(0);
    pool.apply_l2_update(2, U256::from(640_774u64), 100, 252, 252);
    // 内部价格 = 640,774 × 100000/10000 = 6,407,740
    assert_eq!(pool.prices[2], U256::from(6_407_740u64));
    assert_eq!(pool.spreads[2], 5040);
    assert_eq!(pool.ask_price(2).unwrap(), U256::from(6_410_260u64));
    assert_eq!(pool.bid_price(2).unwrap(), U256::from(6_405_220u64));
    // q0j = floor(10^10/6,410,260) = 1559（dj=8，无费）
    assert_eq!(pool.q0j[2], Some(U256::from(1559u64)));
}

fn test_enrich_wrong_tx_hash_returns_none() {
    let raw = real_update_tx();
    let topics = vec![BINARYFI_UPDATE_EVENT, update_asset_topic()];
    let log_data = LogData::new(topics, Bytes::new()).unwrap();
    let bogus = B256::repeat_byte(0xff);
    assert!(enrich_update_log_data(
        &[hex::encode(&raw)],
        Some(bogus),
        &log_data,
        BINARYFI_ENGINE_ADDRESS
    )
    .is_none());
    assert!(enrich_update_log_data(
        &[] as &[&str],
        Some(keccak256(&raw)),
        &log_data,
        BINARYFI_ENGINE_ADDRESS
    )
    .is_none());
}

fn test_pool() -> BinaryFiPropPool {
    let mut pool = BinaryFiPropPool::default();
    pool.assets = vec![
        Token::new_with_decimals(address!("0x779ded0c9e1022225f8e0630b35a9b54be713736"), 6),
        Token::new_with_decimals(address!("0x58100046a4afcd4ee4fadbd4244f3f895a341c56"), 18),
    ];
    let n = pool.assets.len();
    pool.prices = vec![U256::ZERO; n];
    pool.spreads = vec![0; n];
    pool.bid_offsets = vec![0; n];
    pool.ask_offsets = vec![0; n];
    pool.q0j = vec![None; n];
    pool.sell_raw = vec![None; n];
    pool.buy_zero_over_vault = vec![false; n];
    pool.max_outputs = vec![None; n];
    pool.max_inputs = vec![None; n];
    pool.reserves = vec![U256::ZERO; n];
    pool.rates = vec![Rate::zero(); n * n];
    pool.price_updated_block = vec![0; n];
    pool.prices[0] = U256::from(BINARYFI_PRICE0_DEFAULT);
    pool.reserves[0] = U256::from(350_000_000u64);
    pool.reserves[1] = U256::from(8_000_000_000_000_000_000u64);
    pool
}

/// 3 资产实例（USDT0 + 2 个可交易资产），用于跨 pair 共享金库测试。
fn test_pool_3() -> BinaryFiPropPool {
    let mut pool = test_pool();
    pool.assets.push(Token::new_with_decimals(
        address!("0x68fa48b1c2fe52b3d776e1953e0e782b5044ce28"),
        18,
    ));
    let n = pool.assets.len();
    pool.prices.push(U256::ZERO);
    pool.spreads.push(0);
    pool.bid_offsets.push(0);
    pool.ask_offsets.push(0);
    pool.q0j.push(None);
    pool.sell_raw.push(None);
    pool.buy_zero_over_vault.push(false);
    pool.max_outputs.push(None);
    pool.max_inputs.push(None);
    pool.reserves.push(U256::from(9_000_000_000_000_000_000u64));
    pool.rates.resize(n * n, Rate::zero());
    pool.price_updated_block.push(0);
    // test_pool() 未初始化阶梯/容量向量（Default 为空），按 3 资产补齐
    pool.sell_ladders = vec![None; n];
    pool.buy_ladders = vec![None; n];
    pool.ladder_reserves = vec![None; n];
    pool.buy_ladder_remaining = vec![None; n];
    pool
}

/// 共享金库账本（L1）：跨 pair Swap 只更新金库余额、不锚定本实例价格
/// （2026-08-19 事故根因：其他 pair 的 SELL 抽干 USDT0 金库，本实例不可见 →
/// 本地金库门控放行必然失败的交易）。Swap 涉及非本 exposed pair 资产时：
///  - reserves 按部署级共享金库口径增减（in += / out -=），跨 pair 抽干全局可见；
///  - 费率/价格不因跨 pair 成交被污染（rates 保持原值）；
///  - 本 pair 的 BUY 阶梯容量不因他人 pair 成交被消耗。
#[test]
fn test_sync_cross_pair_swap_updates_shared_vault_only() {
    let mut pool = test_pool_3();
    // 实例暴露 pair = (0,1)；资产 2 不在本 pair
    pool.exposed_pair = Some((0, 1));
    // 本 pair BUY 阶梯容量（asset1）
    pool.buy_ladder_remaining[1] = Some(U256::from(62_000_000u64));
    // 预置 0→2 费率，验证跨 pair 成交不覆盖价格
    pool.set_rate(0, 2, U256::from(1_000_000u64), U256::from(1_000_000u64));

    let amount_in = U256::from(400_000_000u64);
    let amount_out = U256::from(53_100_000u64);
    let mut data = amount_in.to_be_bytes::<32>().to_vec();
    data.extend_from_slice(&amount_out.to_be_bytes::<32>());
    let log_data = LogData::new(
        vec![
            BINARYFI_SWAP_EVENT,
            address_topic(BINARYFI_ROUTER_ADDRESS),
            address_topic(pool.assets[0].address), // USDT0 in
            address_topic(pool.assets[2].address), // asset2 out（跨 pair）
        ],
        Bytes::from(data),
    )
    .unwrap();
    let log = Log {
        inner: AlloyLog {
            address: BINARYFI_POOL_ADDRESS,
            data: log_data,
        },
        ..Default::default()
    };

    let action = pool.sync(&log).expect("sync ok");
    assert!(matches!(action, SyncAction::None));

    // 共享金库账本：reserves[0] += in、reserves[2] -= out（跨 pair 抽干全局可见）
    assert_eq!(pool.reserves[0], U256::from(350_000_000u64) + amount_in);
    assert_eq!(
        pool.reserves[2],
        U256::from(9_000_000_000_000_000_000u64) - amount_out
    );
    // 本 pair 资产 1 的金库/容量不受他人 pair 成交影响
    assert_eq!(pool.reserves[1], U256::from(8_000_000_000_000_000_000u64));
    assert_eq!(
        pool.buy_ladder_remaining[1],
        Some(U256::from(62_000_000u64))
    );
    // 跨 pair 成交不污染本实例费率（0→2 不在 exposed pair，rates 保持原值）
    let r = pool.rates[pool.pair_index(0, 2)];
    assert_eq!(r.num, U256::from(1_000_000u64));
    assert_eq!(r.den, U256::from(1_000_000u64));
}

#[test]
fn test_swap_event_anchors_rate_and_reserves() {
    let mut pool = test_pool();
    let in_amt = U256::from(35_357_671u64);
    let out_amt = U256::from_str_radix("235576460790192551", 10).unwrap();
    pool.anchor_rate(0, 1, in_amt, out_amt);

    let rate = pool.rates[pool.pair_index(0, 1)];
    assert_eq!(rate.num, out_amt);
    assert_eq!(rate.den, in_amt);
    assert_eq!(pool.reserves[0], U256::from(350_000_000u64) + in_amt);
    assert_eq!(
        pool.reserves[1],
        U256::from(8_000_000_000_000_000_000u64) - out_amt
    );

    // price0 标定：implied = out * pj * 10^d0 / (in * 10^dj)
    pool.prices[1] = U256::from(15005);
    let mut pool2 = test_pool();
    pool2.prices[1] = U256::from(15005);
    pool2.anchor_rate(0, 1, in_amt, out_amt);
    assert!(pool2.price0_calibrated);
    let expected = out_amt * U256::from(15005) * U256::from(10u64.pow(6))
        / (in_amt * U256::from(10u64.pow(18)));
    // 99.97 → 99（整数除法）
    assert_eq!(pool2.prices[0], U256::from(99));
    assert_eq!(pool2.prices[0], expected);
}

#[test]
fn test_price_update_is_idempotent_set() {
    let mut pool = test_pool();
    pool.apply_l2_update(1, U256::from(13_984u64), 100, 3, 3);
    // 0→SKHYx：rate = q0j/10^6 × (1e6−fee)/1e6 = q0j×999000 / 1e12
    let rate = pool.rates[pool.pair_index(0, 1)];
    assert_eq!(
        rate.num,
        U256::from_str_radix("7142346464574247218000", 10).unwrap()
    );
    assert_eq!(rate.den, U256::from(1_000_000_000_000u64));
    // SKHYx→0：raw = 13984−3 = 13,981（无费），
    // rate = raw/10^14 × (1e6−fee)/1e6 = 13,981×999,000 / 1e20
    let rate_ba = pool.rates[pool.pair_index(1, 0)];
    assert_eq!(rate_ba.num, U256::from(13_967_019_000u64));
    assert_eq!(rate_ba.den, U256::from(10u64).pow(U256::from(20)));

    // 重复同 price 更新：费率不变（幂等）
    pool.apply_l2_update(1, U256::from(13_984u64), 101, 3, 3);
    assert_eq!(pool.rates[pool.pair_index(0, 1)], rate);
    assert_eq!(pool.price_updated_block[1], 101);
}

#[test]
fn test_simulate_swap_matches_onchain_sample() {
    let mut pool = test_pool();
    // 新引擎锚点：SKHYx price=13984/askOff=3/sellOff=3（链上逐位对拍）
    pool.apply_l2_update(1, U256::from(13_984u64), 100, 3, 3);
    // BUY 小额：0→SKHYx in=1e6 → q0j = 7,142,346,464,574,247
    let out = pool
        .simulate_swap(
            pool.assets[0].address,
            pool.assets[1].address,
            U256::from(1_000_000u64),
        )
        .unwrap();
    assert_eq!(out, U256::from_str_radix("7142346464574247", 10).unwrap());
    // SELL：SKHYx→0 in=1e18 → 139,670,190
    // （输入侧扣费：rem=in−in/1000 × bid=13,981；fee=1000 链上逐位一致）
    let sell = pool
        .simulate_swap(
            pool.assets[1].address,
            pool.assets[0].address,
            U256::from(1_000_000_000_000_000_000u64),
        )
        .unwrap();
    assert_eq!(sell, U256::from(139_670_190u64));

    // 大额 linear > 金库余额 → 实时金库零门槛归零（链上实测：linear 超金库即 0）
    let huge = pool
        .simulate_swap(
            pool.assets[0].address,
            pool.assets[1].address,
            U256::from(1_000_000_000_000u64),
        )
        .unwrap();
    assert_eq!(huge, U256::ZERO);
}

/// SELL 阶梯上限：先扣费再按 maxIn 截断消耗量（consume = min(rem, maxIn)）。
///   - price=15005/askOff=4/sellOff=4 → bid=15001（无费）
///   - in=1e18（< maxIn）：149,859,990（线性，输入侧扣费）
///   - in=1e20 / 1e24（> maxIn）：3,684,065,588（饱和 = maxIn×bid×1e-14，fee 无关）
#[test]
fn test_engine_quote_sell_cap_matches_onchain() {
    let mut pool = test_pool();
    pool.apply_l2_update(1, U256::from(15_005u64), 100, 4, 4);
    // maxIn = 196 × 125,300,000,000,000,000
    pool.max_inputs[1] = Some(U256::from(196u64) * U256::from(125_300_000_000_000_000u64));
    assert_eq!(
        pool.max_inputs[1],
        Some(U256::from_str_radix("24558800000000000000", 10).unwrap())
    );

    // USDT0 金库余额调大，避免超容量归零（本测试只验证 maxIn 饱和截断）
    pool.reserves[0] = U256::from(10u64.pow(15));

    // 线性区（in=1e18 < maxIn）：out = fee_rem(in)×raw×1e4/1e18
    // raw = 15005−4 = 15,001 → out(1e18) = 149,859,990
    let out = pool
        .simulate_swap(
            pool.assets[1].address,
            pool.assets[0].address,
            U256::from(10u64.pow(18)),
        )
        .unwrap();
    assert_eq!(out, U256::from(149_859_990u64));

    // 饱和区：in=1e20 与 1e24 均为 consume=min(rem,maxIn)=maxIn → maxIn×raw×1e4/1e18
    let out = pool
        .simulate_swap(
            pool.assets[1].address,
            pool.assets[0].address,
            U256::from(10u128.pow(20)),
        )
        .unwrap();
    assert_eq!(out, U256::from(3_684_065_588u64));
    let out = pool
        .simulate_swap(
            pool.assets[1].address,
            pool.assets[0].address,
            U256::from(10u128.pow(24)),
        )
        .unwrap();
    assert_eq!(out, U256::from(3_684_065_588u64));
}

/// P0 回归：BUY 费率先按 rem = in − in/1000 整数扣减再线性报价（非最后乘 999/1000）。
/// 两笔生产失败套利交易的 binaryFI 池子 quote 逐位对拍：
///   - 67475114 asset9 price=13649/askOff=4，in=44,291,018 → 324,080,619,644,034,278
///   - 67476139 asset9 price=13684/askOff=5，in=122,156,425 → 891,476,871,940,974,505
#[test]
fn test_engine_quote_buy_rem_fee_matches_failed_tx_samples() {
    let mut pool = test_pool();
    pool.apply_l2_update(1, U256::from(13_649u64), 67_475_114, 4, 4);
    let out = pool.engine_quote(0, 1, U256::from(44_291_018u64)).unwrap();
    assert_eq!(out, U256::from_str_radix("324080619644034278", 10).unwrap());

    pool.apply_l2_update(1, U256::from(13_684u64), 67_476_139, 5, 4);
    let out = pool.engine_quote(0, 1, U256::from(122_156_425u64)).unwrap();
    assert_eq!(out, U256::from_str_radix("891476871940974505", 10).unwrap());
}

/// BUY 阶梯上限：线性区 = q0j（in=1e6），饱和区 min(linear, maxOut)
#[test]
fn test_engine_quote_buy_cap_matches_onchain() {
    let mut pool = test_pool();
    pool.apply_l2_update(1, U256::from(13_984u64), 100, 3, 3);
    pool.max_outputs[1] = Some(U256::from_str_radix("7643300000000000000", 10).unwrap());

    let out = pool
        .simulate_swap(
            pool.assets[0].address,
            pool.assets[1].address,
            U256::from(1_000_000u64),
        )
        .unwrap();
    assert_eq!(out, U256::from_str_radix("7142346464574247", 10).unwrap());

    for amt in [U256::from(1_000_000_000_000u64), U256::from(10u128.pow(24))] {
        let out = pool
            .simulate_swap(pool.assets[0].address, pool.assets[1].address, amt)
            .unwrap();
        // 饱和型 maxOut ≤ 金库：min(linear, maxOut) 正常返回（锚点块实测）
        assert_eq!(
            out,
            U256::from_str_radix("7643300000000000000", 10).unwrap()
        );
    }
}

/// BUY 超阈值归零型（阶梯容量 > 金库余额）：linear ≤ 金库余额才返回，否则 0。
/// 构造样例：xSOL dj=9，price=7376/askOff=3/sellOff=3 → ask=7379、q0j=13,538,419
#[test]
fn test_engine_quote_buy_zero_over_vault() {
    let mut pool = test_pool();
    pool.assets.push(Token::new_with_decimals(
        address!("0x505000008DE8748DBd4422ff4687a4FC9bEba15b"),
        9,
    ));
    let n = pool.assets.len();
    pool.prices.push(U256::ZERO);
    pool.spreads.push(0);
    pool.bid_offsets.push(0);
    pool.ask_offsets.push(0);
    pool.q0j.push(None);
    pool.sell_raw.push(None);
    pool.rates = vec![Rate::zero(); n * n];
    pool.price_updated_block.push(0);
    pool.buy_zero_over_vault.push(true);
    pool.max_outputs.push(None);
    pool.max_inputs.push(None);
    pool.reserves.push(U256::from(160_992_934u64));
    pool.apply_l2_update(2, U256::from(7_376u64), 100, 3, 3);
    // BUY(1e6) = q0j = 13,538,419
    assert_eq!(
        pool.engine_quote(0, 2, U256::from(1_000_000u64)).unwrap(),
        U256::from(13_538_419u64)
    );
    // 线性区：in=11,870,000 → 160,701,043 ≤ 金库 → 返回
    assert_eq!(
        pool.engine_quote(0, 2, U256::from(11_870_000u64)).unwrap(),
        U256::from(160_701_043u64)
    );
    // 线性区边界：in=11,880,000 → 160,836,427 ≤ 金库 → 返回
    assert_eq!(
        pool.engine_quote(0, 2, U256::from(11_880_000u64)).unwrap(),
        U256::from(160_836_427u64)
    );
    // 超阈值归零：in=11,900,000 → 161,107,196 > 金库 → 0
    assert_eq!(
        pool.engine_quote(0, 2, U256::from(11_900_000u64)).unwrap(),
        U256::ZERO
    );
    // 跨资产两段式：SKHYx(1) → xSOL(2), in=5e16（第二段不含 999/1000 因子）
    pool.apply_l2_update(1, U256::from(13_984u64), 100, 3, 3);
    // v = floor(fee_rem(5e16) × 13,981 × 1e4 / 1e18) = 6,983,509
    // linear = floor(6,983,509 × 1e5 / 7379) = 94,640,317 ≤ 金库 → 返回
    assert_eq!(
        pool.engine_quote(1, 2, U256::from(50_000_000_000_000_000u64))
            .unwrap(),
        U256::from(94_640_317u64)
    );
    // in=1e17：v = 13,967,019 → linear = 189,280,647 > 金库 → 0
    assert_eq!(
        pool.engine_quote(1, 2, U256::from(100_000_000_000_000_000u64))
            .unwrap(),
        U256::ZERO
    );
}

#[test]
fn test_engine_quote_cross_two_stage() {
    let mut pool = test_pool();
    pool.assets.push(Token::new_with_decimals(
        address!("0x8aD3c73F833d3F9A523aB01476625F269aEB7Cf0"),
        18,
    ));
    let n = pool.assets.len();
    pool.prices.push(U256::ZERO);
    pool.spreads.push(0);
    pool.bid_offsets.push(0);
    pool.ask_offsets.push(0);
    pool.q0j.push(None);
    pool.sell_raw.push(None);
    pool.rates = vec![Rate::zero(); n * n];
    pool.price_updated_block.push(0);
    // asset4(2)：price=32481/askOff=18/sellOff=9 → ask=32499、raw4=32,472（无费）
    pool.apply_l2_update(2, U256::from(32_481u64), 100, 18, 9);
    pool.apply_l2_update(1, U256::from(13_984u64), 100, 3, 3);
    // SKHYx(1) -> asset4(2), in=1e18：两段式（第一段输入侧扣费，第二段无额外因子）
    // v = floor(fee_rem(1e18) × 13,981 × 1e4 / 1e18) = 139,670,190
    // out = floor(139,670,190 × 1e14 / 32499) = 429,767,654,389,365,826
    let out = pool
        .engine_quote(1, 2, U256::from(10u64.pow(18)))
        .expect("engine quote");
    assert_eq!(out, U256::from_str_radix("429767654389365826", 10).unwrap());
    // asset4(2) -> SKHYx(1)：raw4 = 32481−9 = 32,472（无费）
    // v = floor(fee_rem(1e18) × 32,472 × 1e4 / 1e18) = 324,395,280
    // out = floor(324,395,280 × 1e14 / 13987) = 2,319,262,743,976,549,653
    let out = pool
        .engine_quote(2, 1, U256::from(10u64.pow(18)))
        .expect("engine quote");
    assert_eq!(
        out,
        U256::from_str_radix("2319262743976549653", 10).unwrap()
    );
}

#[test]
fn test_exact_out_rounds_up() {
    let mut pool = test_pool();
    let idx = pool.pair_index(0, 1);
    pool.rates[idx] = Rate {
        num: U256::from(3),
        den: U256::from(2),
    };
    let need = pool
        .simulate_swap_exact_out(
            pool.assets[0].address,
            pool.assets[1].address,
            U256::from(5),
        )
        .unwrap();
    // ceil(5 * 2 / 3) = ceil(3.33) = 4
    assert_eq!(need, U256::from(4));
}

#[test]
fn test_mark_and_clear_stale() {
    let mut pool = test_pool();
    pool.mark_stale_for_asset(1);
    assert_eq!(pool.stale_pairs.len(), 2);
    let pairs = pool.stale_pairs.clone();
    pool.clear_stale_pairs(&pairs);
    assert!(pool.stale_pairs.is_empty());

    pool.mark_stale_for_asset(1);
    pool.apply_price_update(1, U256::from(15005), 1);
    assert!(pool.stale_pairs.is_empty());
}

#[test]
fn test_sync_swap_log_anchors() {
    let mut pool = test_pool();
    let amount_in = U256::from(35_357_671u64);
    let amount_out = U256::from_str_radix("235576460790192551", 10).unwrap();

    let mut data = amount_in.to_be_bytes::<32>().to_vec();
    data.extend_from_slice(&amount_out.to_be_bytes::<32>());
    let log_data = LogData::new(
        vec![
            BINARYFI_SWAP_EVENT,
            address_topic(BINARYFI_ROUTER_ADDRESS),
            address_topic(pool.assets[0].address),
            address_topic(pool.assets[1].address),
        ],
        Bytes::from(data),
    )
    .unwrap();
    let log = Log {
        inner: AlloyLog {
            address: BINARYFI_POOL_ADDRESS,
            data: log_data,
        },
        ..Default::default()
    };

    let action = pool.sync(&log).expect("sync ok");
    assert!(matches!(action, SyncAction::None));
    let rate = pool.rates[pool.pair_index(0, 1)];
    assert_eq!(rate.num, amount_out);
    assert_eq!(rate.den, amount_in);
    assert_eq!(pool.reserves[0], U256::from(350_000_000u64) + amount_in);
    assert_eq!(
        pool.reserves[1],
        U256::from(8_000_000_000_000_000_000u64) - amount_out
    );
}

#[test]
fn test_sync_fee_event_updates_fee_and_rederives_rates() {
    let mut pool = test_pool();
    // 先建立无费价格（L2 语义）：默认 fee=1000 → rates 含 fee 折算
    pool.apply_l2_update(1, U256::from(13_984u64), 100, 3, 3);
    assert_eq!(pool.fee_ppm, BINARYFI_DEFAULT_FEE_PPM);
    let rate_old = pool.rates[pool.pair_index(0, 1)];
    assert!(rate_old.num > U256::ZERO);

    // 引擎 FeeUpdated：单 topic，data 前 32 字节 = 新 fee ppm（200）
    let data = U256::from(200u64).to_be_bytes::<32>().to_vec();
    let log_data = LogData::new(vec![BINARYFI_FEE_EVENT], Bytes::from(data)).unwrap();
    let log = Log {
        inner: AlloyLog {
            address: BINARYFI_ENGINE_ADDRESS,
            data: log_data.clone(),
        },
        ..Default::default()
    };

    let action = pool.sync(&log).expect("sync ok");
    assert!(matches!(action, SyncAction::AsyncUpdate));
    assert_eq!(pool.fee_ppm, 200);
    // rates 同步重导：fee 1000→200 → 可执行中间价上移（0→1 方向）
    let rate_new = pool.rates[pool.pair_index(0, 1)];
    assert!(
        rate_new.num > rate_old.num,
        "rate should rise after fee cut"
    );
    assert_eq!(rate_new.den, rate_old.den);

    // 重复同 fee 事件 → 无动作
    let action2 = pool.sync(&log).expect("sync ok");
    assert!(matches!(action2, SyncAction::None));

    // 事件地址不是 engine（池子地址 + fee topic）→ 不进 fee 分支（默认 Resync）
    let log_other = Log {
        inner: AlloyLog {
            address: BINARYFI_POOL_ADDRESS,
            data: log_data.clone(),
        },
        ..Default::default()
    };
    let action3 = pool.sync(&log_other).expect("sync ok");
    assert!(matches!(action3, SyncAction::Resync));
}

#[test]
fn test_sync_enriched_update_applies_price() {
    let mut pool = test_pool();
    // 增强后的 data = price / blockNumber / data0..2 / askOffsetRaw / bidOffsetRaw
    // （7 个 word；data0..2=0 → 点差偏移 0 → 默认 spread 8）
    let mut data = Vec::new();
    data.extend_from_slice(&U256::from(15005u64).to_be_bytes::<32>());
    data.extend_from_slice(&U256::from(0x400c944u64).to_be_bytes::<32>());
    data.extend_from_slice(&[0u8; 96]);
    data.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
    data.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());

    let log_data = LogData::new(
        vec![BINARYFI_UPDATE_EVENT, asset_topic(1)],
        Bytes::from(data),
    )
    .unwrap();
    let log = Log {
        inner: AlloyLog {
            address: BINARYFI_ENGINE_ADDRESS,
            data: log_data,
        },
        ..Default::default()
    };

    let action = pool.sync(&log).expect("sync ok");
    assert!(matches!(action, SyncAction::None));
    assert_eq!(pool.prices[1], U256::from(15005));
    assert_eq!(pool.price_updated_block[1], 0x400c944);
    // 点差偏移 0/0 → raw = 15005（无费），
    // rate(1→0) = raw/10^14 × (1e6−fee)/1e6 = 15,005×999,000/1e20
    let rate_ba = pool.rates[pool.pair_index(1, 0)];
    assert_eq!(rate_ba.num, U256::from(14_989_995_000u64));
    assert_eq!(rate_ba.den, U256::from(10u64).pow(U256::from(20)));
    // BUY：q0j = floor(1e20/15005)（无费），rate(0→1) = q0j×999,000/1e12
    let rate = pool.rates[pool.pair_index(0, 1)];
    assert_eq!(
        rate.num,
        U256::from_str_radix("6657780739753414647000", 10).unwrap()
    );
    assert_eq!(rate.den, U256::from(1_000_000_000_000u64));
}

/// AsyncUpdate 快照写回的核心安全前提：L2 日志已把价格推进到块 N 后，旧块快照
/// （snap_block < N）写回不得回退 L2 价格（`apply_snapshot` 的 log_fresh 保鲜）。
/// 这是"BinaryFi AsyncUpdate 放宽 last_synced_block 竞态丢弃、直接写回"的正确性
/// 依据——快照只补事件拿不到的 quote/bid/容量/费率，价格以日志为准。
#[test]
fn test_apply_snapshot_does_not_regress_fresh_log_price() {
    let mut pool = test_pool();
    // L2 日志：asset1 价格 15005 落到块 0x400c944，price_updated_block 推进
    pool.apply_l2_update_full(
        1,
        U256::from(15005u64),
        0x400c944,
        0,
        0,
        U256::ZERO,
        U256::ZERO,
    );
    assert_eq!(pool.prices[1], U256::from(15005));
    assert_eq!(pool.price_updated_block[1], 0x400c944);

    // 旧块快照（snap_block < 日志块）：quote 观测给出不同 bid/q0j
    let ok = |v: &str| QuoteResult {
        amountOut: U256::from_str_radix(v, 10).unwrap(),
        success: true,
    };
    let snap = Snapshot {
        assets: pool.assets.iter().map(|t| t.address).collect(),
        decimals: vec![6u8, 18u8],
        scales: vec![U256::ZERO, U256::from(10_000u64)],
        poolBalances: vec![U256::ZERO, U256::ZERO],
        vaultReserves: vec![
            U256::from(10u64).pow(U256::from(30)),
            U256::from(10u64).pow(U256::from(30)),
        ],
        vaultBalances: Vec::new(),
        quotePairs: vec![U256::from(1), U256::from(2)],
        quotes: vec![ok("1000000000000000"), ok("14850")], // 0→1 small / 1→0 small
        fee: U256::from(1000),
    };
    pool.apply_snapshot(&snap, 0x400c944 - 1);

    // log_fresh：日志价格 >= 快照块 → 快照不覆盖 L2 价格/ask/q0j
    assert_eq!(pool.prices[1], U256::from(15005), "L2 价格不得被旧快照回退");
    assert_eq!(pool.price_updated_block[1], 0x400c944);
    // 容量/禁用状态仍以快照 quote 观测为准（事件拿不到的权威数据）
    assert!(!pool.buy_disabled[1]);
}

#[test]
fn test_sync_canonical_update_marks_stale() {
    let mut pool = test_pool();
    let log_data = LogData::new(vec![BINARYFI_UPDATE_EVENT, asset_topic(1)], Bytes::new()).unwrap();
    let log = Log {
        inner: AlloyLog {
            address: BINARYFI_ENGINE_ADDRESS,
            data: log_data,
        },
        ..Default::default()
    };

    let action = pool.sync(&log).expect("sync ok");
    assert!(matches!(action, SyncAction::AsyncUpdate));
    assert_eq!(pool.stale_pairs.len(), 2);
    assert!(pool.stale_pairs.contains(&pool.pair_index(0, 1)));
    assert!(pool.stale_pairs.contains(&pool.pair_index(1, 0)));
}

/// canonical（无 raw bytes）update 事件：价格由 AsyncUpdate 快照补缺，但时效时钟
/// 必须用事件块号推进——链上引擎每次 update 交易都写 per-asset lastUpdateBlock
/// （即使内容相同也刷新，实测 67430645/47 两笔相同 NVDAx update）。若不推进，
/// 时效门控在 canonical 路径整体失效，会用快照价格算出链上已过期=0 的幻影利润。
#[test]
fn test_sync_canonical_update_advances_freshness_clock() {
    let mut pool = test_pool();
    let log_data = LogData::new(vec![BINARYFI_UPDATE_EVENT, asset_topic(1)], Bytes::new()).unwrap();
    let log = Log {
        inner: AlloyLog {
            address: BINARYFI_ENGINE_ADDRESS,
            data: log_data,
        },
        block_number: Some(100),
        ..Default::default()
    };

    let action = pool.sync(&log).expect("sync ok");
    assert!(matches!(action, SyncAction::AsyncUpdate));
    assert_eq!(pool.price_updated_block[1], 100);

    // diff=5 仍新鲜（引擎 quote 需先有价格：快照路径经 apply_price_update 锚定）
    pool.apply_l2_update(1, U256::from(13_984u64), 100, 3, 3);
    pool.last_synced_block = 105;
    let fresh = pool.engine_quote(0, 1, U256::from(1_000_000u64)).unwrap();
    assert!(!fresh.is_zero());

    // diff=6 过期返回 0（与链上一致：NVDAx lastUpdate=67430638 时 67430644 归零）
    pool.last_synced_block = 106;
    assert_eq!(
        pool.engine_quote(0, 1, U256::from(1_000_000u64)).unwrap(),
        U256::ZERO
    );

    // 无块号的 canonical 日志（历史回放/缺块号）不推进时钟：price_updated_block
    // 保持 100，避免把旧日志误判成新时效。
    let log_no_blk = Log {
        inner: AlloyLog {
            address: BINARYFI_ENGINE_ADDRESS,
            data: LogData::new(vec![BINARYFI_UPDATE_EVENT, asset_topic(1)], Bytes::new()).unwrap(),
        },
        ..Default::default()
    };
    pool.sync(&log_no_blk).expect("sync ok");
    assert_eq!(pool.price_updated_block[1], 100);
}

/// BUY 低小数位资产：dj=4 < d0-2 时 q0j = floor(10^(dj+2)×999/(1000×ask)) 很小，
/// BUY 报价 = floor(in × q0j / 10^d0) 仍精确。
#[test]
fn test_engine_quote_buy_low_decimals_asset() {
    let mut pool = test_pool();
    pool.assets[1] =
        Token::new_with_decimals(address!("0x58100046a4afcd4ee4fadbd4244f3f895a341c56"), 4);
    // ask = 15005 + 4 = 15009 → q0j = floor(1e6×999/(1000×15009)) = 66
    pool.apply_l2_update(1, U256::from(15_005u64), 100, 4, 4);
    let out = pool
        .engine_quote(0, 1, U256::from(1_000_000u64))
        .expect("quote");
    assert_eq!(out, U256::from(66u64));
}

/// exact_out 必须遵守精确 cap（maxOut / maxIn），不能用 96% 金库兜底高估合法输出。
#[test]
fn test_exact_out_respects_ladder_caps() {
    let mut pool = test_pool();
    pool.apply_l2_update(1, U256::from(15_005u64), 100, 4, 4);

    // BUY 饱和型：maxOut=1000（远小于 96% 金库）→ 超过即拒绝
    pool.max_outputs[1] = Some(U256::from(1000u64));
    assert!(pool
        .simulate_swap_exact_out(
            pool.assets[0].address,
            pool.assets[1].address,
            U256::from(5000u64),
        )
        .is_err());
    let in_needed = pool
        .simulate_swap_exact_out(
            pool.assets[0].address,
            pool.assets[1].address,
            U256::from(500u64),
        )
        .unwrap();
    assert!(in_needed > U256::ZERO);

    // SELL：maxIn=1 整枚 SKHYx → 可达输出上限 = maxIn×bid×1e-14
    pool.max_outputs[1] = None;
    pool.max_inputs[1] = Some(U256::from(1_000_000_000_000_000_000u64));
    // 超过 maxIn 线性可达输出 → 拒绝
    assert!(pool
        .simulate_swap_exact_out(
            pool.assets[1].address,
            pool.assets[0].address,
            U256::from(200_000_000u64),
        )
        .is_err());
    let in_needed = pool
        .simulate_swap_exact_out(
            pool.assets[1].address,
            pool.assets[0].address,
            U256::from(100_000_000u64),
        )
        .unwrap();
    assert!(in_needed > U256::ZERO);
}

/// L2 多档 SELL 阶梯（真实链上 asset6 update calldata，block 0x402fdb5）：
/// data0 sellLadder = 0x01808801c08801d088094ba8...（权重未折算，scale=10000）
/// 解码 [(24,136),(28,136),(29,136),(148,2984)]；buyLadder = 0x01806001a08509f2c6...
/// 解码 [(24,96),(26,133),(159,710)]。引擎储备 R=29.4e15（链上反推）时
/// `ladder_sell_out` 逐档累加输出与链上逐位一致。
#[test]
fn test_ladder_sell_tiers_asset6() {
    let mut pool = test_pool();
    pool.assets.push(Token::new_with_decimals(
        address!("0xe7b000003a45145decf8a28fc755ad5ec5ea025a"),
        18,
    ));
    let n = pool.assets.len();
    pool.prices.push(U256::ZERO);
    pool.spreads.push(0);
    pool.bid_offsets.push(0);
    pool.ask_offsets.push(0);
    pool.q0j.push(None);
    pool.sell_raw.push(None);
    pool.price_scales.push(10_000);
    pool.buy_disabled.push(false);
    pool.buy_zero_over_vault.push(false);
    pool.max_outputs.push(None);
    pool.max_inputs.push(None);
    pool.reserves.push(U256::ZERO);
    pool.rates.resize(n * n, Rate::zero());
    pool.stale_pairs.resize(n * n, 0);
    pool.price_updated_block.push(0);
    pool.sell_ladders.resize(n, None);
    pool.buy_ladders.resize(n, None);
    pool.ladder_reserves.resize(n, None);
    // 金库余额放大，避免 sell_zero_over_vault 干扰阶梯断言
    pool.reserves[0] = U256::from(10_000_000_000_000_000u64);

    // 链上 update calldata 的 data0/data1（左对齐 256 位阶梯字段）
    let data0 = U256::from_str_radix(
        "01808801c08801d088094ba80000000000000000000000000000000000000000",
        16,
    )
    .unwrap();
    let data1 = U256::from_str_radix(
        "01806001a08509f2c60000000000000000000000000000000000000000000000",
        16,
    )
    .unwrap();
    pool.apply_l2_update_full(2, U256::from(76_925u64), 0x402fdb5, 4, 3, data0, data1);

    // 阶梯解码（weight 未折算，scale=10000）
    assert_eq!(
        pool.sell_ladders[2],
        Some(vec![(24, 136), (28, 136), (29, 136), (148, 2984)])
    );
    assert_eq!(
        pool.buy_ladders[2],
        Some(vec![(24, 96), (26, 133), (159, 710)])
    );

    // 引擎储备 R = 29.4e15（链上反推），SELL 逐档输出与链上一致
    pool.ladder_reserves[2] = Some(U256::from(29_400_000_000_000_000u64));
    let cases: &[(u64, u64)] = &[
        (1_000_000_000_000_00, 76_824),               // 1e14，首档内
        (10_000_000_000_000_000, 7_682_409),          // 1e16
        (1_000_000_000_000_000_000, 768_240_990),     // 1e18
        (5_000_000_000_000_000_000, 3_841_165_086),   // 5e18，跨 2 档
        (10_000_000_000_000_000_000, 7_682_150_304),  // 1e19，跨 3 档
        (18_000_000_000_000_000_000, 13_820_554_332), // 1.8e19，跨 4 档
    ];
    for &(inp, expected) in cases {
        let out = pool
            .engine_quote(2, 0, U256::from(inp))
            .expect("sell quote");
        assert_eq!(out, U256::from(expected), "in={inp}");
    }

    // ladder + R 未知时回退单档线性（bid=76925-42=76883 → in×76883×1e-14）
    let mut fallback = test_pool();
    fallback.assets.push(Token::new_with_decimals(
        address!("0xe7b000003a45145decf8a28fc755ad5ec5ea025a"),
        18,
    ));
    let n = fallback.assets.len();
    fallback.prices.push(U256::ZERO);
    fallback.spreads.push(0);
    fallback.bid_offsets.push(0);
    fallback.ask_offsets.push(0);
    fallback.q0j.push(None);
    fallback.sell_raw.push(None);
    fallback.price_scales.push(10_000);
    fallback.buy_disabled.push(false);
    fallback.buy_zero_over_vault.push(false);
    fallback.max_outputs.push(None);
    fallback.max_inputs.push(None);
    fallback.reserves.push(U256::ZERO);
    fallback.rates.resize(n * n, Rate::zero());
    fallback.stale_pairs.resize(n * n, 0);
    fallback.price_updated_block.push(0);
    fallback.sell_ladders.push(None);
    fallback.buy_ladders.push(None);
    fallback.ladder_reserves.push(None);
    fallback.reserves[0] = U256::from(10_000_000_000_000_000u64);
    fallback.apply_l2_update(2, U256::from(76_925u64), 0x402fdb5, 4, 3);
    // raw = 76925×999 - 3×1000 = 76,845,075；out = in×raw×1e4/(1000×1e18)
    let out = fallback
        .engine_quote(2, 0, U256::from(1_000_000_000_000_000u64))
        .expect("sell quote fallback");
    assert_eq!(out, U256::from(768_450u64));
}

/// P0 回归：饱和型 BUY 受实时金库零门槛约束 —— maxOut 来自快照时刻 probe，
/// 金库被 Swap 抽干后（NVDAx 实测 vault≈1.07e12 近空）链上 quote 恒 0，
/// 本地不得继续按快照 maxOut 报价制造幻影利润。
#[test]
fn test_buy_capped_saturating_vault_depleted_zeroes() {
    let mut pool = test_pool();
    pool.apply_l2_update(1, U256::from(13_984u64), 100, 3, 3);
    // 饱和型 maxOut 已知（快照时刻值）
    pool.max_outputs[1] = Some(U256::from_str_radix("1301449609759457280", 10).unwrap());
    // 健康金库：in=1e9 → linear≈7.146e18 > maxOut → 饱和截断到 maxOut
    pool.reserves[1] = U256::from_str_radix("21087064247644425879", 10).unwrap();
    let out = pool
        .engine_quote(0, 1, U256::from(1_000_000_000u64))
        .unwrap();
    assert_eq!(
        out,
        U256::from_str_radix("1301449609759457280", 10).unwrap()
    );
    // 金库近空（Swap 抽干、maxOut 未刷新）：链上归零，本地必须归零
    pool.reserves[1] = U256::from(1_070_000_000_000u64);
    let out = pool
        .engine_quote(0, 1, U256::from(1_000_000_000u64))
        .unwrap();
    assert_eq!(out, U256::ZERO);
    // 归零型行为不变：linear ≤ 金库才返回（不叠加 min(maxOut)）
    pool.buy_zero_over_vault[1] = true;
    pool.reserves[1] = U256::from_str_radix("21087064247644425879", 10).unwrap();
    let out = pool
        .engine_quote(0, 1, U256::from(1_000_000_000u64))
        .unwrap();
    let linear = U256::from(1_000_000_000u64)
        .checked_mul(U256::from(10u64).pow(U256::from(20)))
        .unwrap()
        .checked_mul(U256::from(999))
        .unwrap()
        / (U256::from(1000u64) * U256::from(13_987u64) * U256::from(10u64).pow(U256::from(6)));
    assert_eq!(out, linear);
}

/// P1 回归：非单调阶梯退化（NVDAx 块 67430640 实测）—— big probe（1e10）
/// 落在退化平顶区 1.301e18，而 mid probe（1e9）仍线性 4.456e18 > big；
/// 快照不得把 big 当 maxOut 截断线性区（否则 in=867,053,194 本地 1.301e18
/// vs 链上 3.863e18，低估 66%）。
#[test]
fn test_apply_snapshot_degenerate_maxout_cleared() {
    let mut pool = BinaryFiPropPool::default();
    let usdt0 = address!("0x779ded0c9e1022225f8e0630b35a9b54be713736");
    let nvdx = address!("0xc845b2894dbddd03858fd2d643b4ef725fe0849d");
    let ok = |v: &str| QuoteResult {
        amountOut: U256::from_str_radix(v, 10).unwrap(),
        success: true,
    };
    let snap = Snapshot {
        assets: vec![usdt0, nvdx],
        decimals: vec![6u8, 18u8],
        scales: vec![U256::ZERO, U256::from(10_000u64)],
        poolBalances: vec![U256::ZERO, U256::ZERO],
        vaultReserves: vec![
            U256::from(11_281_542_985u64),
            U256::from_str_radix("21087064247644425879", 10).unwrap(),
        ],
        vaultBalances: Vec::new(),
        // n=2：small 0→1=1、1→0=2；big(1e10)=n²+1=5；bigsell(1e20)=2n²+1=9；
        // mid(1e9)=3n²+1=13
        quotePairs: vec![
            U256::from(1),
            U256::from(2),
            U256::from(5),
            U256::from(9),
            U256::from(13),
        ],
        quotes: vec![
            ok("4455644262075732"),    // 0→1 small：q0j
            ok("22384"),               // 1→0 small：bid
            ok("1301449609759457280"), // 0→1 big：退化平顶
            ok("10916672400"),         // 1→0 100 整枚
            ok("4456402343463560796"), // 0→1 mid：线性 4.456e18
        ],
        fee: U256::from(1000), // 失败交易窗口费率为 1000ppm
    };
    pool.apply_snapshot(&snap, 67_430_640);
    // 退化检测：mid(4.456e18) > big(1.301e18) → maxOut 清除（不得截断线性区）
    assert_eq!(pool.max_outputs[1], None);
    assert!(!pool.buy_zero_over_vault[1]);
    // 线性区恢复：in=867,053,194（失败交易实际输入）→ ≈3.863e18，非 1.301e18
    let out = pool.engine_quote(0, 1, U256::from(867_053_194u64)).unwrap();
    let plateau = U256::from_str_radix("1301449609759457280", 10).unwrap();
    assert!(out > plateau, "linear region must not be capped: {out}");
    let chain = U256::from_str_radix("3863280589625797243", 10).unwrap();
    let diff = if out > chain {
        out - chain
    } else {
        chain - out
    };
    assert!(
        diff < U256::from(100_000_000_000_000u64),
        "linear drift too large: sim={out} chain={chain}"
    );
    // 金库抽干 → 实时零门槛归零
    pool.reserves[1] = U256::from(1_070_000_000_000u64);
    let out = pool.engine_quote(0, 1, U256::from(867_053_194u64)).unwrap();
    assert_eq!(out, U256::ZERO);
}

/// P1 回归：多档 SELL 引擎储备 R 单调二分推导 —— 100 整枚 probe 未饱和时
/// （总容量 > 输入）闭式公式失效，二分求解仍能恢复 R，ladder 路径逐位复刻
/// probe；不再回退单档线性高估。
#[test]
fn test_sell_ladder_r_derived_when_probe_unsaturated() {
    let mut pool = test_pool();
    // test_pool 未初始化 ladder 容器；L2 阶梯写入前补齐
    pool.sell_ladders = vec![None; pool.assets.len()];
    pool.buy_ladders = vec![None; pool.assets.len()];
    pool.ladder_reserves = vec![None; pool.assets.len()];
    // L2 更新携带真实引擎价格 + sell ladder（asset1，scale=10000）
    let data0 = {
        // 两档：w=500 q=2000、w=1000 q=3000（24bit 左对齐，qty 12bit ≤ 4095）
        let a = (500u64 << 12) | 2_000;
        let b = (1_000u64 << 12) | 3_000;
        let mut d = U256::ZERO;
        d |= U256::from(a) << U256::from(256 - 24);
        d |= U256::from(b) << U256::from(256 - 48);
        d
    };
    pool.apply_l2_update_full(1, U256::from(15_000u64), 100, 0, 0, data0, U256::ZERO);
    let snap_block = 100u64; // 与 L2 块一致 → 快照不覆盖引擎价格
    let ok = |v: &str| QuoteResult {
        amountOut: U256::from_str_radix(v, 10).unwrap(),
        success: true,
    };
    let snap = Snapshot {
        assets: pool.assets.iter().map(|t| t.address).collect(),
        decimals: vec![6u8, 18u8],
        scales: vec![U256::ZERO, U256::from(10_000u64)],
        poolBalances: vec![U256::ZERO, U256::ZERO],
        vaultReserves: vec![
            U256::from(10u64).pow(U256::from(30)),
            U256::from(10u64).pow(U256::from(30)),
        ],
        vaultBalances: Vec::new(),
        quotePairs: vec![
            U256::from(1),
            U256::from(2),
            U256::from(5),
            U256::from(9),
            U256::from(13),
        ],
        quotes: vec![
            ok("1000000000000000"),     // 0→1 small（BUY 侧非本测试重点）
            ok("14485"),                // 1→0 small：tier-1 线性 bid
            ok("10000000000000000000"), // 0→1 big
            ok("14193000000"),          // 1→0 100 整枚：两档部分消费、未饱和
            ok("1000000000000000000"),  // 0→1 mid
        ],
        fee: U256::from(1000), // L2 同块快照，费率锚定 1000ppm
    };
    pool.apply_snapshot(&snap, snap_block);
    // 多档 probe 与单档不兼容 → sell_raw 清空（快照近似），但 R 必须恢复
    assert_eq!(pool.sell_raw[1], None);
    let r = pool.ladder_reserves[1].expect("R must be derived");
    assert!(
        r > U256::from(19u64) * U256::from(10u64).pow(U256::from(15)),
        "r={r}"
    );
    assert!(
        r < U256::from(21u64) * U256::from(10u64).pow(U256::from(15)),
        "r={r}"
    );
    // ladder 路径逐位复刻 100 整枚 probe
    let out = pool
        .engine_quote(1, 0, U256::from(10u64).pow(U256::from(20)))
        .unwrap();
    assert_eq!(out, U256::from(14_193_000_000u64));
    // 小额（tier-1 内）与链上 bid 一致
    let small = pool
        .engine_quote(1, 0, U256::from(10u64).pow(U256::from(14)))
        .unwrap();
    assert_eq!(small, U256::from(14_485u64));
    // 5e19 输入（跨两档）：与 ladder 逐档一致（recovered R 等价区 ±2 wei）
    let mid = pool
        .engine_quote(
            1,
            0,
            U256::from(5u64) * U256::from(10u64).pow(U256::from(19)),
        )
        .unwrap();
    assert!(
        mid >= U256::from(7_199_999_998u64) && mid <= U256::from(7_200_000_002u64),
        "mid-tier out={mid}"
    );
}

/// binaryFI 链上时效：引擎 update 后 5 块窗口内报价正常，差 ≥ 6 块 quote 返回 0。
/// 用模块自身数据 price_updated_block + last_synced_block 判定（链上实测，
/// NVDAx/SPYx/asset2/3 均在最后 update 后第 6 块起归零）。
#[test]
fn test_quote_stale_returns_zero() {
    let mut pool = test_pool();
    pool.apply_l2_update(1, U256::from(13_984u64), 100, 3, 3);
    let fresh = pool.engine_quote(0, 1, U256::from(1_000_000u64)).unwrap();
    assert!(!fresh.is_zero());

    // diff = 5 仍新鲜（窗口边界）
    pool.last_synced_block = 105;
    assert_eq!(
        pool.engine_quote(0, 1, U256::from(1_000_000u64)).unwrap(),
        fresh
    );

    // diff = 6 过期：BUY / SELL / 跨资产全部返回 0
    pool.last_synced_block = 106;
    assert_eq!(
        pool.engine_quote(0, 1, U256::from(1_000_000u64)).unwrap(),
        U256::ZERO
    );
    assert_eq!(
        pool.engine_quote(1, 0, U256::from(1_000_000_000_000u64))
            .unwrap(),
        U256::ZERO
    );
    // simulate_swap 同步 0，且不改余额
    let out = pool
        .simulate_swap(
            pool.assets[0].address,
            pool.assets[1].address,
            U256::from(1_000_000u64),
        )
        .unwrap();
    assert_eq!(out, U256::ZERO);
    // spot 归零
    let p = pool
        .calculate_price(pool.assets[0].address, pool.assets[1].address)
        .unwrap();
    assert_eq!(p, 0.0);
    // exact_out 拒绝
    assert!(pool
        .simulate_swap_exact_out(
            pool.assets[0].address,
            pool.assets[1].address,
            U256::from(1)
        )
        .is_err());

    // price_updated_block == 0（快照/锚定路径，无 update 日志）不判过期
    pool.price_updated_block[1] = 0;
    pool.last_synced_block = 10_000;
    assert_eq!(
        pool.engine_quote(0, 1, U256::from(1_000_000u64)).unwrap(),
        fresh
    );
}

/// BUY 阶梯容量进报价路径：update 重置 Σqty×R，Swap 事件消费递减，封顶随之下降。
/// 链上实测（asset2 @ 67430640）：buy_ladders [(1960,767),(1960,1150),(1960,1183)]、
/// R=20000 → 平顶 62,000,000 = Σqty×R，in≥5e10 起逐位吻合。
#[test]
fn test_buy_ladder_remaining_caps_and_consumes() {
    let mut pool = test_pool();
    let n = pool.assets.len();
    pool.sell_ladders = vec![None; n];
    pool.buy_ladders = vec![None; n];
    pool.ladder_reserves = vec![None; n];
    pool.buy_ladder_remaining = vec![None; n];
    pool.ladder_reserves[1] = Some(U256::from(20_000u64)); // 引擎 cap = R（链上实测）
                                                           // data1 = 3 档 (w=3): qty 767/1150/1183（左对齐 24bit/档）
    let tier1 = U256::from(3u64 << 12 | 767);
    let tier2 = U256::from(3u64 << 12 | 1150);
    let tier3 = U256::from(3u64 << 12 | 1183);
    let data1 = (tier1 << 232) | (tier2 << 208) | (tier3 << 184);
    pool.apply_l2_update_full(
        1,
        U256::from(13_984u64),
        100,
        3,
        3,
        U256::ZERO, // data0 空：sell ladder 清空
        data1,
    );
    assert_eq!(
        pool.buy_ladders[1],
        Some(vec![(3, 767), (3, 1150), (3, 1183)])
    );
    let cap = U256::from(62_000_000u64);
    assert_eq!(pool.buy_ladder_remaining[1], Some(cap));

    // 大额 BUY：linear > 容量 → 封顶 = Σqty×R
    let big = pool
        .engine_quote(0, 1, U256::from(10u64).pow(U256::from(24)))
        .unwrap();
    assert_eq!(big, cap);
    // simulate_swap 同语义
    let sim = pool
        .simulate_swap(
            pool.assets[0].address,
            pool.assets[1].address,
            U256::from(10u64).pow(U256::from(24)),
        )
        .unwrap();
    assert_eq!(sim, cap);

    // Swap 事件消费：0→1 消耗 amount_out
    pool.anchor_rate(0, 1, U256::from(1_000_000u64), U256::from(30_000_000u64));
    assert_eq!(
        pool.buy_ladder_remaining[1],
        Some(U256::from(32_000_000u64))
    );
    let big2 = pool
        .engine_quote(0, 1, U256::from(10u64).pow(U256::from(24)))
        .unwrap();
    assert_eq!(big2, U256::from(32_000_000u64));

    // 消费到 0：BUY 封顶 0（链上容量耗尽语义）
    pool.anchor_rate(0, 1, U256::from(1_000_000u64), U256::from(32_000_000u64));
    assert_eq!(pool.buy_ladder_remaining[1], Some(U256::ZERO));
    assert_eq!(
        pool.engine_quote(0, 1, U256::from(10u64).pow(U256::from(24)))
            .unwrap(),
        U256::ZERO
    );

    // 下一次 update 重置容量
    pool.apply_l2_update_full(1, U256::from(13_984u64), 101, 3, 3, U256::ZERO, data1);
    assert_eq!(pool.buy_ladder_remaining[1], Some(cap));
}

/// BUY 梯子被 MM 清空（data1=0）：容量必须权威归零，不能残留 alive 期非零值
/// （链上 0→j quote=0 而本地仍报价 → 幻影利润/执行 revert）。再次非空则恢复。
#[test]
fn test_buy_ladder_remaining_zeroed_when_ladder_cleared() {
    let mut pool = test_pool();
    let n = pool.assets.len();
    pool.sell_ladders = vec![None; n];
    pool.buy_ladders = vec![None; n];
    pool.ladder_reserves = vec![None; n];
    pool.buy_ladder_remaining = vec![None; n];
    pool.ladder_reserves[1] = Some(U256::from(20_000u64)); // 引擎 cap = R（链上实测）
    let tier1 = U256::from(3u64 << 12 | 767);
    let tier2 = U256::from(3u64 << 12 | 1150);
    let tier3 = U256::from(3u64 << 12 | 1183);
    let data1 = (tier1 << 232) | (tier2 << 208) | (tier3 << 184);
    let cap = U256::from(62_000_000u64);

    // alive 期：非空 data1 → Σqty×R
    pool.apply_l2_update_full(1, U256::from(13_984u64), 100, 3, 3, U256::ZERO, data1);
    assert_eq!(pool.buy_ladder_remaining[1], Some(cap));

    // MM 清空 BUY 梯子：data1=0 → 容量必须归零，报价/spot 同步归零
    pool.apply_l2_update_full(1, U256::from(13_984u64), 101, 3, 3, U256::ZERO, U256::ZERO);
    assert_eq!(pool.buy_ladder_remaining[1], Some(U256::ZERO));
    assert_eq!(
        pool.engine_quote(0, 1, U256::from(10u64).pow(U256::from(24)))
            .unwrap(),
        U256::ZERO
    );
    assert_eq!(
        pool.calculate_price(pool.assets[0].address, pool.assets[1].address)
            .unwrap(),
        0.0
    );

    // MM 重新启用 BUY：非空 data1 → 容量恢复
    pool.apply_l2_update_full(1, U256::from(13_984u64), 102, 3, 3, U256::ZERO, data1);
    assert_eq!(pool.buy_ladder_remaining[1], Some(cap));
    assert_eq!(
        pool.engine_quote(0, 1, U256::from(10u64).pow(U256::from(24)))
            .unwrap(),
        cap
    );
}

/// SELL 梯子被 MM 清空（data0=0）：max_inputs 必须权威归零（线性回退不再沿用
/// alive 期快照容量），j→0 quote/spot 同步归零。
#[test]
fn test_sell_dead_when_ladder_cleared() {
    let mut pool = test_pool();
    let n = pool.assets.len();
    pool.sell_ladders = vec![None; n];
    pool.buy_ladders = vec![None; n];
    pool.ladder_reserves = vec![None; n];
    pool.buy_ladder_remaining = vec![None; n];
    // alive 期快照观测的非零 SELL 容量
    pool.max_inputs[1] = Some(U256::from(1_000_000u64));
    let data0 = U256::from(3u64 << 12 | 767) << 232; // 单档 (w=3, qty=767)

    // alive 期：非空 data0 → sell_ladders 解析、max_inputs 不受影响
    pool.apply_l2_update_full(1, U256::from(13_984u64), 100, 3, 3, data0, U256::ZERO);
    assert_eq!(pool.sell_ladders[1], Some(vec![(3, 767)]));
    assert_eq!(pool.max_inputs[1], Some(U256::from(1_000_000u64)));

    // MM 清空 SELL 梯子：data0=0 → max_inputs 归零，j→0 quote/spot 归零
    pool.apply_l2_update_full(1, U256::from(13_984u64), 101, 3, 3, U256::ZERO, U256::ZERO);
    assert_eq!(pool.sell_ladders[1], None);
    assert_eq!(pool.max_inputs[1], Some(U256::ZERO));
    assert_eq!(
        pool.engine_quote(1, 0, U256::from(10u64).pow(U256::from(24)))
            .unwrap(),
        U256::ZERO
    );
    assert_eq!(
        pool.calculate_price(pool.assets[1].address, pool.assets[0].address)
            .unwrap(),
        0.0
    );
}

/// spot 与链上可交易性对齐（P2 加固）：方向容量恒为 0 时 spot 必须为 0.0，
/// 与 simulate_swap/链上 quote 一致，避免 multihop/2hop 预过滤把已死方向排高。
/// SELL 死 = maxIn==0（ladderWeight×reserve=0，只买不卖）；BUY 死 =
/// buy_ladder_remaining==0（Σqty×R=0）或快照 maxOut==0；跨资产任一侧死则 0。
#[test]
fn test_spot_zero_when_direction_dead() {
    let mut pool = test_pool();
    // 第三资产覆盖跨资产方向
    pool.assets.push(Token::new_with_decimals(
        address!("0xb7c00000bcdeef966b20b3d884b98e64d2b06b4f"),
        8,
    ));
    let n = pool.assets.len();
    pool.prices.push(U256::ZERO);
    pool.spreads.push(0);
    pool.bid_offsets.push(0);
    pool.ask_offsets.push(0);
    pool.q0j.push(None);
    pool.sell_raw.push(None);
    pool.buy_zero_over_vault.push(false);
    pool.max_outputs.push(None);
    pool.max_inputs.push(None);
    pool.reserves.push(U256::from(62_000_000u64));
    pool.rates.resize(n * n, Rate::zero());
    pool.price_updated_block.push(0);
    pool.sell_ladders = vec![None; n];
    pool.buy_ladders = vec![None; n];
    pool.ladder_reserves = vec![None; n];
    pool.buy_ladder_remaining = vec![None; n];

    // 正常状态：两侧 + 跨资产 spot 均非 0
    pool.apply_l2_update(1, U256::from(15_005u64), 100, 3, 3);
    pool.apply_l2_update(2, U256::from(20_000u64), 100, 4, 4);
    let p_sell = pool
        .calculate_price(pool.assets[1].address, pool.assets[0].address)
        .unwrap();
    let p_buy = pool
        .calculate_price(pool.assets[0].address, pool.assets[1].address)
        .unwrap();
    let p_cross = pool
        .calculate_price(pool.assets[1].address, pool.assets[2].address)
        .unwrap();
    assert!(p_sell > 0.0 && p_buy > 0.0 && p_cross > 0.0);

    // SELL 死：maxIn=0 → spot(i→0)=0，跨资产输入侧死；BUY 不受影响
    pool.max_inputs[1] = Some(U256::ZERO);
    assert_eq!(
        pool.calculate_price(pool.assets[1].address, pool.assets[0].address)
            .unwrap(),
        0.0
    );
    assert_eq!(
        pool.calculate_price(pool.assets[1].address, pool.assets[2].address)
            .unwrap(),
        0.0
    );
    assert!(
        pool.calculate_price(pool.assets[0].address, pool.assets[1].address)
            .unwrap()
            > 0.0
    );
    pool.max_inputs[1] = None;

    // BUY 死：buy_ladder_remaining=0 → spot(0→j)=0，跨资产输出侧死；SELL 不受影响
    pool.buy_ladder_remaining[2] = Some(U256::ZERO);
    assert_eq!(
        pool.calculate_price(pool.assets[0].address, pool.assets[2].address)
            .unwrap(),
        0.0
    );
    assert_eq!(
        pool.calculate_price(pool.assets[1].address, pool.assets[2].address)
            .unwrap(),
        0.0
    );
    assert!(
        pool.calculate_price(pool.assets[2].address, pool.assets[0].address)
            .unwrap()
            > 0.0
    );
    pool.buy_ladder_remaining[2] = None;

    // maxOut=0 同语义；未知（None）不门控，与 96% 兜底口径一致
    pool.max_outputs[2] = Some(U256::ZERO);
    assert_eq!(
        pool.calculate_price(pool.assets[0].address, pool.assets[2].address)
            .unwrap(),
        0.0
    );
    pool.max_outputs[2] = None;
    assert!(
        pool.calculate_price(pool.assets[0].address, pool.assets[2].address)
            .unwrap()
            > 0.0
    );
}

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

/// L2 完整应用：price + ladder 点差（新引擎 1999/2000 因子，链上逐位对拍）：
///   - SKHYx（index 1）：price=13984，ask 字段=3、sell 字段=3 →
///     bid_offset = ceil(13984/2000)+3 = 10 → bid=13974；ask=13987
///   - asset2（index 2，scale=100000）：price=640774，raw 252/252 →
///     ask_off=2520、sell_off=2520 → bid_offset = ceil(6407740/2000)+2520 = 5724
#[test]
fn test_apply_l2_update_spread_and_scale() {
    let mut pool = test_pool();
    // SKHYx（index 1，dj=18，scale=10000 默认）
    pool.apply_l2_update(1, U256::from(13_984u64), 100, 3, 3);
    assert_eq!(pool.prices[1], U256::from(13_984u64));
    assert_eq!(pool.bid_price(1).unwrap(), U256::from(13_974u64));
    assert_eq!(pool.ask_price(1).unwrap(), U256::from(13_987u64));
    // q0j = floor(1e20×1999/(2000×13987))，链上 0→SKHYx in=1e6 逐位对拍
    assert_eq!(
        pool.q0j[1],
        Some(U256::from_str_radix("7145921212554514", 10).unwrap())
    );
    let out = pool.engine_quote(0, 1, U256::from(1_000_000u64)).unwrap();
    assert_eq!(out, U256::from_str_radix("7145921212554514", 10).unwrap());
    // SELL：SKHYx→0 in=1e15 → 139,740（链上逐位对拍）
    assert_eq!(
        pool.engine_quote(1, 0, U256::from(1_000_000_000_000_000u64))
            .unwrap(),
        U256::from(139_740u64)
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
    assert_eq!(pool.spreads[2], 8244);
    assert_eq!(pool.ask_price(2).unwrap(), U256::from(6_410_260u64));
    assert_eq!(pool.bid_price(2).unwrap(), U256::from(6_402_016u64));
    // q0j = floor(10^10×1999/(2000×6,410,260)) = 1559（dj=8）
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
    // 0→SKHYx：rate = q0j/10^6 = 7,145,921,212,554,514 / 1e6
    let rate = pool.rates[pool.pair_index(0, 1)];
    assert_eq!(
        rate.num,
        U256::from_str_radix("7145921212554514", 10).unwrap()
    );
    assert_eq!(rate.den, U256::from(1_000_000u64));
    // SKHYx→0：raw = 13984×1999 − 3×2000 = 27,948,016，
    // rate = raw×10^4/(2000×10^18) = 27,948,016/(2×10^17)
    let rate_ba = pool.rates[pool.pair_index(1, 0)];
    assert_eq!(rate_ba.num, U256::from(27_948_016u64));
    assert_eq!(rate_ba.den, U256::from(200_000_000_000_000_000u64));

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
    // BUY 小额：0→SKHYx in=1e6 → q0j = 7,145,921,212,554,514
    let out = pool
        .simulate_swap(
            pool.assets[0].address,
            pool.assets[1].address,
            U256::from(1_000_000u64),
        )
        .unwrap();
    assert_eq!(out, U256::from_str_radix("7145921212554514", 10).unwrap());
    // SELL：SKHYx→0 in=1e18 → 139,740,080
    // （精确有理数 raw=27,948,016，含 raw/2000 小数部分；整数 bid=13,974 会低估 80）
    let sell = pool
        .simulate_swap(
            pool.assets[1].address,
            pool.assets[0].address,
            U256::from(1_000_000_000_000_000_000u64),
        )
        .unwrap();
    assert_eq!(sell, U256::from(139_740_080u64));

    // 大额受输出余额 96% 截断
    let huge = pool
        .simulate_swap(
            pool.assets[0].address,
            pool.assets[1].address,
            U256::from(1_000_000_000_000u64),
        )
        .unwrap();
    let cap = U256::from(8_000_000_000_000_000_000u64) * U256::from(9600) / U256::from(10_000);
    assert_eq!(huge, cap);
}

/// SELL 阶梯上限：maxIn 截断。新引擎因子：
///   - price=15005/askOff=4/sellOff=4 → bid_offset=ceil(15005/2000)+4=12 → bid=14993
///   - in=1e18（< maxIn）：149,930,000（线性）
///   - in=1e20 / 1e24（> maxIn）：3,682,100,884（饱和 = maxIn × bid × 1e-14）
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

    // 线性区（in=1e18 < maxIn）：out = in×raw×1e4/(2000×1e18)
    // raw = 15005×1999 − 4×2000 = 29,986,995 → out(1e18) = 149,934,975
    let out = pool
        .simulate_swap(
            pool.assets[1].address,
            pool.assets[0].address,
            U256::from(10u64.pow(18)),
        )
        .unwrap();
    assert_eq!(out, U256::from(149_934_975u64));

    // 饱和区：in=1e20 与 1e24 均为 min(in, maxIn)×raw×1e4/(2000×1e18)
    let out = pool
        .simulate_swap(
            pool.assets[1].address,
            pool.assets[0].address,
            U256::from(10u128.pow(20)),
        )
        .unwrap();
    assert_eq!(out, U256::from(3_682_223_064u64));
    let out = pool
        .simulate_swap(
            pool.assets[1].address,
            pool.assets[0].address,
            U256::from(10u128.pow(24)),
        )
        .unwrap();
    assert_eq!(out, U256::from(3_682_223_064u64));
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
    assert_eq!(out, U256::from_str_radix("7145921212554514", 10).unwrap());

    for amt in [U256::from(1_000_000_000_000u64), U256::from(10u128.pow(24))] {
        let out = pool
            .simulate_swap(pool.assets[0].address, pool.assets[1].address, amt)
            .unwrap();
        assert_eq!(
            out,
            U256::from_str_radix("7643300000000000000", 10).unwrap()
        );
    }
}

/// BUY 超阈值归零型（阶梯容量 > 金库余额）：linear ≤ 金库余额才返回，否则 0。
/// 构造样例：xSOL dj=9，price=7376/askOff=3/sellOff=3 → ask=7379、q0j=13,545,195
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
    // BUY(1e6) = q0j = 13,545,195
    assert_eq!(
        pool.engine_quote(0, 2, U256::from(1_000_000u64)).unwrap(),
        U256::from(13_545_195u64)
    );
    // 线性区：in=11,870,000 → 160,781,474 ≤ 金库 → 返回
    assert_eq!(
        pool.engine_quote(0, 2, U256::from(11_870_000u64)).unwrap(),
        U256::from(160_781_474u64)
    );
    // 线性区边界：in=11,880,000 → 160,916,926 ≤ 金库 → 返回
    assert_eq!(
        pool.engine_quote(0, 2, U256::from(11_880_000u64)).unwrap(),
        U256::from(160_916_926u64)
    );
    // 超阈值归零：in=11,900,000 → 161,187,830 > 金库 → 0
    assert_eq!(
        pool.engine_quote(0, 2, U256::from(11_900_000u64)).unwrap(),
        U256::ZERO
    );
    // 跨资产两段式：SKHYx(1) → xSOL(2), in=5e16（第二段不含 1999/2000 因子）
    pool.apply_l2_update(1, U256::from(13_984u64), 100, 3, 3);
    // v = floor(5e16 × 27,948,016 × 1e4 / (2000×1e18)) = 6,987,004
    // linear = floor(6,987,004 × 1e5 / 7379) = 94,687,681 ≤ 金库 → 返回
    assert_eq!(
        pool.engine_quote(1, 2, U256::from(50_000_000_000_000_000u64))
            .unwrap(),
        U256::from(94_687_681u64)
    );
    // in=1e17：v = 13,974,008 → linear = 189,375,362 > 金库 → 0
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
    // asset4(2)：price=32481/askOff=18/sellOff=9 → ask=32499、raw4=64,911,519
    pool.apply_l2_update(2, U256::from(32_481u64), 100, 18, 9);
    pool.apply_l2_update(1, U256::from(13_984u64), 100, 3, 3);
    // SKHYx(1) -> asset4(2), in=1e18：两段式（第二段不含 1999/2000 因子）
    // v = floor(1e18 × 27,948,016 × 1e4 / (2000×1e18)) = 139,740,080
    // out = floor(139,740,080 × 1e14 / 32499) = 429,982,707,160,220,314
    let out = pool
        .engine_quote(1, 2, U256::from(10u64.pow(18)))
        .expect("engine quote");
    assert_eq!(out, U256::from_str_radix("429982707160220314", 10).unwrap());
    // asset4(2) -> SKHYx(1)：raw4 = 32481×1999 − 9×2000 = 64,911,519
    // v = floor(1e18 × 64,911,519 × 1e4 / (2000×1e18)) = 324,557,595
    // out = floor(324,557,595 × 1e14 / 13987) = 2,320,423,214,413,383,856
    let out = pool
        .engine_quote(2, 1, U256::from(10u64.pow(18)))
        .expect("engine quote");
    assert_eq!(
        out,
        U256::from_str_radix("2320423214413383856", 10).unwrap()
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
    // 点差偏移 0/0 → raw = 15005×1999 = 29,994,995，
    // rate(1→0) = raw×10^4/(2000×10^18) = 29,994,995/(2×10^17)
    let rate_ba = pool.rates[pool.pair_index(1, 0)];
    assert_eq!(rate_ba.num, U256::from(29_994_995u64));
    assert_eq!(rate_ba.den, U256::from(200_000_000_000_000_000u64));
    // BUY：q0j = floor(1e20×1999/(2000×15005))，rate(0→1) = q0j/1e6
    let rate = pool.rates[pool.pair_index(0, 1)];
    assert_eq!(
        rate.num,
        U256::from_str_radix("6661112962345884", 10).unwrap()
    );
    assert_eq!(rate.den, U256::from(1_000_000u64));
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

/// BUY 低小数位资产：dj=4 < d0-2 时 q0j = floor(10^(dj+2)×1999/(2000×ask)) 很小，
/// BUY 报价 = floor(in × q0j / 10^d0) 仍精确。
#[test]
fn test_engine_quote_buy_low_decimals_asset() {
    let mut pool = test_pool();
    pool.assets[1] =
        Token::new_with_decimals(address!("0x58100046a4afcd4ee4fadbd4244f3f895a341c56"), 4);
    // ask = 15005 + 4 = 15009 → q0j = floor(1e6×1999/(2000×15009)) = 66
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
        (1_000_000_000_000_00, 76_862),               // 1e14，首档内
        (10_000_000_000_000_000, 7_686_254),          // 1e16
        (1_000_000_000_000_000_000, 768_625_495),     // 1e18
        (5_000_000_000_000_000_000, 3_843_087_511),   // 5e18，跨 2 档
        (10_000_000_000_000_000_000, 7_685_995_104),  // 1e19，跨 3 档
        (18_000_000_000_000_000_000, 13_827_464_262), // 1.8e19，跨 4 档
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
    // raw = 76925×1999 - 3×2000 = 153,767,075；out = in×raw×1e4/(2000×1e18)
    let out = fallback
        .engine_quote(2, 0, U256::from(1_000_000_000_000_000u64))
        .expect("sell quote fallback");
    assert_eq!(out, U256::from(768_835u64));
}

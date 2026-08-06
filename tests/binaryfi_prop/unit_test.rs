//! BinaryFi propAMM 单元测试（报价公式 / L2 calldata 解析 / 三层同步）。
//!
//! 由 `src/amms/binaryfi_prop/mod.rs` 的 `#[cfg(test)] mod tests` 迁移而来，
//! 通过 `tests/binaryfi_prop.rs` 入口编译运行。

use alloy::hex;
use alloy::primitives::Log as AlloyLog;
use alloy::primitives::{address, keccak256, Address, B256, Bytes, LogData, U256};
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

    let enriched = enrich_update_log_data(&[hex::encode(&raw)], Some(tx_hash), &log_data, BINARYFI_ENGINE_ADDRESS)
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

/// L2 完整应用：price + ladder 点差。67160388 实测：
///   - xETH：price=186787，buyLad=0x02e00100（ask 字段 0x02e0→46）、
///     sellLad=0x02e15a00（bid 字段 0x02e1→46）→ spread=92，
///     ask=186833 / bid=186741
///   - asset2（scale=100000）：raw 字段 0x0fc3/0x0fc1 → 252/252 → ×10 = 2520/2520
#[test]
fn test_apply_l2_update_spread_and_scale() {
    let mut pool = test_pool();
    pool.assets.push(Token::new_with_decimals(
        address!("0xE7B000003A45145decf8a28FC755aD5eC5EA025A"),
        18,
    ));
    let n = pool.assets.len();
    pool.prices.push(U256::ZERO);
    pool.spreads.push(0);
    pool.rates = vec![Rate::zero(); n * n];
    pool.price_updated_block.push(0);

    // xETH：scale=10000（默认）
    pool.apply_l2_update(1, U256::from(186_787u64), 100, 46, 46);
    assert_eq!(pool.prices[1], U256::from(186_787u64));
    assert_eq!(pool.spreads[1], 92);
    assert_eq!(pool.ask_price(1).unwrap(), U256::from(186_833u64));
    assert_eq!(pool.bid_price(1).unwrap(), U256::from(186_741u64));
    // 0→xETH in=1e6：floor(1e20/186833) = 535,237,351,003,302
    let out = pool.engine_quote(0, 1, U256::from(1_000_000u64)).unwrap();
    assert_eq!(out, U256::from_str_radix("535237351003302", 10).unwrap());

    // asset2：scale=100000，raw 252 → 实际 2520
    pool.assets.push(Token::new_with_decimals(
        address!("0xb7C00000bcDEeF966b20B3D884B98E64d2b06b4f"),
        8,
    ));
    let n = pool.assets.len();
    pool.prices.push(U256::ZERO);
    pool.spreads.push(0);
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
}

#[test]
fn test_enrich_wrong_tx_hash_returns_none() {
    let raw = real_update_tx();
    let topics = vec![BINARYFI_UPDATE_EVENT, update_asset_topic()];
    let log_data = LogData::new(topics, Bytes::new()).unwrap();
    let bogus = B256::repeat_byte(0xff);
    assert!(
        enrich_update_log_data(
            &[hex::encode(&raw)],
            Some(bogus),
            &log_data,
            BINARYFI_ENGINE_ADDRESS
        )
        .is_none()
    );
    assert!(
        enrich_update_log_data(&[] as &[&str], Some(keccak256(&raw)), &log_data, BINARYFI_ENGINE_ADDRESS)
            .is_none()
    );
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
    pool.prices[1] = U256::from(15005);
    pool.spreads[1] = BINARYFI_DEFAULT_SPREAD;
    pool.apply_price_update(1, U256::from(15005), 100);
    // ask = 15005 + 4 = 15009：num = 10^(18-6+2), den = 15009
    let rate = pool.rates[pool.pair_index(0, 1)];
    assert_eq!(rate.num, U256::from(10u64.pow(14)));
    assert_eq!(rate.den, U256::from(15009));
    // bid = 15005 - 4 = 15001
    let rate_ba = pool.rates[pool.pair_index(1, 0)];
    assert_eq!(rate_ba.num, U256::from(15001));
    assert_eq!(rate_ba.den, U256::from(10u64.pow(14)));

    // 重复同 price 更新：费率不变（幂等）
    pool.apply_price_update(1, U256::from(15005), 101);
    assert_eq!(pool.rates[pool.pair_index(0, 1)], rate);
    assert_eq!(pool.price_updated_block[1], 101);
}

#[test]
fn test_simulate_swap_matches_onchain_sample() {
    let mut pool = test_pool();
    // 真实链上：USDT0 → SKHYx，in=35,357,671 → out=235,576,460,790,192,551
    // 价格已知路径：engine_quote 用 ask=15009（spread=8）
    pool.prices[1] = U256::from(15005);
    pool.spreads[1] = BINARYFI_DEFAULT_SPREAD;
    pool.apply_price_update(1, U256::from(15005), 100);
    let out = pool
        .simulate_swap(
            pool.assets[0].address,
            pool.assets[1].address,
            U256::from(35_357_671u64),
        )
        .unwrap();
    assert_eq!(out, U256::from_str_radix("235576460790192551", 10).unwrap());

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

/// 67160388 真实链上对拍：SELL 阶梯上限 maxIn = 196 × 1.253e17（ladder weight ×
/// engine reserve，maxIn 由 100 整枚 probe 恢复）：
///   - in=1e18（< maxIn）：150,010,000（线性）
///   - in=1e20（100 整枚，> maxIn）：3,684,065,588（饱和 = maxIn × bid × 1e-14）
///   - in=1e24：同样 3,684,065,588
#[test]
fn test_engine_quote_sell_cap_matches_onchain() {
    let mut pool = test_pool();
    // 67160388 配置：cap=15005，spread_sell=4 → bid=15001
    pool.prices[1] = U256::from(15005);
    pool.spreads[1] = 8;
    pool.apply_price_update(1, U256::from(15005), 100);
    // maxIn = 196 × 125,300,000,000,000,000
    pool.max_inputs[1] = Some(U256::from(196u64) * U256::from(125_300_000_000_000_000u64));
    assert_eq!(
        pool.max_inputs[1],
        Some(U256::from_str_radix("24558800000000000000", 10).unwrap())
    );

    // 线性区（in=1e18 < maxIn）
    let out = pool
        .simulate_swap(
            pool.assets[1].address,
            pool.assets[0].address,
            U256::from(10u64.pow(18)),
        )
        .unwrap();
    assert_eq!(out, U256::from(150_010_000u64));

    // 饱和区：in=1e20 与 1e24 均为 maxIn×bid×1e-14
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

/// 67160388 真实链上对拍：BUY 阶梯上限 maxOut = 61 × 1.253e17 = 7,643,300,000,000,000,000：
///   - in=1e6（线性）：6,662,669,065,227,530
///   - in=1e10 / 1e24（饱和）：7,643,300,000,000,000,000
#[test]
fn test_engine_quote_buy_cap_matches_onchain() {
    let mut pool = test_pool();
    // 67160388 配置：cap=15005，spread_buy=4 → ask=15009
    pool.prices[1] = U256::from(15005);
    pool.spreads[1] = 8;
    pool.apply_price_update(1, U256::from(15005), 100);
    pool.max_outputs[1] = Some(U256::from(61u64) * U256::from(125_300_000_000_000_000u64));
    assert_eq!(
        pool.max_outputs[1],
        Some(U256::from_str_radix("7643300000000000000", 10).unwrap())
    );

    let out = pool
        .simulate_swap(
            pool.assets[0].address,
            pool.assets[1].address,
            U256::from(1_000_000u64),
        )
        .unwrap();
    assert_eq!(out, U256::from_str_radix("6662669065227530", 10).unwrap());

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

/// 67160388 真实链上对拍：xSOL 类 BUY 超阈值归零型
/// （0→j 大额 probe = 0 且小额 > 0：阶梯容量 > 金库余额）：
///   - in=1e6 → 13,551,971（线性，ask=7379）
///   - in=11,870,000 → 160,861,905（线性，≤ 金库 160,992,934）
///   - in=11,880,000 → 0（linear > 金库余额，引擎归零而非饱和）
#[test]
fn test_engine_quote_buy_zero_over_vault() {
    let mut pool = test_pool();
    pool.assets.push(Token::new_with_decimals(
        address!("0x505000008DE8748DBd4422ff4687a4FC9bEba15b"),
        9,
    ));
    let n = pool.assets.len();
    pool.prices.push(U256::from(7376));
    pool.spreads.push(6);
    pool.rates = vec![Rate::zero(); n * n];
    pool.price_updated_block.push(0);
    pool.buy_zero_over_vault.push(true);
    pool.max_outputs.push(None);
    pool.max_inputs.push(None);
    pool.reserves.push(U256::from(160_992_934u64));
    pool.apply_price_update(2, U256::from(7376), 100);
    // ask = 7376 + 3 = 7379
    assert_eq!(
        pool.engine_quote(0, 2, U256::from(1_000_000u64)).unwrap(),
        U256::from(13_551_971u64)
    );
    assert_eq!(
        pool.engine_quote(0, 2, U256::from(11_870_000u64)).unwrap(),
        U256::from(160_861_905u64)
    );
    assert_eq!(
        pool.engine_quote(0, 2, U256::from(11_880_000u64)).unwrap(),
        U256::ZERO
    );
    // 跨资产两段式：SKHYx(1) → xSOL(2), in=5e16
    // v = floor(5e16 * 15001 * 1e4 / 1e18) = 7,500,500
    // linear = floor(7,500,500 * 1e5 / 7379) = 101,646,564 ≤ 金库 → 返回
    pool.prices[1] = U256::from(15005);
    pool.spreads[1] = 8;
    pool.apply_price_update(1, U256::from(15005), 100);
    assert_eq!(
        pool.engine_quote(1, 2, U256::from(50_000_000_000_000_000u64))
            .unwrap(),
        U256::from(101_646_564u64)
    );
    // in=1e17：v = 15,001,000 → linear = 203,293,129 > 金库 → 0
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
    pool.prices.push(U256::from(32481));
    pool.spreads.push(35);
    pool.prices[1] = U256::from(15005);
    pool.rates = vec![Rate::zero(); n * n];
    pool.price_updated_block.push(0);
    // SKHYx(1) -> asset4(2), in=1e18：两段式
    // v = floor(1e18 * 15001 * 1e4 / 1e18) = 150_010_000
    // out = floor(150_010_000 * 1e14 / 32499) = 461_583_433_336_410_351
    let out = pool
        .engine_quote(1, 2, U256::from(10u64.pow(18)))
        .expect("engine quote");
    assert_eq!(out, U256::from_str_radix("461583433336410351", 10).unwrap());
    // asset4(2) -> SKHYx(1)：bid4 = 32481 - 17 = 32464
    // v = floor(1e18 * 32464 * 1e4 / 1e18) = 324_640_000
    // out = floor(324_640_000 * 1e14 / 15009) = 2_162_968_885_335_465_387
    let out = pool
        .engine_quote(2, 1, U256::from(10u64.pow(18)))
        .expect("engine quote");
    assert_eq!(
        out,
        U256::from_str_radix("2162968885335465387", 10).unwrap()
    );
}

#[test]
fn test_recover_bid_ask_from_quotes() {
    // SKHYx：0->11 in=1e6 → 6_662_669_065_227_530（ask=15009）
    //       11->0 in=1e18 → 150_010_000（bid=15001）
    let ask = BinaryFiPropPool::recover_ask(
        U256::from_str_radix("6662669065227530", 10).unwrap(),
        18,
    )
    .expect("ask");
    assert_eq!(ask, U256::from(15009));
    // asset4：0->4 in=1e6 → 3_077_017_754_392_442（ask=32499）
    let ask4 = BinaryFiPropPool::recover_ask(
        U256::from_str_radix("3077017754392442", 10).unwrap(),
        18,
    )
    .expect("ask4");
    assert_eq!(ask4, U256::from(32499));
    // xSOL：0->3 in=1e6 → 13_551_971（dj=9, ask=7379）
    let ask3 = BinaryFiPropPool::recover_ask(U256::from(13_551_971u64), 9).expect("ask3");
    assert_eq!(ask3, U256::from(7379));
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
    // spread 未知 → 默认 8：ask = 15009
    let rate = pool.rates[pool.pair_index(0, 1)];
    assert_eq!(rate.num, U256::from(10u64.pow(14)));
    assert_eq!(rate.den, U256::from(15009));
    // bid = 15001
    let rate_ba = pool.rates[pool.pair_index(1, 0)];
    assert_eq!(rate_ba.num, U256::from(15001));
    assert_eq!(rate_ba.den, U256::from(10u64.pow(14)));
}

#[test]
fn test_sync_canonical_update_marks_stale() {
    let mut pool = test_pool();
    let log_data =
        LogData::new(vec![BINARYFI_UPDATE_EVENT, asset_topic(1)], Bytes::new()).unwrap();
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

/// BUY 公式负指数分支：目标资产 decimals(4) < USDT0 decimals(6) 时
/// linear = in * 10^(dj-d0+2) / ask = in / ask（旧实现误算为 in*100/ask）。
#[test]
fn test_engine_quote_buy_low_decimals_asset() {
    let mut pool = test_pool();
    pool.assets[1] = Token::new_with_decimals(
        address!("0x58100046a4afcd4ee4fadbd4244f3f895a341c56"),
        4,
    );
    pool.prices[1] = U256::from(15005);
    pool.spreads[1] = 8;
    pool.apply_price_update(1, U256::from(15005), 100);
    // ask = 15005 + 4 = 15009；in=10^6 → 10^6 / 15009 = 66（floor）
    let out = pool
        .engine_quote(0, 1, U256::from(1_000_000u64))
        .expect("quote");
    assert_eq!(out, U256::from(1_000_000u64) / U256::from(15009));
}

/// exact_out 必须遵守精确 cap（maxOut / maxIn），不能用 96% 金库兜底高估合法输出。
#[test]
fn test_exact_out_respects_ladder_caps() {
    let mut pool = test_pool();
    pool.prices[1] = U256::from(15005);
    pool.spreads[1] = 8;
    pool.apply_price_update(1, U256::from(15005), 100);

    // BUY 饱和型：maxOut=1000（远小于 96% 金库）→ 超过即拒绝
    pool.max_outputs[1] = Some(U256::from(1000u64));
    assert!(
        pool.simulate_swap_exact_out(
            pool.assets[0].address,
            pool.assets[1].address,
            U256::from(5000u64),
        )
        .is_err()
    );
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
    assert!(
        pool.simulate_swap_exact_out(
            pool.assets[1].address,
            pool.assets[0].address,
            U256::from(200_000_000u64),
        )
        .is_err()
    );
    let in_needed = pool
        .simulate_swap_exact_out(
            pool.assets[1].address,
            pool.assets[0].address,
            U256::from(100_000_000u64),
        )
        .unwrap();
    assert!(in_needed > U256::ZERO);
}

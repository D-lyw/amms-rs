//! Arc ARGUS（UniswapV3 主池）FoT 回放对账
//!
//! 用**链上前一块的池子状态**回放链上已成功的 swap，逐位对账：
//!   - 方向 A→B（卖出税币进池）：池子按**名义额**实收 → 输出应与链上逐位一致
//!   - 方向 B→A（从池子买出税币）：池子 math 输出 gross，接收方实收 net = gross − floor(gross×fee)
//!
//! 用法（数值全部来自链上，见命令历史）：
//! ```text
//! cargo run --example replay_arc_argus_v3 -- \
//!   --pool 0x6a3b.. --token-a <base> --token-b <taxed> --dec-a 6 --dec-b 18 \
//!   --sqrt <pre_sqrtPriceX96> --tick <pre_tick> --liquidity <pre_liquidity> --fee 10000 \
//!   --tax-bps 100 --from A --amount-in <raw>
//! ```
use std::str::FromStr;

use alloy::primitives::{Address, U256};
use amms::amms::{
    amm::AutomatedMarketMaker, fot, fot::FotTaxType, uniswap_v3::UniswapV3Pool, Token,
};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut pool_addr = Address::ZERO;
    let mut token_a = Address::ZERO;
    let mut token_b = Address::ZERO;
    let mut dec_a = 6u8;
    let mut dec_b = 18u8;
    let mut sqrt_price = U256::ZERO;
    let mut tick = 0i32;
    let mut liquidity = 0u128;
    let mut fee = 0u32;
    let mut tick_spacing = 0i32;
    let mut tax_bps = 0u64;
    let mut from_a = true;
    let mut as_balance = false;
    let mut amount_in = U256::ZERO;

    let mut i = 1;
    while i + 1 < args.len() {
        let k = args[i].trim_start_matches("--").to_string();
        let v = args[i + 1].clone();
        match k.as_str() {
            "pool" => pool_addr = Address::from_str(&v).unwrap(),
            "token-a" => token_a = Address::from_str(&v).unwrap(),
            "token-b" => token_b = Address::from_str(&v).unwrap(),
            "dec-a" => dec_a = v.parse().unwrap(),
            "dec-b" => dec_b = v.parse().unwrap(),
            "sqrt" => sqrt_price = U256::from_str(&v).unwrap(),
            "tick" => tick = v.parse().unwrap(),
            "liquidity" => liquidity = v.parse().unwrap(),
            "fee" => fee = v.parse().unwrap(),
            "tick-spacing" => tick_spacing = v.parse().unwrap(),
            "tax-bps" => tax_bps = v.parse().unwrap(),
            "from" => from_a = v.eq_ignore_ascii_case("a"),
            "mode" => as_balance = v.eq_ignore_ascii_case("balance"),
            "amount-in" => amount_in = U256::from_str(&v).unwrap(),
            _ => {}
        }
        i += 2;
    }

    let (token_in, token_out) = if from_a { (token_a, token_b) } else { (token_b, token_a) };

    let mut pool = UniswapV3Pool::new(pool_addr);
    pool.token_a = Token::new_with_decimals(token_a, dec_a);
    pool.token_b = Token::new_with_decimals(token_b, dec_b);
    pool.fee = fee;
    pool.tick_spacing = tick_spacing;
    pool.tick = tick;
    pool.sqrt_price = sqrt_price;
    pool.liquidity = liquidity;

    // 注册税种（含白名单池过滤）
    if tax_bps > 0 {
        fot::register_fot_token(
            token_b,
            FotTaxType::BuySell {
                buy_fee_bps: tax_bps,
                sell_fee_bps: tax_bps,
                pairs: vec![pool_addr],
                swap_back_threshold: U256::MAX,
            },
        );
        fot::apply_to_token(&mut pool.token_a);
        fot::apply_to_token(&mut pool.token_b);
    }

    // `--amount-in` 语义二选一：
    //   默认（名义额口径）：= 池子应实收的名义额（对账链上 Swap 事件用）
    //   `--mode balance`（余额口径）：= 本腿付款方可用余额（引擎 hop 链语义）
    let input_token = if from_a { &pool.token_a } else { &pool.token_b };
    let (nominal_in, balance_in) = if as_balance {
        (
            input_token.fot_input_nominal_for_balance(pool_addr, amount_in),
            amount_in,
        )
    } else {
        (
            amount_in,
            input_token.fot_input_cost_for(pool_addr, amount_in),
        )
    };

    // 无税基准（= 池子 math 的输出，即链上 Swap 事件里的 gross）
    let mut plain = pool.clone();
    plain.token_a.fot_tax = None;
    plain.token_b.fot_tax = None;
    let gross_out = plain
        .simulate_swap(token_in, token_out, nominal_in)
        .unwrap();
    let taxed_out = pool.simulate_swap(token_in, token_out, balance_in).unwrap();
    // V3 输入侧实际支出（余额/授权口径）
    let cost = input_token.fot_input_cost_for(pool_addr, nominal_in);

    println!("direction      = {}", if from_a { "A→B" } else { "B→A" });
    println!(
        "口径           = {}",
        if as_balance { "余额(引擎语义)" } else { "名义额(对账链上)" }
    );
    println!("balance_in     = {balance_in}");
    println!("nominal_in     = {nominal_in}");
    println!("gross_out(无税)= {gross_out}");
    println!("taxed_out      = {taxed_out}");
    println!("payer_cost     = {cost}  (V3 加收型：转给池子 nominal_in，另付 tax)");
}

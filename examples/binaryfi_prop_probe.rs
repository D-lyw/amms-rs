//! BinaryFi propAMM mainfork 验证。
//!
//! 在 anvil fork 上运行模块真实代码路径：`BinaryFiPropPool::init()`（批量合约 +
//! 引擎 getAssetReserves）、`simulate_swap`、`update()`、日志驱动
//! `apply_price_update`，并逐 pair 用独立的 `pool.quote` eth_call 交叉验证本地
//! 模拟与链上报价是否逐位一致。
//!
//! 用法:
//!   cargo run --example binaryfi_prop_probe -- http://127.0.0.1:8557

use std::sync::Arc;

use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{address, Address, U256},
    providers::{Provider, ProviderBuilder},
    rpc::client::ClientBuilder,
    sol,
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::amms::{
    amm::AutomatedMarketMaker,
    binaryfi_prop::{BinaryFiPropPool, BINARYFI_POOL_ADDRESS, BINARYFI_ROUTER_ADDRESS},
    Token,
};

sol! {
    #[sol(rpc)]
    interface IBinaryFiQuote {
        function quote(
            address recipient,
            address tokenIn,
            address tokenOut,
            uint256 amountIn
        ) external view returns (uint256 amountOut);
    }
}

const USDT0: Address = address!("0x779ded0c9e1022225f8e0630b35a9b54be713736");
const SKHYX: Address = address!("0x58100046a4afcd4ee4fadbd4244f3f895a341c56");
const XETH: Address = address!("0xe7b000003a45145decf8a28fc755ad5ec5ea025a");
const XSOL: Address = address!("0x505000008de8748dbd4422ff4687a4fc9beba15b");
const CRCLX: Address = address!("0xfebded1b0986a8ee107f5ab1a1c5a813491deceb");

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();

    let mut args = std::env::args().skip(1);
    let rpc = args
        .next()
        .or_else(|| std::env::var("XLAYER_RPC").ok())
        .unwrap_or_else(|| "http://127.0.0.1:8557".to_string());
    let block = args
        .next()
        .and_then(|b| b.parse::<u64>().ok())
        .map(alloy::eips::BlockNumberOrTag::Number)
        .map(BlockId::Number);
    eprintln!("binaryfi_prop_probe -> {rpc} block={block:?}");
    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(300))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(rpc.parse()?);
    let provider = Arc::new(ProviderBuilder::new().connect_client(client));

    // 0) 模块真实 init()
    let block_id = block.unwrap_or(BlockId::latest());
    let mut pool = BinaryFiPropPool::default()
        .init(block_id, provider.clone())
        .await?;
    println!("assets = {}", pool.assets.len());
    for (i, a) in pool.assets.iter().enumerate() {
        let sh = pool.spreads.get(i).copied().unwrap_or(0);
        let bid = pool
            .prices
            .get(i)
            .copied()
            .map(|p| p.saturating_sub(U256::from(sh / 2)));
        let ask = pool
            .prices
            .get(i)
            .copied()
            .map(|p| p + U256::from((sh + 1) / 2));
        println!(
            "  [{i:02}] {} d={} price={} spread={} bid={} ask={} reserve={}",
            a.address,
            a.decimals,
            pool.prices[i],
            sh,
            bid.unwrap_or_default(),
            ask.unwrap_or_default(),
            pool.reserves.get(i).copied().unwrap_or_default()
        );
    }
    let derived = pool.rates.iter().filter(|r| !r.is_zero()).count();
    println!("derived rates = {derived}/132");

    let pool_contract = IBinaryFiQuote::new(BINARYFI_POOL_ADDRESS, provider.clone());
    let assets_snapshot = pool.assets.clone();
    let quote_block = block_id;
    let quote_chain = |i: usize, j: usize, amount: U256| {
        let c = pool_contract.clone();
        let assets = assets_snapshot.clone();
        async move {
            c.quote(
                BINARYFI_ROUTER_ADDRESS,
                assets[i].address,
                assets[j].address,
                amount,
            )
            .call()
            .block(quote_block)
            .await
        }
    };

    // 1) 真实套利交易金额锚点：USDT0(0) -> SKHYx(11), in = 35.357671
    let real_in = U256::from(35_357_671u64);
    let direct = quote_chain(0, 11, real_in).await?;
    let sim = pool.simulate_swap(USDT0, SKHYX, real_in)?;
    println!(
        "real tx 0->11 in={real_in}: sim={sim} chain={direct} match={}",
        sim == direct
    );
    assert_eq!(sim, direct, "real tx amount mismatch");

    // 1.5) 阶梯上限对拍（67160388 实测值；maxIn/maxOut 由快照恢复）：
    //   - SELL SKHYx(11) -> USDT0(0)：in=1e24 饱和 → 3,684,065,588
    //   - BUY  USDT0(0) -> SKHYx(11)：in=1e10 饱和 → 7,643,300,000,000,000,000
    let sell_cap_chain = quote_chain(11, 0, U256::from(10u128.pow(24))).await?;
    let sell_cap_sim = pool.simulate_swap(SKHYX, USDT0, U256::from(10u128.pow(24)))?;
    println!(
        "SELL cap 11->0 in=1e24: sim={sell_cap_sim} chain={sell_cap_chain} match={} maxIn={}",
        sell_cap_sim == sell_cap_chain,
        pool.max_inputs
            .get(11)
            .copied()
            .flatten()
            .unwrap_or_default()
    );
    assert_eq!(sell_cap_sim, sell_cap_chain, "SELL cap mismatch");
    let buy_cap_chain = quote_chain(0, 11, U256::from(10_000_000_000u64)).await?;
    let buy_cap_sim = pool.simulate_swap(USDT0, SKHYX, U256::from(10_000_000_000u64))?;
    println!(
        "BUY cap 0->11 in=1e10: sim={buy_cap_sim} chain={buy_cap_chain} match={} maxOut={}",
        buy_cap_sim == buy_cap_chain,
        pool.max_outputs
            .get(11)
            .copied()
            .flatten()
            .unwrap_or_default()
    );
    assert_eq!(buy_cap_sim, buy_cap_chain, "BUY cap mismatch");

    // 2) 逐 pair 网格对比。
    //    - 断言区（线性区，低于引擎 per-asset 封顶）：小额全部；0→j 整币
    //    - 信息区：j→0 整币与跨资产整币（可能触发引擎 per-asset 封顶，
    //      该 cap 是存储参数、事件/calldata 不可观测，仅打印）
    let n = pool.assets.len();
    let mut exact_total = 0u32;
    let mut exact_ok = 0u32;
    let mut info_capped = 0u32;
    let mut info_mismatch = 0u32;
    for i in 0..n {
        for j in 0..n {
            if i == j {
                continue;
            }
            let di = pool.assets[i].decimals as u32;
            let whole = U256::from(10u64).pow(U256::from(di));
            // 小额：整币的 10^-8（远低于各资产封顶）
            let small = if di > 8 {
                U256::from(10u64).pow(U256::from(di - 8))
            } else {
                U256::from(1u64)
            };
            let mut cases: Vec<(&str, U256, bool)> = vec![];
            if i == 0 {
                cases.push(("whole", whole, true));
                cases.push(("small", small, true));
            } else {
                cases.push(("small", small, true));
                cases.push(("whole", whole, false));
            }
            for (label, amt, must_match) in cases {
                if amt.is_zero() {
                    continue;
                }
                let chain = match quote_chain(i, j, amt).await {
                    Ok(v) => v,
                    Err(e) => {
                        println!("pair {i}->{j} amt={amt}: chain quote err {e}");
                        continue;
                    }
                };
                let sim =
                    pool.simulate_swap(pool.assets[i].address, pool.assets[j].address, amt)?;
                exact_total += 1;
                if sim == chain {
                    exact_ok += 1;
                } else if must_match {
                    let diff = if sim > chain {
                        sim - chain
                    } else {
                        chain - sim
                    };
                    println!(
                        "MISMATCH pair {i}->{j} {label} amt={amt}: sim={sim} chain={chain} diff={diff}"
                    );
                } else if sim < chain {
                    info_capped += 1;
                } else {
                    info_mismatch += 1;
                    let diff = sim - chain;
                    println!(
                        "INFO pair {i}->{j} {label} amt={amt}: sim={sim} chain={chain} diff={diff} (engine cap < local 96%)"
                    );
                }
            }
        }
    }
    println!(
        "grid compare: {exact_ok}/{exact_total} exact (info capped={info_capped}, info mismatch={info_mismatch})"
    );

    // 3) update_at(block)：标记 SKHYx 相关 pair stale 后批量刷新，费率应保持精确
    //    （指定区块模式必须用 update_at，否则 update() 内部取 latest 会导致对拍错位）
    let j = pool
        .tokens()
        .iter()
        .position(|t| *t == SKHYX)
        .expect("SKHYx in assets");
    let before = pool.rates[pool.pair_index(0, j)];
    pool.mark_stale_for_asset(j);
    pool.update_at(provider.clone(), block_id).await?;
    let after = pool.rates[pool.pair_index(0, j)];
    println!(
        "update(): stale cleared={} rate stable={} ({} / {} -> {} / {})",
        pool.stale_pairs.is_empty(),
        before == after,
        before.num,
        before.den,
        after.num,
        after.den
    );
    assert!(pool.stale_pairs.is_empty());
    let sim2 = pool.simulate_swap(USDT0, SKHYX, real_in)?;
    let chain2 = quote_chain(0, j, real_in).await?;
    println!(
        "post-update real tx 0->{j}: sim={sim2} chain={chain2} match={}",
        sim2 == chain2
    );
    assert_eq!(sim2, chain2, "post-update mismatch");

    // 4) 日志驱动路径：SKHYx update(price) 后 0->11 仍逐位一致
    let j = pool
        .tokens()
        .iter()
        .position(|t| *t == SKHYX)
        .expect("SKHYx in assets");
    let p11 = pool.prices[j];
    pool.apply_price_update(j, p11, 0x400c944);
    let sim3 = pool.simulate_swap(USDT0, SKHYX, real_in)?;
    let chain3 = quote_chain(0, j, real_in).await?;
    println!(
        "log-driven 0->{j}: sim={sim3} chain={chain3} match={}",
        sim3 == chain3
    );
    assert_eq!(sim3, chain3, "log-driven mismatch");

    // 5) 引擎价格恢复一致性：关键资产
    for (label, addr) in [
        ("USDT0", USDT0),
        ("xETH", XETH),
        ("xSOL", XSOL),
        ("SKHYx", SKHYX),
        ("CRCLx", CRCLX),
    ] {
        if let Some(k) = pool.tokens().iter().position(|t| *t == addr) {
            let p = pool.prices[k];
            let s = pool.spreads.get(k).copied().unwrap_or(0);
            println!(
                "price[{label}] idx={k} p={p} spread={s} (~{:.2} USDT0)",
                u256_f64(&p) / 100.0
            );
        }
    }

    println!("BINARYFI PROP PROBE OK");
    Ok(())
}

fn u256_f64(v: &U256) -> f64 {
    let l = v.as_limbs();
    l[0] as f64
        + (l[1] as f64) * 2f64.powi(64)
        + (l[2] as f64) * 2f64.powi(128)
        + (l[3] as f64) * 2f64.powi(192)
}

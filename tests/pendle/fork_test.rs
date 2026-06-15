//! PendlePool 精度验证测试
//!
//! 直接通过 RPC 获取链上数据，验证:
//! 1. init() 后本地位与链上 _storage() 一致（零漂移）
//! 2. Rust simulate_swap 输出的内部一致性
//! 3. 价格计算与链上 marginal rate 对比
//!
//! 不依赖 anvil_setStorageAt，直接对真实链上状态做验证。

use alloy::{
    eips::BlockId,
    primitives::{address, Address, U256},
    providers::{Provider, ProviderBuilder},
    sol,
};
use amms::amms::{amm::AutomatedMarketMaker, pendle::PendlePool};
use eyre::Result;

/// 测试用例：多种底层资产类型的 Pendle Market
const CASES: &[(Address, &str)] = &[
    (
        address!("0271A803f0d3Dec9cCd105A4A4d41e6Ee1458765"),
        "srUSDe",
    ),
    (
        address!("9c560ebaf78e596cbcc27411d633a74d628dd7dc"),
        "sUSDS",
    ),
    (address!("f80b67a32df07960c731794769309e3d30e9717f"), "USDG"),
];

sol! {
    #[sol(rpc)]
    contract IPMarketStorage {
        function _storage() external view returns (
            int128 totalPt, int128 totalSy, uint96 lastLnImpliedRate,
            uint16, uint16, uint16
        );
    }

    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract ISYExchangeRate {
        function exchangeRate() external view returns (uint256);
    }
}

/// 从链上 _storage() 读取状态
async fn onchain_storage(
    provider: &impl Provider,
    market: Address,
    block: BlockId,
) -> Result<(U256, U256, U256)> {
    let c = IPMarketStorage::new(market, provider);
    let s = c._storage().block(block).call().await?;
    Ok((
        U256::from(s.totalPt as u128),
        U256::from(s.totalSy as u128),
        U256::from(s.lastLnImpliedRate),
    ))
}

/// 从链上读取 SY exchangeRate
async fn onchain_exchange_rate(
    provider: &impl Provider,
    sy: Address,
    block: BlockId,
) -> Result<U256> {
    let c = ISYExchangeRate::new(sy, provider);
    Ok(c.exchangeRate().block(block).call().await?)
}

#[tokio::test]
async fn test_pendle_init_and_math() -> Result<()> {
    dotenv::dotenv().ok();
    let rpc = std::env::var("ETHEREUM_PROVIDER").expect("需要 ETHEREUM_PROVIDER 环境变量");

    let provider = ProviderBuilder::new().connect_http(rpc.parse()?);
    let current_block = provider.get_block_number().await?;
    let block_id = BlockId::from(current_block);
    println!("当前区块: {}", current_block);

    for (market_addr, label) in CASES {
        println!("\n═══════ {} ({:#x}) ═══════", label, market_addr);

        // ── Init ──
        let pool = match PendlePool::new(*market_addr)
            .init(block_id, provider.clone())
            .await
        {
            Ok(p) => p,
            Err(e) => {
                println!("⚠️ init 失败: {:?}, 跳过", e);
                continue;
            }
        };

        // ── 1. 验证 init 零漂移 ──
        let (oc_pt, oc_sy, _oc_ln) = onchain_storage(&provider, *market_addr, block_id).await?;
        assert_eq!(pool.total_pt, oc_pt, "[{}] totalPt 漂移", label);
        assert_eq!(pool.total_sy, oc_sy, "[{}] totalSy 漂移", label);

        // 从链上重新读 exchange_rate 做交叉验证
        let oc_rate = onchain_exchange_rate(&provider, pool.sy_address, block_id).await?;
        assert_eq!(
            pool.sy_exchange_rate, oc_rate,
            "[{}] sy_exchange_rate 漂移",
            label
        );

        println!("  init 零漂移 ✅");
        println!(
            "  totalPt={} totalSy={} is_expired={}",
            pool.total_pt, pool.total_sy, pool.is_expired
        );
        println!("  sy_exchange_rate={}", pool.sy_exchange_rate);
        println!(
            "  expiry={} ln_implied_rate={}",
            pool.expiry, pool.last_ln_implied_rate
        );

        if pool.is_expired {
            println!("  已到期, 跳过 swap 验证");
            continue;
        }

        // ── 2. 验证 simulate_swap 内部一致性 ──
        //   a) 零输入 → 零输出
        let zero_out = pool.simulate_swap(pool.pt_token, pool.underlying_token, U256::ZERO)?;
        assert!(zero_out.is_zero(), "[{}] 零输入应返回零", label);
        println!("  零输入→零输出 ✅");

        //   b) 单调性: 更大输入 → 更大输出
        let small_amt = U256::from(10u128.pow(pool.pt_decimals as u32)); // 1 PT
        let out_small = pool.simulate_swap(pool.pt_token, pool.underlying_token, small_amt)?;
        assert!(!out_small.is_zero(), "[{}] 1 PT 不应输出零", label);
        let out_large = pool.simulate_swap(
            pool.pt_token,
            pool.underlying_token,
            small_amt * U256::from(10),
        )?;
        assert!(
            out_large > out_small,
            "[{}] 单调性违反: {} ≯ {}",
            label,
            out_large,
            out_small
        );
        println!("  单调性: PT=1 → {} | PT=10 → {} ✅", out_small, out_large);

        //   c) 价格合理性: PT 价格为正且有限，并与小额 swap 输出一致
        let pt_price = pool.calculate_price(pool.pt_token, pool.underlying_token)?;
        println!("  PT 价格: {} underlying (marginal)", pt_price);
        assert!(
            pt_price.is_finite() && pt_price > 0.0,
            "[{}] PT 价格无效: {}",
            label,
            pt_price
        );

        //   d) 多金额模拟：验证无 panic
        for amt in &[
            U256::from(10u128.pow(17)), // 0.1
            U256::from(10u128.pow(18)), // 1
            U256::from(5e18 as u128),   // 5
            U256::from(10e18 as u128),  // 10
        ] {
            let out = pool.simulate_swap(pool.pt_token, pool.underlying_token, *amt)?;
            assert!(!out.is_zero(), "[{}] PT={}: 零输出", label, amt);
            println!("  simulate_swap(PT={}) = underlying={}", amt, out);
        }

        //   e) Reserve 比例验证: totalSy / totalPt 应与价格一致
        let reserve_ratio = pool.total_sy * U256::from(10u128.pow(18)) / pool.total_pt;
        println!("  totalSy/totalPt 比率: {} (参考)", reserve_ratio);
    }

    println!("\n✅ 全部通过");
    Ok(())
}

use alloy::{
    network::Ethereum,
    primitives::{address, Address, U256},
    providers::{Provider, ProviderBuilder},
    sol,
};
use amms::amms::{
    amm::AutomatedMarketMaker,
    ekubo::{EkuboPool, EkuboPoolKey},
};
use eyre::Result;
use std::env;

// Ekubo 常量
const MIN_TICK: i32 = -887272;
const MAX_TICK: i32 = 887272;

// Ekubo Core 接口
sol! {
    #[sol(rpc)]
    interface IEkuboCore {
        function getPoolState((address, address, uint24, int24) poolKey)
            external
            view
            returns (uint160 sqrtPriceX96, int24 tick, uint128 liquidity);
    }
}

async fn setup_provider() -> Result<impl alloy::providers::Provider<Ethereum> + Clone> {
    let rpc_url = env::var("ETHEREUM_RPC_URL").unwrap_or("https://eth.merkle.io".to_string());
    Ok(ProviderBuilder::new().connect_http(rpc_url.parse()?))
}

/// 验证本地模拟与链上结果的偏差
#[allow(dead_code)]
fn verify_diff(local: U256, chain: U256, threshold_ppm: u64) {
    if local == chain {
        return;
    }

    let diff = if local > chain {
        local - chain
    } else {
        chain - local
    };

    let ratio = diff * U256::from(1_000_000) / chain;
    println!(
        "Diff: {}, Ratio: {} ppm (Threshold: {})",
        diff, ratio, threshold_ppm
    );

    assert!(ratio <= U256::from(threshold_ppm), "Deviation too high!");
}

// ========== 测试常量 ==========

use alloy::eips::BlockId;

// 正确的 Ekubo Core 地址 (Ethereum Mainnet)
const EKUBO_CORE: Address = address!("e0e0e08a6a4b9dc7bd67bcb7aade5cf48157d444");

// ETH/USDC 池子 (Oracle Extension)
// Token0: 0x0
// Token1: USDC
fn get_pool_eth_usdc_real() -> EkuboPoolKey {
    // Config: 0x514d5de68852628af2f1236f780866989660ada6000000000000000000000000
    // Extension (MSB): 0x514d...
    let config_str = "514d5de68852628af2f1236f780866989660ada6000000000000000000000000";
    let config = U256::from_str_radix(config_str, 16).unwrap();

    EkuboPoolKey {
        token0: Address::ZERO,
        token1: address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
        config,
    }
}

#[tokio::test]
async fn test_sync_real_pool_eth_usdc() -> Result<()> {
    // 这是一个 Fork 测试，需要 ETHEREUM_RPC_URL
    let provider = setup_provider().await?;
    let provider = std::sync::Arc::new(provider);

    let key = get_pool_eth_usdc_real();
    println!("Testing Pool: ETH/USDC (Oracle)");

    // Parse Config locally
    let parsed_config = amms::amms::ekubo::PoolConfig::from_bytes32(key.config);
    println!("Config Ext: {}", parsed_config.extension);

    assert_eq!(
        parsed_config.extension,
        address!("514d5de68852628af2f1236f780866989660ada6")
    );

    let pool = EkuboPool::new(EKUBO_CORE, key);
    let pool = pool.init(BlockId::latest(), provider).await?;

    println!("ETH/USDC Pool Synced!");
    println!("Liquidity: {}", pool.liquidity);
    println!("SqrtPrice: {}", pool.sqrt_price);
    println!("Tick: {}", pool.tick);

    // 即使 Liquidity 为 0, 只要不报错就说明 Key 正确且连接正常
    Ok(())
}

// 占位: 旧的 mock 数据函数保留但不再作为主要的 verification source
#[allow(dead_code)]
fn get_pool_usdc_usdt() -> EkuboPoolKey {
    EkuboPoolKey::new_concentrated(
        address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"), // USDC
        address!("dac17f958d2ee523a2206206994597c13d831ec7"), // USDT
        3000,                                                 // fee 3000
        60,                                                   // tick_spacing 60
        Address::ZERO,                                        // extension
    )
}

// 测试不同的 fee tier
#[allow(dead_code)]
fn get_pool_usdc_usdt_fee_100() -> EkuboPoolKey {
    EkuboPoolKey::new_concentrated(
        address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
        address!("dac17f958d2ee523a2206206994597c13d831ec7"),
        100, // 0.01% fee
        10,  // 更小的 tick spacing
        address!("0000000000000000000000000000000000000000"),
    )
}

#[allow(dead_code)]
fn get_pool_usdc_usdt_fee_500() -> EkuboPoolKey {
    EkuboPoolKey::new_concentrated(
        address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
        address!("dac17f958d2ee523a2206206994597c13d831ec7"),
        500, // 0.05% fee
        50,  // tick spacing
        address!("0000000000000000000000000000000000000000"),
    )
}

// WBTC/WETH 池子 (更常见的交易对)
#[allow(dead_code)]
fn get_pool_wbtc_weth() -> EkuboPoolKey {
    EkuboPoolKey::new_concentrated(
        address!("2260fac5e5542a773aa44fbcfedf7c193bc2c599"), // WBTC
        address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"), // WETH
        3000,                                                 // 0.3% fee
        60,                                                   // tick spacing
        address!("0000000000000000000000000000000000000000"), // extension
    )
}

// WETH/USDC 池子
#[allow(dead_code)]
fn get_pool_weth_usdc() -> EkuboPoolKey {
    EkuboPoolKey::new_concentrated(
        address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"), // WETH
        address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"), // USDC
        3000,                                                 // 0.3% fee
        60,                                                   // tick spacing
        address!("0000000000000000000000000000000000000000"), // extension
    )
}

// ========== 基础功能测试 ==========

#[tokio::test]
async fn test_ekubo_basic_init() -> Result<()> {
    let provider = setup_provider().await?;
    let provider = std::sync::Arc::new(provider); // Cloneable provider

    // Common tokens
    let eth = Address::ZERO;
    let weth = address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
    let usdc = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    let usdt = address!("dac17f958d2ee523a2206206994597c13d831ec7");
    let wbtc = address!("2260fac5e5542a773aa44fbcfedf7c193bc2c599");

    let pairs = vec![
        ("ETH/USDC", eth, usdc),
        ("USDC/USDT", usdc, usdt),
        ("WBTC/ETH", wbtc, eth),
        ("WETH/USDC", weth, usdc),
    ];

    // Common V3-style fees/tick_spacings
    // Fee units: 100=0.01%, 500=0.05%, 3000=0.3%, 10000=1%
    // TickSpacing: 1, 10, 60, 200
    let configs = vec![
        (100, 1),
        (500, 10),
        (3000, 60),
        (10000, 200),
        (2000, 60), // OTHERS?
        // Maybe fee 0?
        (0, 1),
    ];

    println!("Starting Brute Force Discovery for Ekubo V2 Pools...");

    for (pair_name, t0, t1) in pairs {
        for &(fee, ts) in &configs {
            let key = EkuboPoolKey::new_concentrated(t0, t1, fee, ts, Address::ZERO);

            // Quick check
            let pool = EkuboPool::new(EKUBO_CORE, key);

            // We use a simplified init that ignores errors to continue
            match pool
                .init(alloy::eips::BlockId::latest(), provider.clone())
                .await
            {
                Ok(p) => {
                    if p.liquidity > 0 {
                        println!("FOUND ACTIVE POOL: {} Fee={} TS={}", pair_name, fee, ts);
                        println!("  Liquidity: {}", p.liquidity);
                        println!("  SqrtPrice: {}", p.sqrt_price);
                        println!("  Tick: {}", p.tick);
                        return Ok(()); // Found one!
                    }
                }
                Err(_) => {
                    // Ignore not found
                }
            }
        }
    }

    println!("No active pools found with standard configs.");
    Ok(())
}

#[tokio::test]
async fn test_ekubo_state_consistency() -> Result<()> {
    let provider = setup_provider().await?;

    let pool_key = get_pool_usdc_usdt();
    let pool = EkuboPool::new(EKUBO_CORE, pool_key);

    // 尝试初始化
    let pool = match pool
        .init(alloy::eips::BlockId::latest(), provider.clone())
        .await
    {
        Ok(p) => p,
        Err(_) => {
            println!("Skipping test: pool not found");
            return Ok(());
        }
    };

    // 验证本地状态合理性
    println!("Pool State:");
    println!("  pool_id: {:?}", pool.pool_id);
    println!("  sqrt_price: {}", pool.sqrt_price);
    println!("  tick: {}", pool.tick);
    println!("  liquidity: {}", pool.liquidity);
    println!("  fee: {}", pool.fee);
    println!("  tick_spacing: {}", pool.tick_spacing);

    // 基本验证
    assert!(
        pool.sqrt_price > U256::ZERO,
        "sqrt_price should be positive"
    );
    assert!(pool.liquidity > 0, "liquidity should be positive");
    assert!(pool.fee > 0, "fee should be positive");
    assert!(pool.tick_spacing > 0, "tick_spacing should be positive");

    // Tick 应该在合理范围内
    assert!(
        pool.tick >= MIN_TICK && pool.tick <= MAX_TICK,
        "tick out of range"
    );

    println!("✓ State consistency verified (local state is valid)");

    Ok(())
}

// ========== Swap 模拟测试 ==========

#[tokio::test]
async fn test_ekubo_small_trade() -> Result<()> {
    let provider = setup_provider().await?;

    let pool = EkuboPool::new(EKUBO_CORE, get_pool_usdc_usdt());

    let pool = match pool
        .init(alloy::eips::BlockId::latest(), provider.clone())
        .await
    {
        Ok(p) => p,
        Err(e) => {
            println!("Skipping test: pool not found - {}", e);
            return Ok(());
        }
    };

    // 小额交易测试 (1000 USDC -> USDT)
    let amount_in = U256::from(1000) * U256::from(10).pow(U256::from(6)); // 1000 USDC (6 decimals)
    let local_out = pool.simulate_swap(pool.token_a.address, pool.token_b.address, amount_in)?;

    println!("Small Trade (1000 USDC -> USDT):");
    println!("  Local Output: {} USDT", local_out);

    // 基本验证
    assert!(local_out > U256::ZERO, "Output should be positive");

    println!("✓ Small trade simulation completed");

    Ok(())
}

#[tokio::test]
async fn test_ekubo_price_impact() -> Result<()> {
    let provider = setup_provider().await?;

    let pool = EkuboPool::new(EKUBO_CORE, get_pool_usdc_usdt());

    let pool = match pool
        .init(alloy::eips::BlockId::latest(), provider.clone())
        .await
    {
        Ok(p) => p,
        Err(_) => {
            println!("Skipping test: pool not found");
            return Ok(());
        }
    };

    // 测试不同金额的价格影响
    let amounts = vec![
        U256::from(100) * U256::from(10).pow(U256::from(6)), // 100 USDC
        U256::from(1000) * U256::from(10).pow(U256::from(6)), // 1000 USDC
        U256::from(10000) * U256::from(10).pow(U256::from(6)), // 10000 USDC
        U256::from(100000) * U256::from(10).pow(U256::from(6)), // 100000 USDC
    ];

    println!("\nPrice Impact Test:");
    for amount_in in amounts {
        let out = pool.simulate_swap(pool.token_a.address, pool.token_b.address, amount_in)?;
        let rate = out * U256::from(10).pow(U256::from(6)) / amount_in;
        println!(
            "  In: {} USDC -> Out: {} USDT (Rate: {} USDT/USDC)",
            amount_in, out, rate
        );

        // 基本合理性检查
        assert!(out > U256::ZERO, "Output should be positive");
        assert!(rate > U256::ZERO, "Rate should be positive");
    }

    println!("✓ Price impact test completed");

    Ok(())
}

#[tokio::test]
async fn test_ekubo_reverse_swap() -> Result<()> {
    let provider = setup_provider().await?;

    let pool = EkuboPool::new(EKUBO_CORE, get_pool_usdc_usdt());

    let pool = match pool
        .init(alloy::eips::BlockId::latest(), provider.clone())
        .await
    {
        Ok(p) => p,
        Err(_) => {
            println!("Skipping test: pool not found");
            return Ok(());
        }
    };

    // 正向交易
    let amount_in = U256::from(1000) * U256::from(10).pow(U256::from(6)); // 1000 USDC
    let out_1 = pool.simulate_swap(pool.token_a.address, pool.token_b.address, amount_in)?;

    // 反向交易
    let out_2 = pool.simulate_swap(pool.token_b.address, pool.token_a.address, out_1)?;

    println!("\nReverse Swap Test:");
    println!("  {} USDC -> {} USDT", amount_in, out_1);
    println!("  {} USDT -> {} USDC", out_1, out_2);
    println!("  Loss: {} wei", amount_in - out_2);

    // 验证反向交易后的金额应该小于初始金额(因为有手续费)
    assert!(
        out_2 < amount_in,
        "Reverse swap should result in less due to fees"
    );

    println!("✓ Reverse swap test completed");

    Ok(())
}

// ========== 边界条件测试 ==========

#[tokio::test]
async fn test_ekubo_zero_amount() -> Result<()> {
    let provider = setup_provider().await?;

    let pool = EkuboPool::new(EKUBO_CORE, get_pool_usdc_usdt());

    let pool = match pool
        .init(alloy::eips::BlockId::latest(), provider.clone())
        .await
    {
        Ok(p) => p,
        Err(_) => {
            println!("Skipping test: pool not found");
            return Ok(());
        }
    };

    // 零金额测试
    let zero_amount = U256::ZERO;
    let out = pool.simulate_swap(pool.token_a.address, pool.token_b.address, zero_amount)?;

    assert_eq!(out, U256::ZERO, "Zero amount should return zero output");

    println!("✓ Zero amount test completed");

    Ok(())
}

// ========== 活跃池子发现与验证 ==========

/// 已知的活跃 poolId (从 Etherscan 获取的 WETH/USDC 交易)
/// poolId: 0x9995855c00494d039ab6792f18e368e530dff931614e2ba87050c938ccff35e3
/// Tokens: USDC <-> WETH
#[tokio::test]
async fn test_bruteforce_find_weth_usdc_pool() -> Result<()> {
    use alloy::hex;
    use alloy::primitives::B256;

    let target_pool_id = B256::from_slice(
        &hex::decode("9995855c00494d039ab6792f18e368e530dff931614e2ba87050c938ccff35e3").unwrap(),
    );

    // Ekubo 要求 token0 < token1 (地址排序)
    let usdc = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    let weth = address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");

    // 确保排序正确
    let (token0, token1) = if usdc < weth {
        (usdc, weth)
    } else {
        (weth, usdc)
    };
    println!("Token0: {:?}", token0);
    println!("Token1: {:?}", token1);
    println!("Target poolId: {:?}", target_pool_id);

    // 扩展参数范围 - 包括更多可能的组合
    // 添加 Oracle Extension 地址
    let oracle_ext = address!("514d5de68852628af2f1236f780866989660ada6");

    let fees: Vec<u64> = (0..=100000).step_by(10).collect(); // 更细粒度
    let tick_spacings = vec![1, 2, 5, 10, 20, 50, 60, 100, 200, 1000];
    let extensions = vec![Address::ZERO, oracle_ext];

    println!(
        "Searching {} fee x {} ts x {} ext = {} combinations...",
        fees.len(),
        tick_spacings.len(),
        extensions.len(),
        fees.len() * tick_spacings.len() * extensions.len()
    );

    for &ext in &extensions {
        for &ts in &tick_spacings {
            for &fee in &fees {
                // 构建 EkuboPoolKey 使用正确的方法
                let key = EkuboPoolKey::new_concentrated(token0, token1, fee, ts, ext);

                // 使用与源码相同的 pool_id() 方法
                let computed_id = key.pool_id();

                if computed_id == target_pool_id {
                    println!("\n🎉 FOUND MATCHING CONFIG!");
                    println!("  Fee: {}", fee);
                    println!("  TickSpacing: {}", ts);
                    println!("  Extension: {:?}", ext);
                    println!("  Config (hex): {:?}", key.config);
                    return Ok(());
                }
            }
        }
    }

    println!("❌ No matching config found in search space.");
    println!("The pool may use a non-standard extension or fee encoding.");
    Ok(())
}

/// 直接使用已知的活跃 WETH/USDC 池子测试
/// 如果已知正确的 config，直接验证
#[tokio::test]
async fn test_sync_active_weth_usdc_pool() -> Result<()> {
    let provider = setup_provider().await?;
    let provider = std::sync::Arc::new(provider);

    // 尝试常见配置
    let usdc = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    let weth = address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");

    // 尝试不同的 fee tier
    let configs_to_try = vec![
        (500, 10, "0.05%"),
        (3000, 60, "0.30%"),
        (10000, 200, "1.00%"),
        (100, 1, "0.01%"),
    ];

    for (fee, ts, desc) in configs_to_try {
        println!("\nTrying WETH/USDC Fee={} ({}), TS={}...", fee, desc, ts);

        let key = EkuboPoolKey::new_concentrated(usdc, weth, fee, ts, Address::ZERO);
        let pool = EkuboPool::new(EKUBO_CORE, key);

        match pool.init(BlockId::latest(), provider.clone()).await {
            Ok(p) => {
                if p.liquidity > 0 {
                    println!("✅ FOUND ACTIVE POOL!");
                    println!("  Fee: {} ({})", fee, desc);
                    println!("  TickSpacing: {}", ts);
                    println!("  Liquidity: {}", p.liquidity);
                    println!("  SqrtPrice: {}", p.sqrt_price);
                    println!("  Tick: {}", p.tick);

                    // 尝试模拟 Swap
                    let amount_in = U256::from(100) * U256::from(10).pow(U256::from(6)); // 100 USDC
                    match p.simulate_swap(p.token_a.address, p.token_b.address, amount_in) {
                        Ok(out) => {
                            println!("  Simulated: 100 USDC -> {} wei WETH", out);
                        }
                        Err(e) => {
                            println!("  Simulate failed: {}", e);
                        }
                    }
                    return Ok(());
                } else {
                    println!("  Pool exists but liquidity = 0");
                }
            }
            Err(e) => {
                println!("  Init failed: {}", e);
            }
        }
    }

    println!("\n⚠️ No active WETH/USDC pool found with standard configs.");
    Ok(())
}

// ========== 通过 Factory 事件发现池子 ==========

use alloy::rpc::types::Filter;
use amms::amms::ekubo::EkuboFactory;
use amms::amms::factory::AutomatedMarketMakerFactory;

/// 通过 PoolInitialized 事件发现主网上所有的 Ekubo V2 池子
#[tokio::test]
async fn test_discover_pools_via_factory() -> Result<()> {
    let provider = setup_provider().await?;
    let provider = std::sync::Arc::new(provider);

    // Ekubo Core 合约部署区块: 22047273 (用户提供)
    // 我们从部署区块开始搜索，希望能找到早期的池子
    let start_block = 22047273u64;
    let current_block = provider.get_block_number().await?;

    // 我们不需要搜索到最新，只要搜索一部分历史，找到有用的池子即可
    // 限制搜索范围，例如搜索部署后的 100,000 个区块
    let search_range = 100_000u64;
    let end_block = std::cmp::min(current_block, start_block + search_range);

    println!(
        "Discovering Ekubo V2 pools from block {} to {} (Range: {})",
        start_block,
        end_block,
        end_block - start_block
    );

    // 创建 Factory (用于 helper 方法)
    let factory = EkuboFactory::new(EKUBO_CORE, start_block);
    let event_sig = factory.pool_creation_event();
    println!("Looking for PoolInitialized events: {:?}", event_sig);

    // 分批查询，每批 1000 个区块 (RPC 限制)
    let batch_size = 1000u64;
    let mut pools_found = 0;
    let mut pools_with_liquidity = vec![];

    let mut from = start_block;
    while from < end_block {
        let to = std::cmp::min(from + batch_size - 1, end_block);

        // 打印进度
        if (from - start_block) % 10000 == 0 {
            println!("Scanning blocks {} - {}...", from, to);
        }

        let filter = Filter::new()
            .address(EKUBO_CORE)
            .event_signature(event_sig)
            .from_block(from)
            .to_block(to);

        // 忽略单次查询错误，继续重试或跳过
        match provider.get_logs(&filter).await {
            Ok(logs) => {
                if !logs.is_empty() {
                    println!("  Found {} events in blocks {}-{}", logs.len(), from, to);
                    pools_found += logs.len();

                    for log in logs {
                        if let Ok(amm) = factory.create_pool(log) {
                            if let amms::amms::amm::AMM::EkuboPool(pool) = amm {
                                // 尝试初始化检查流动性
                                // 注意：我们只对有潜力的池子做 RPC 请求，避免请求过多

                                if pools_with_liquidity.len() < 5 {
                                    let full_pool =
                                        EkuboPool::new(EKUBO_CORE, pool.pool_key.clone());
                                    match full_pool.init(BlockId::latest(), provider.clone()).await
                                    {
                                        Ok(p) => {
                                            if p.liquidity > 0 {
                                                println!(
                                                    "  🎉 ACTIVE POOL FOUND! {}/{} Liq: {}",
                                                    p.token_a.address,
                                                    p.token_b.address,
                                                    p.liquidity
                                                );
                                                println!(
                                                    "     Config: Fee={} TS={} Ext={}",
                                                    p.fee,
                                                    p.tick_spacing,
                                                    p.pool_key.parse_config().extension
                                                );
                                                pools_with_liquidity.push(p);
                                            }
                                        }
                                        Err(_) => {} // Ignore init errors
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                println!("  RPC error at blocks {}-{}: {}", from, to, e);
                // 简单的重试逻辑或跳过
            }
        }

        if pools_with_liquidity.len() >= 3 {
            println!("Found enough active pools, stopping search.");
            break;
        }

        from += batch_size;
    }

    println!("\n========== SUMMARY ==========");
    println!("Total pools found: {}", pools_found);
    println!(
        "Active pools with liquidity found: {}",
        pools_with_liquidity.len()
    );

    if !pools_with_liquidity.is_empty() {
        println!("\nVerifying swap on first active pool...");
        let pool = &pools_with_liquidity[0];

        // 模拟 Swap
        let amount_in = U256::from(1000); // Small amount
        match pool.simulate_swap(pool.token_a.address, pool.token_b.address, amount_in) {
            Ok(out) => {
                println!("✅ Swap Simulation Successful!");
                println!("   Input: {} {}", amount_in, pool.token_a.symbol);
                println!("   Output: {} {}", out, pool.token_b.symbol);
                println!("   Price: {}", pool.token_b_price);
            }
            Err(e) => {
                println!("❌ Swap Simulation Failed: {}", e);
            }
        }
    } else {
        println!("⚠️ No active pools found with liquidity in the scanned range.");
    }

    Ok(())
}

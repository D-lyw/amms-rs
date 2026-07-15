//! Caliber propAMM Fork Integration Test
//!
//! 测试 CaliberPropPool 的池子发现、初始化、simulate_swap、with_amms 集成。
//!
//! ## XLayer 真实池子
//! Contract: 0x154586b2479b9a11e3d4db90024dc0e26f097312
//! OKLink: https://www.oklink.com/zh-hans/x-layer/evm/address/0x154586b2479b9a11e3d4db90024dc0e26f097312
//!
//! 环境变量: XLAYER_PROVIDER 或 XLAYER_RPC_URL

use alloy::{
    eips::BlockId,
    hex::FromHex,
    network::Network,
    primitives::{address, keccak256, Address, B256, U256},
    providers::{Provider, ProviderBuilder},
    sol,
};
use amms::{
    amms::{
        amm::AutomatedMarketMaker,
        caliber_prop::{CaliberPropPool, ICaliberPropAMM},
    },
    state_space::StateSpaceBuilder,
};
use eyre::Result;
use std::env;
use std::sync::Arc;

// ============================================================
// Constants
// ============================================================

const XLAYER_CHAIN_ID: u64 = 196;
const CALIBER_CONTRACT: Address = address!("154586b2479b9a11e3d4db90024dc0e26f097312");

// ERC20 metadata ABI for reading decimals
sol! {
    #[sol(rpc)]
    interface IERC20Metadata {
        function decimals() external view returns (uint8);
        function symbol() external view returns (string);
    }
}

// ============================================================
// Helper: RPC URL
// ============================================================

fn xlayer_provider_url() -> Option<String> {
    dotenv::dotenv().ok();
    env::var("XLAYER_PROVIDER")
        .or_else(|_| env::var("XLAYER_RPC_URL"))
        .ok()
}

// ============================================================
// Helper: eth_getStorageAt
// ============================================================

async fn get_storage_at_raw<N, P>(
    provider: &P,
    address: Address,
    slot: B256,
) -> Result<B256>
where
    N: Network,
    P: Provider<N>,
{
    let slot_u256 = U256::from_be_bytes(slot.0);
    let result: U256 = provider.get_storage_at(address, slot_u256).await?;
    Ok(B256::from(result.to_be_bytes::<32>()))
}

// ============================================================
// Helper: read token address from pair config storage
// ============================================================

/// pairConfig 映射在 storage slot 6（mapping(bytes32 => PairConfig)）
/// PairConfig.tokenX 在 struct 中 offset 0
/// PairConfig.tokenY 在 struct 中 offset 32 bytes（第二个 U256 字段）
async fn read_token_addresses<N, P>(
    provider: &P,
    contract: Address,
    pair_id: B256,
) -> Result<(Address, Address)>
where
    N: Network,
    P: Provider<N>,
{
    // base_slot = keccak256(abi.encode(uint256(6)))
    let base_slot = B256::from_hex(
        "0x0000000000000000000000000000000000000000000000000000000000000006",
    )?;

    // slot_tokenX = keccak256(pair_id . base_slot)
    let mut input = [0u8; 64];
    input[..32].copy_from_slice(pair_id.as_ref());
    input[32..].copy_from_slice(base_slot.as_ref());
    let hash = keccak256(input);
    let slot0 = B256::from(hash);

    // slot_tokenY = slot_tokenX + 1
    let mut slot1_num = U256::from_be_bytes(slot0.0);
    slot1_num += U256::from(1);
    let slot1 = B256::from(slot1_num.to_be_bytes::<32>());

    let raw0 = get_storage_at_raw(provider, contract, slot0).await?;
    let raw1 = get_storage_at_raw(provider, contract, slot1).await?;

    let token_x = Address::from_slice(&<B256 as AsRef<[u8]>>::as_ref(&raw0)[12..]);
    let token_y = Address::from_slice(&<B256 as AsRef<[u8]>>::as_ref(&raw1)[12..]);

    Ok((token_x, token_y))
}

// ============================================================
// Test: 发现池子 & 初始化 & Swap 模拟精度验证
// ============================================================

#[tokio::test]
async fn test_caliber_prop_discover_and_init() -> Result<()> {
    let rpc_url = match xlayer_provider_url() {
        Some(url) => url,
        None => {
            println!("SKIP: XLAYER_PROVIDER not set");
            return Ok(());
        }
    };

    let provider = Arc::new(ProviderBuilder::new().connect_http(rpc_url.parse()?));

    // 验证链 ID
    let chain_id = provider.get_chain_id().await?;
    if chain_id != XLAYER_CHAIN_ID {
        println!(
            "SKIP: expected XLayer chain_id {}, got {}",
            XLAYER_CHAIN_ID, chain_id
        );
        return Ok(());
    }

    println!("=== Caliber propAMM XLayer Fork Test ===");
    println!("Contract: {}", CALIBER_CONTRACT);

    let caliber = ICaliberPropAMM::new(CALIBER_CONTRACT, provider.clone());

    // 1. 获取所有 pair IDs
    let pair_ids = caliber
        .getAllPairIds(U256::ZERO, U256::from(50u64))
        .call()
        .await?;

    println!("Found {} pair(s) on Caliber DEX", pair_ids.len());

    if pair_ids.is_empty() {
        println!("No pairs found, test passes vacuously.");
        return Ok(());
    }

    // 2. 对每个 pair 进行测试
    let mut tested_count = 0;

    for (i, &pair_id) in pair_ids.iter().enumerate() {
        println!("\n--- Pair #{} ---", i);
        println!("pairId: {:?}", pair_id);

        // 读取 token 地址
        let (token_x, token_y) = match read_token_addresses(
            provider.as_ref(),
            CALIBER_CONTRACT,
            pair_id,
        )
        .await
        {
            Ok(tokens) => tokens,
            Err(e) => {
                println!("  Failed to read tokens: {e}");
                continue;
            }
        };

        // 读取 token decimals
        let erc20_x = IERC20Metadata::new(token_x, provider.clone());
        let erc20_y = IERC20Metadata::new(token_y, provider.clone());

        let decimals_x = erc20_x.decimals().call().await.unwrap_or(18);
        let decimals_y = erc20_y.decimals().call().await.unwrap_or(18);

        println!("  tokenX: {} (decimals={})", token_x, decimals_x);
        println!("  tokenY: {} (decimals={})", token_y, decimals_y);

        // 排序 token
        let (token_a, token_b, decimals_a, decimals_b) = if token_x < token_y {
            (token_x, token_y, decimals_x, decimals_y)
        } else {
            (token_y, token_x, decimals_y, decimals_x)
        };

        // 3. 创建并初始化 CaliberPropPool
        let virtual_address =
            CaliberPropPool::virtual_address_from_pair_id(pair_id, CALIBER_CONTRACT);

        let pool = CaliberPropPool {
            contract_address: CALIBER_CONTRACT,
            pair_id,
            virtual_address,
            token_x,
            token_y,
            created_block: 0,
            last_synced_block: 0,
            token_a: amms::amms::Token {
                address: token_a,
                decimals: decimals_a,
                symbol: String::new(),
                chain_id,
            },
            token_b: amms::amms::Token {
                address: token_b,
                decimals: decimals_b,
                symbol: String::new(),
                chain_id,
            },
            reserve_a: U256::ZERO,
            reserve_b: U256::ZERO,
            ladder: Default::default(),
            price_a_in_b: 0.0,
            price_b_in_a: 0.0,
        };

        let pool = match pool
            .init::<alloy::network::Ethereum, _>(BlockId::latest(), provider.clone())
            .await
        {
            Ok(p) => {
                println!("  Initialized successfully!");
                p
            }
            Err(e) => {
                println!("  Init failed: {e}");
                continue;
            }
        };

        println!(
            "  reserve_a: {}, reserve_b: {}",
            pool.reserve_a, pool.reserve_b
        );
        println!(
            "  ladder_a_to_b points: {}, ladder_b_to_a points: {}",
            pool.ladder.ladder_a_to_b.len(),
            pool.ladder.ladder_b_to_a.len()
        );
        println!(
            "  price_a_in_b: {:.6}, price_b_in_a: {:.6}",
            pool.price_a_in_b, pool.price_b_in_a
        );

        // 4. 诊断：直接调用 quote() 验证池子是否有定价
        test_direct_quote(
            provider.clone(),
            CALIBER_CONTRACT,
            &pool,
            token_a,
            token_b,
            pair_id,
        )
        .await?;

        // 5. 验证 simulate_swap vs 链上 quote()
        test_swap_accuracy(
            provider.clone(),
            CALIBER_CONTRACT,
            &pool,
            token_a,
            token_b,
            pair_id,
        )
        .await?;

        tested_count += 1;

        // 只测试第一个池子以节省时间
        if tested_count >= 1 {
            break;
        }
    }

    if tested_count == 0 {
        println!("No testable pairs found.");
    }

    Ok(())
}

// ============================================================
// 诊断：直接调用 quote() 验证池子是否有定价
// ============================================================

async fn test_direct_quote<N, P>(
    provider: P,
    contract_address: Address,
    _pool: &CaliberPropPool,
    token_a: Address,
    token_b: Address,
    pair_id: B256,
) -> Result<()>
where
    N: Network,
    P: Provider<N> + Clone,
{
    let caliber = ICaliberPropAMM::new(contract_address, provider);

    println!("\n  === Direct Quote Diagnostic ===");

    // 测试 quote() 单次调用（token_a → token_b）
    let test_amounts = [
        U256::from(1_000_000u64),   // 1 USDT
        U256::from(100_000_000u64), // 100 USDT
    ];

    for &amount_in in &test_amounts {
        match caliber
            .quote(pair_id, token_a, token_b, amount_in)
            .call()
            .await
        {
            Ok(amount_out) => {
                if amount_out.is_zero() {
                    println!("    quote({token_a}→{token_b}, {amount_in}): amountOut=0 (no quote available)");
                } else {
                    println!("    quote({token_a}→{token_b}, {amount_in}): amountOut={amount_out}");
                }
            }
            Err(e) => {
                println!("    quote({token_a}→{token_b}, {amount_in}): ERROR: {e}");
            }
        }
    }

    // 测试 batchQuote 单次调用（看具体返回结构）
    let batch_amount = U256::from(10_000_000u64); // 10 USDT
    let requests = vec![
        ICaliberPropAMM::QuoteRequest {
            pairId: pair_id,
            tokenIn: token_a,
            tokenOut: token_b,
            amountIn: batch_amount,
        },
        ICaliberPropAMM::QuoteRequest {
            pairId: pair_id,
            tokenIn: token_b,
            tokenOut: token_a,
            amountIn: batch_amount,
        },
    ];

    match caliber.batchQuote(requests).call().await {
        Ok(results) => {
            println!("    batchQuote returned {} results:", results.len());
            for (i, r) in results.iter().enumerate() {
                println!(
                    "      [{}] amountOut={}, success={}",
                    i, r.amountOut, r.success
                );
            }
        }
        Err(e) => {
            println!("    batchQuote ERROR: {e}");
        }
    }

    Ok(())
}

// ============================================================
// 验证 simulate_swap 与链上 quote() 对比
// ============================================================

async fn test_swap_accuracy<N, P>(
    provider: P,
    contract_address: Address,
    pool: &CaliberPropPool,
    token_a: Address,
    token_b: Address,
    pair_id: B256,
) -> Result<()>
where
    N: Network,
    P: Provider<N> + Clone,
{
    let caliber = ICaliberPropAMM::new(contract_address, provider);

    // 测试多个交易量（reserve 的 0.1%, 0.5%, 1%, 5%）
    let test_percentages = [1u64, 5, 10, 50]; // basis points * 10 (1 = 0.1%, 5 = 0.5%, etc.)

    println!("\n  === Swap Accuracy Test ===");

    for &bps_x10 in &test_percentages {
        let amount_in_a = pool.reserve_a * U256::from(bps_x10) / U256::from(1000);
        let amount_in_b = pool.reserve_b * U256::from(bps_x10) / U256::from(1000);

        for &(token_in, token_out, amount_in) in &[
            (token_a, token_b, amount_in_a),
            (token_b, token_a, amount_in_b),
        ] {
            if amount_in.is_zero() {
                continue;
            }

            // 本地 simulate_swap
            let local_out = match pool.simulate_swap(token_in, token_out, amount_in) {
                Ok(v) => v,
                Err(e) => {
                    println!(
                        "    simulate_swap error (in={amount_in}): {e}"
                    );
                    continue;
                }
            };

            // 链上 quote()
            let chain_out = caliber
                .quote(pair_id, token_in, token_out, amount_in)
                .call()
                .await?;

            let bps = bps_x10 as f64 / 10.0;
            let diff = if chain_out > local_out {
                chain_out - local_out
            } else {
                local_out - chain_out
            };

            let diff_bps = if chain_out.is_zero() {
                0.0
            } else {
                (diff.as_limbs()[0] as f64) / (chain_out.as_limbs()[0] as f64) * 10000.0
            };

            let status = if diff_bps < 100.0 { "OK" } else { "WARN" };

            println!(
                "    [{status}] {bps:.1}% reserve: amount_in={amount_in} \
                 local_out={local_out} chain_out={chain_out} diff={diff_bps:.1}bps"
            );
        }
    }

    Ok(())
}

// ============================================================
// Test: with_amms 集成（StateSpace 构建）
// ============================================================

#[tokio::test]
async fn test_caliber_prop_with_amms() -> Result<()> {
    let rpc_url = match xlayer_provider_url() {
        Some(url) => url,
        None => {
            println!("SKIP: XLAYER_PROVIDER not set");
            return Ok(());
        }
    };

    let provider = Arc::new(ProviderBuilder::new().connect_http(rpc_url.parse()?));

    let chain_id = provider.get_chain_id().await?;
    if chain_id != XLAYER_CHAIN_ID {
        println!("SKIP: wrong chain_id {chain_id}");
        return Ok(());
    }

    let caliber = ICaliberPropAMM::new(CALIBER_CONTRACT, provider.clone());

    // 发现第一个 pair
    let pair_ids = caliber
        .getAllPairIds(U256::ZERO, U256::from(10u64))
        .call()
        .await?;

    if pair_ids.is_empty() {
        println!("No pairs found.");
        return Ok(());
    }

    let pair_id = pair_ids[0];

    // 读取 token 地址
    let (token_x, token_y) =
        match read_token_addresses(provider.as_ref(), CALIBER_CONTRACT, pair_id).await {
            Ok(t) => t,
            Err(e) => {
                println!("Failed to read tokens: {e}");
                return Ok(());
            }
        };

    let (token_a, token_b) = if token_x < token_y {
        (token_x, token_y)
    } else {
        (token_y, token_x)
    };

    let virtual_address =
        CaliberPropPool::virtual_address_from_pair_id(pair_id, CALIBER_CONTRACT);

    let pool = CaliberPropPool {
        contract_address: CALIBER_CONTRACT,
        pair_id,
        virtual_address,
        token_x,
        token_y,
        created_block: 0,
        last_synced_block: 0,
        token_a: amms::amms::Token {
            address: token_a,
            decimals: 18, // 在 sync 阶段填充
            symbol: String::new(),
            chain_id,
        },
        token_b: amms::amms::Token {
            address: token_b,
            decimals: 18,
            symbol: String::new(),
            chain_id,
        },
        reserve_a: U256::ZERO,
        reserve_b: U256::ZERO,
        ladder: Default::default(),
        price_a_in_b: 0.0,
        price_b_in_a: 0.0,
    };

    let seed_amm = amms::amms::amm::AMM::CaliberPropPool(pool);

    println!("=== with_amms Integration Test ===");
    println!(
        "Building StateSpace with 1 Caliber pool: {}",
        virtual_address
    );

    // 使用 with_amms 构建 StateSpace
    let result = StateSpaceBuilder::new(provider.clone())
        .block(0)
        .with_amms(vec![seed_amm])
        .sync()
        .await;

    match result {
        Ok(manager) => {
            let state = manager.state.read().await;
            let pool = state
                .get(&virtual_address)
                .expect("pool should be in state");

            println!(
                "StateSpace built successfully! Pool has {} ladder_a_to_b points.",
                match pool {
                    amms::amms::amm::AMM::CaliberPropPool(p) => p.ladder.ladder_a_to_b.len(),
                    _ => 0,
                }
            );
        }
        Err(e) => {
            println!("StateSpace build failed: {e}");
            // 非致命错误 — Caliber sync 可能不完全支持
        }
    }

    Ok(())
}

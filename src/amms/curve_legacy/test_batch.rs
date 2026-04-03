// Test file for Curve Legacy batch initialization verification
// Run with: cargo test -p amms test_curve_legacy_batch --features test -- --nocapture

#[cfg(test)]
mod tests {
    use crate::amms::curve_legacy::factory::GetCurveLegacyPoolDataBatchRequest;
    use crate::amms::curve_legacy::factory::PoolData;

    use alloy::eips::BlockId;
    use alloy::primitives::{address, Address};
    use alloy::providers::ProviderBuilder;
    use alloy::sol_types::SolValue;
    use eyre::Result;

    // Use struct from factory
    use GetCurveLegacyPoolDataBatchRequest::PoolInput;

    // Known Legacy pools from Main Registry
    // pool_subtype: 0=StableSwap (LegacyStable), 1=CryptoSwap (LegacyCrypto)
    const LEGACY_POOLS: &[(Address, u8)] = &[
        // From graph.ndjson - 12 Curve Legacy pools
        (address!("1005F7406f32a61BD760CfA14aCCd2737913d546"), 0), // LegacyStable - USDC/USDT
        (address!("4e0915C88bC70750D68C481540F081fEFaF22273"), 0), // LegacyStable - FRAX/USDC
        (address!("752eBeb79963cf0732E9c0fec72a49FD1DEfAEAC"), 1), // LegacyCrypto
        (address!("80466c64868E1ab14a1Ddf27A676C3fcBE638Fe5"), 0), // LegacyStable - WETH/WBTC
        (address!("9838eCcC42659FA8AA7daF2aD134b53984c9427b"), 1), // LegacyCrypto
        (address!("98638FAcf9a3865cd033F36548713183f6996122"), 1), // LegacyCrypto
        (address!("AdCFcf9894335dC340f6Cd182aFA45999F45Fc44"), 1), // LegacyCrypto
        (address!("B576491F1E6e5E62f1d8F26062Ee822B40B0E0d4"), 1), // LegacyCrypto - CVX/WETH
        (address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46"), 1), // LegacyCrypto - tricrypto2
        (address!("DcEF968d416a41Cdac0ED8702fAC8128A64241A2"), 0), // LegacyStable - FRAX/USDC
        (address!("E84f5b1582BA325fDf9cE6B0c1F087ccfC924e54"), 1), // LegacyCrypto
        (address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7"), 0), // LegacyStable - 3pool
    ];

    #[tokio::test]
    async fn test_batch_request_single_pool() -> Result<()> {
        let rpc_url = std::env::var("ETHEREUM_PROVIDER")
            .unwrap_or_else(|_| "https://eth.llamarpc.com".to_string());

        let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

        // Test 3pool (StableSwap)
        let pool_address = LEGACY_POOLS[0].0;
        let pool_type = LEGACY_POOLS[0].1;

        println!("Testing pool: {:?} (type: {})", pool_address, pool_type);

        let inputs = vec![PoolInput {
            pool: pool_address,
            poolType: pool_type,
        }];

        let deployer = GetCurveLegacyPoolDataBatchRequest::deploy_builder(provider.clone(), inputs);

        match deployer.call_raw().block(BlockId::latest()).await {
            Ok(res) => {
                println!("Raw response length: {} bytes", res.len());

                match <Vec<PoolData> as SolValue>::abi_decode(&res) {
                    Ok(pool_data_list) => {
                        for data in &pool_data_list {
                            println!("Pool: {:?}", data.poolAddress);
                            println!("  nCoins: {}", data.nCoins);
                            println!("  coins: {:?}", data.coins);
                            println!("  balances: {:?}", data.balances);
                            println!("  decimals: {:?}", data.decimals);
                            println!("  amp: {}", data.amp);
                            println!("  fee: {}", data.fee);

                            // Verify balances are non-zero
                            for (i, balance) in data.balances.iter().enumerate() {
                                if balance.is_zero() {
                                    println!("  WARNING: Balance {} is ZERO!", i);
                                } else {
                                    println!("  Balance {} OK: {}", i, balance);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        println!("Decode error: {}", e);
                        return Err(e.into());
                    }
                }
            }
            Err(e) => {
                println!("Call failed: {}", e);
                return Err(e.into());
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_batch_request_all_known_pools() -> Result<()> {
        let rpc_url = std::env::var("ETHEREUM_PROVIDER")
            .unwrap_or_else(|_| "https://eth.llamarpc.com".to_string());

        let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

        for (pool_address, pool_type) in LEGACY_POOLS {
            println!(
                "\n=== Testing pool: {:?} (type: {}) ===",
                pool_address, pool_type
            );

            let inputs = vec![PoolInput {
                pool: *pool_address,
                poolType: *pool_type,
            }];

            let deployer =
                GetCurveLegacyPoolDataBatchRequest::deploy_builder(provider.clone(), inputs);

            match deployer.call_raw().block(BlockId::latest()).await {
                Ok(res) => match <Vec<PoolData> as SolValue>::abi_decode(&res) {
                    Ok(pool_data_list) => {
                        for data in &pool_data_list {
                            println!("  nCoins: {}", data.nCoins);

                            let mut all_balances_ok = true;
                            for (i, balance) in data.balances.iter().enumerate() {
                                if balance.is_zero() {
                                    println!("  FAIL: Balance {} is ZERO!", i);
                                    all_balances_ok = false;
                                }
                            }

                            if all_balances_ok {
                                println!(
                                    "  SUCCESS: All {} balances are non-zero",
                                    data.balances.len()
                                );
                            }
                        }
                    }
                    Err(e) => {
                        println!("  Decode FAILED: {}", e);
                    }
                },
                Err(e) => {
                    println!("  Call FAILED: {}", e);
                }
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_full_init_batch_from_ndjson() -> Result<()> {
        use crate::amms::amm::AMM;
        use crate::amms::curve_legacy::factory::CurveLegacyFactory;
        use crate::amms::curve_legacy::types::{CurveLegacyPool, CurveLegacyPoolType};
        use std::fs::File;
        use std::io::{BufRead, BufReader};

        let rpc_url = std::env::var("ETHEREUM_PROVIDER")
            .unwrap_or_else(|_| "https://eth.llamarpc.com".to_string());

        let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

        // 从NDJSON文件读取CurveLegacy池子
        let ndjson_path = "../../outputs/dex-detector/graph.ndjson";
        println!("[DEBUG] 读取NDJSON文件: {}", ndjson_path);

        let file = File::open(ndjson_path).expect("无法打开ndjson文件");
        let reader = BufReader::new(file);

        let mut amms: Vec<AMM> = Vec::new();

        for line in reader.lines() {
            let line = line?;
            if line.contains("\"pool_type\":\"CurveLegacy\"") {
                // 解析JSON
                let json: serde_json::Value = serde_json::from_str(&line)?;

                let address_str = json["address"].as_str().unwrap();
                let pool_subtype = json["pool_subtype"].as_str().unwrap_or("LegacyStable");

                let address: Address = address_str.parse()?;
                let pool_type = if pool_subtype.contains("Crypto") {
                    CurveLegacyPoolType::CryptoSwap
                } else {
                    CurveLegacyPoolType::StableSwap
                };

                println!(
                    "[DEBUG] 发现CurveLegacy池子: {:?} ({})",
                    address, pool_subtype
                );
                amms.push(AMM::CurveLegacyPool(CurveLegacyPool::new(
                    address, pool_type,
                )));
            }
        }

        println!("[DEBUG] 总共发现 {} 个CurveLegacy池子", amms.len());

        if amms.is_empty() {
            println!("[DEBUG] 没有找到CurveLegacy池子！");
            return Ok(());
        }

        // 调用init_batch
        println!("[DEBUG] 开始调用 CurveLegacyFactory::init_batch...");
        match CurveLegacyFactory::init_batch::<_, _>(amms, BlockId::latest(), provider).await {
            Ok(initialized) => {
                println!("SUCCESS: Initialized {} pools", initialized.len());
                for amm in initialized {
                    if let AMM::CurveLegacyPool(pool) = amm {
                        println!(
                            "  Pool {:?}: n_coins={}, balances={:?}",
                            pool.address,
                            pool.n_coins,
                            pool.balances.len()
                        );

                        // Check required fields based on pool type
                        match pool.pool_type {
                            CurveLegacyPoolType::StableSwap => {
                                if pool.amp.is_none() {
                                    println!("    [ERROR] StableSwap pool missing amp!");
                                } else {
                                    println!("    [OK] amp: {:?}", pool.amp.unwrap());
                                }
                            }
                            CurveLegacyPoolType::CryptoSwap => {
                                let mut missing = Vec::new();
                                if pool.d.is_none() {
                                    missing.push("d");
                                }
                                if pool.gamma.is_none() {
                                    missing.push("gamma");
                                }
                                if pool.mid_fee.is_none() {
                                    missing.push("mid_fee");
                                }
                                if pool.out_fee.is_none() {
                                    missing.push("out_fee");
                                }
                                if pool.fee_gamma.is_none() {
                                    missing.push("fee_gamma");
                                }

                                if !missing.is_empty() {
                                    println!(
                                        "    [ERROR] CryptoSwap pool missing fields: {:?}",
                                        missing
                                    );
                                } else {
                                    println!("    [OK] CryptoSwap fields present (d, gamma, etc.)");
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                println!("[DEBUG] FAILED: {}", e);
                return Err(e.into());
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_batch_request_all_legacy_pools_together() -> Result<()> {
        use crate::amms::amm::AMM;
        use crate::amms::curve_legacy::factory::CurveLegacyFactory;
        use crate::amms::curve_legacy::types::{CurveLegacyPool, CurveLegacyPoolType};

        let rpc_url = std::env::var("ETHEREUM_PROVIDER")
            .unwrap_or_else(|_| "https://eth.llamarpc.com".to_string());

        let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

        let mut amms: Vec<AMM> = Vec::new();
        for (pool_address, pool_type) in LEGACY_POOLS {
            let p_type = if *pool_type == 1 {
                CurveLegacyPoolType::CryptoSwap
            } else {
                CurveLegacyPoolType::StableSwap
            };
            amms.push(AMM::CurveLegacyPool(CurveLegacyPool::new(
                *pool_address,
                p_type,
            )));
        }

        println!("Testing batch init with {} pools...", amms.len());

        // 使用 factory 的 init_batch 逻辑 (注意：这会使用 factory.rs 中修改后的 5M gas limit)
        match CurveLegacyFactory::init_batch::<_, _>(amms, BlockId::latest(), provider).await {
            Ok(initialized) => {
                println!("SUCCESS: Initialized {} pools", initialized.len());
                for amm in initialized {
                    if let AMM::CurveLegacyPool(pool) = amm {
                        println!(
                            "  Pool {:?}: n_coins={}, balances={:?}",
                            pool.address,
                            pool.n_coins,
                            pool.balances.len()
                        );

                        // Check required fields based on pool type
                        match pool.pool_type {
                            CurveLegacyPoolType::StableSwap => {
                                if pool.amp.is_none() {
                                    println!("    [ERROR] StableSwap pool missing amp!");
                                } else {
                                    println!("    [OK] amp: {:?}", pool.amp.unwrap());
                                }
                            }
                            CurveLegacyPoolType::CryptoSwap => {
                                let mut missing = Vec::new();
                                if pool.d.is_none() {
                                    missing.push("d");
                                }
                                if pool.gamma.is_none() {
                                    missing.push("gamma");
                                }
                                if pool.mid_fee.is_none() {
                                    missing.push("mid_fee");
                                }
                                if pool.out_fee.is_none() {
                                    missing.push("out_fee");
                                }
                                if pool.fee_gamma.is_none() {
                                    missing.push("fee_gamma");
                                }

                                if !missing.is_empty() {
                                    println!(
                                        "    [ERROR] CryptoSwap pool missing fields: {:?}",
                                        missing
                                    );
                                } else {
                                    println!("    [OK] CryptoSwap fields present (d, gamma, etc.)");
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                println!("FAILED: {}", e);
                return Err(e.into());
            }
        }

        Ok(())
    }
}

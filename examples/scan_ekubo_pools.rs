use alloy::{
    primitives::{address, b256, Address, B256, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::{BlockNumberOrTag, Filter},
    sol,
    sol_types::SolEvent,
};
use eyre::Result;

sol! {
    #[derive(Debug)]
    struct PoolKey {
        address token0;
        address token1;
        bytes32 config;
    }

    #[derive(Debug)]
    event PoolInitialized(
        bytes32 poolId,
        PoolKey poolKey,
        int32 tick,
        uint96 sqrtRatio
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    let rpc_url = std::env::var("ETHEREUM_RPC_URL").unwrap_or("https://eth.merkle.io".to_string());
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let core_address = address!("e0e0e08a6a4b9dc7bd67bcb7aade5cf48157d444");

    // Retry loop for getting current block
    let mut current_block = 0;
    loop {
        match provider.get_block_number().await {
            Ok(bn) => {
                current_block = bn;
                println!("Current Head: {}", current_block);
                break;
            }
            Err(e) => {
                println!("Error getting block number: {:?}. Retrying...", e);
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }

    // User specified start block
    let start_scan_block = 22047272;
    let target_pool_id = b256!("14f04a89d7912a7dc3d3ec35e830efb8d2ddbddb3dd345bcb302aa14cc644c60");

    println!(
        "Simple Sequential Scanning forward from block {} for target PoolId {}...",
        start_scan_block, target_pool_id
    );

    let batch_size = 1000;

    let mut current_scan_start = start_scan_block;

    while current_scan_start < current_block {
        let mut current_scan_end = current_scan_start + batch_size;
        if current_scan_end > current_block {
            current_scan_end = current_block;
        }

        println!("Scanning {}-{} ...", current_scan_start, current_scan_end);

        let filter = Filter::new()
            .address(core_address)
            .event_signature(PoolInitialized::SIGNATURE_HASH)
            .from_block(BlockNumberOrTag::Number(current_scan_start))
            .to_block(BlockNumberOrTag::Number(current_scan_end));

        match provider.get_logs(&filter).await {
            Ok(logs) => {
                for log in logs {
                    if let Ok(decoded) = PoolInitialized::decode_log(&log.inner) {
                        println!("Decoded: {:?}", decoded);
                        if decoded.poolId == target_pool_id {
                            println!("\n✅ FOUND TARGET POOL!");
                            println!("Block: {}", log.block_number.unwrap_or_default());
                            println!(
                                "Transaction: {:?}",
                                log.transaction_hash.unwrap_or_default()
                            );
                            println!("  PoolId: {}", decoded.poolId);
                            println!("  Token0: {:?}", decoded.poolKey.token0);
                            println!("  Token1: {:?}", decoded.poolKey.token1);
                            println!("  Config (Raw): {}", decoded.poolKey.config);

                            // Unpack Config
                            let config_u256: U256 = decoded.poolKey.config.into();

                            // tickSpacing: lowest 32 bits
                            let tick_spacing_mask = U256::from(0xFFFFFFFFu64);
                            let tick_spacing = (config_u256 & tick_spacing_mask).to::<u64>() as u32;

                            // fee: next 64 bits (bits 32..96)
                            let fee_mask = U256::from(0xFFFFFFFFFFFFFFFFu128);
                            // Explicitly type to avoid inference errors
                            let fee_u256: U256 = (config_u256 >> 32) & fee_mask;
                            let fee: u64 = fee_u256.to::<u64>();

                            // extension: next 160 bits (bits 96..256)
                            let extension_val: U256 = config_u256 >> 96;
                            let extension = Address::from_word(B256::from(extension_val));

                            println!("  -> Fee: {}", fee);
                            println!("  -> TickSpacing: {}", tick_spacing);
                            println!("  -> Extension: {:?}", extension);

                            println!("  Tick: {}", decoded.tick);
                            println!("  SqrtRatio: {}", decoded.sqrtRatio);
                            return Ok(());
                        }
                    }
                }
                // Be nice to the RPC
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(e) => {
                println!(
                    "Error scanning block range {}-{}: {:?}. Retrying...",
                    current_scan_start, current_scan_end, e
                );
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
        }

        current_scan_start = current_scan_end + 1;
    }

    println!("Target pool not found in scanned range.");
    Ok(())
}

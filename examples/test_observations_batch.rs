/// Test Multicall3-based observations fetching (replaces failed batch contract).
use alloy::primitives::{Address, U256};
use alloy::providers::{ProviderBuilder, WsConnect};
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use std::str::FromStr;
use std::time::Instant;

sol! {
    #[sol(rpc)]
    interface IMulticall3 {
        struct Call3 { address target; bool allowFailure; bytes callData; }
        struct Result { bool success; bytes returnData; }
        function aggregate3(Call3[] calldata calls) external payable returns (Result[] memory returnData);
    }
}

const MULTICALL3: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");

const POOLS: &[&str] = &[
    "0xb2cc224c1c9feE385f8ad6a55b4d94E92359DC59",
    "0xaFB62448929664Bfccb0aAe22f232520e765bA88",
    "0x3e66e55e97ce60096f74b7C475e8249f2D31a9fb",
];

use alloy::primitives::address;

// ICLPoolFull interface for observations(uint256)
sol! {
    #[sol(rpc)]
    contract IPool {
        function slot0() external view returns (uint160, int24, uint16, uint16, uint16, bool);
        function observations(uint256 index) external view returns (uint32 blockTimestamp, int56 tickCumulative, uint160 secondsPerLiquidityCumulativeX128, bool initialized);
    }
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let rpc = std::env::var("BASE_RPC_WS").unwrap_or_else(|_|
        "wss://base-mainnet.core.chainstack.com/fc5f8eef2b27bee75a83ca6ab5a02634".to_string());
    let provider = ProviderBuilder::new().connect_ws(WsConnect::new(rpc)).await?;
    let addrs: Vec<Address> = POOLS.iter().map(|s| Address::from_str(s).unwrap()).collect();
    let mc3 = IMulticall3::new(MULTICALL3, provider.clone());

    // Step 1: Get slot0 for each pool
    println!("--- Step 1: slot0 ---");
    let start = Instant::now();
    let mut metas = Vec::new();
    for &addr in &addrs {
        let s0 = IPool::new(addr, provider.clone()).slot0().call().await?;
        println!("  {}: card={} idx={} [{:?}]", addr, s0._3, s0._2, start.elapsed());
        metas.push((addr, s0._3, s0._2));
    }

    // Step 2: Multicall3 - batch observations(i)
    println!("\n--- Step 2: Multicall3 observations(i) ---");
    let start = Instant::now();
    let mut calls = Vec::new();
    let mut call_meta: Vec<(Address, u16, u16, usize)> = Vec::new(); // addr, card, ridx, local_idx

    for &(addr, card, ridx) in &metas {
        let count = (card as u16).min(1500);
        for j in 0..count {
            let sidx = (ridx as u64 + card as u64 - j as u64) % card as u64;
            let calldata = IPool::observationsCall { index: U256::from(sidx) }.abi_encode();
            calls.push(IMulticall3::Call3 { target: addr, allowFailure: true, callData: calldata.into() });
            call_meta.push((addr, card, ridx, j as usize));
        }
    }
    println!("  Total calls: {}", calls.len());

    match mc3.aggregate3(calls).call().await {
        Ok(results) => {
            println!("  Got {} results in {:?}", results.len(), start.elapsed());
            let mut pool_obs: std::collections::HashMap<Address, Vec<(u32, i128)>> = std::collections::HashMap::new();
            for (i, res) in results.iter().enumerate() {
                if i >= call_meta.len() { break; }
                if res.returnData.is_empty() { continue; }
                if let Ok(dec) = <IPool::observationsCall as SolCall>::abi_decode_returns(&res.returnData) {
                    if dec.initialized && dec.blockTimestamp != 0 {
                        let tc: i128 = dec.tickCumulative.unchecked_into::<i64>() as i128;
                        pool_obs.entry(call_meta[i].0).or_default().push((dec.blockTimestamp, tc));
                    }
                }
            }
            for (addr, obs) in &pool_obs {
                let min_ts = obs.iter().map(|(ts,_)| *ts).min().unwrap_or(0);
                let max_ts = obs.iter().map(|(ts,_)| *ts).max().unwrap_or(0);
                println!("  {}: {} observations, ts_range={}..{}", addr, obs.len(), min_ts, max_ts);
            }
        }
        Err(e) => println!("  FAILED: {:?}", e),
    }

    println!("\nTotal time: {:?}", start.elapsed());
    Ok(())
}

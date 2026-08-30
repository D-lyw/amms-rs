//! BSC PancakeV3 批量初始化性能探针（生产同路径）。
//!
//! 用法:
//!   GRAPH_PATH=<56_graph.ndjson> BSC_RPC_HTTP=<https rpc> \
//!     cargo run --example bsc_pancake_v3_init_probe
//!
//! 默认: GRAPH_PATH=../dex-arbitrage/configs/56_graph.ndjson
//!       BSC_RPC_HTTP=https://bsc-mainnet.core.chainstack.com/6013580ce4d36cca1542fb08a8ab2269
//!
//! 流程与生产 arbitrage 一致：从拓扑加载 PancakeV3 池 → StateSpaceBuilder
//! with_init_http_endpoint（HTTP 多连接）→ sync() 批量 init_batch（30 池/批、
//! 固定快照块、批间 200ms），打印总耗时与各批耗时。

use std::time::Instant;

use alloy::{
    primitives::Address,
    providers::Provider,
    providers::ProviderBuilder,
    rpc::client::ClientBuilder,
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::{
    amms::{amm::AMM, pancake_v3::PancakeV3Pool},
    state_space::StateSpaceBuilder,
};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(true)
        .init();

    let graph_path = std::env::var("GRAPH_PATH")
        .unwrap_or_else(|_| "../dex-arbitrage/configs/56_graph.ndjson".into());
    let rpc_http = std::env::var("BSC_RPC_HTTP").unwrap_or_else(|_| {
        "https://bsc-mainnet.core.chainstack.com/6013580ce4d36cca1542fb08a8ab2269".into()
    });

    // 1) 从拓扑 ndjson 收集 PancakeV3 池
    let mut pancake_v3_addrs: Vec<Address> = Vec::new();
    let content = std::fs::read_to_string(&graph_path)?;
    for line in content.lines() {
        let v: serde_json::Value = serde_json::from_str(line)?;
        if v.get("type").and_then(|t| t.as_str()) != Some("pool") {
            continue;
        }
        if v.get("pool_type").and_then(|t| t.as_str()) != Some("PancakeV3") {
            continue;
        }
        let addr: Address = v
            .get("address")
            .and_then(|a| a.as_str())
            .ok_or_else(|| eyre::eyre!("pool row missing address"))?
            .parse()?;
        pancake_v3_addrs.push(addr);
    }
    tracing::info!(path = %graph_path, pools = pancake_v3_addrs.len(), "Loaded PancakeV3 pools");

    // 2) HTTP provider（与生产 init_http_endpoint 同端点）
    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(500))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(rpc_http.parse()?);
    let provider = ProviderBuilder::new().connect_client(client);

    let amms: Vec<AMM> = pancake_v3_addrs
        .into_iter()
        .map(|a| AMM::PancakeV3Pool(PancakeV3Pool::new(a)))
        .collect();

    let start = Instant::now();
    let chain_head = provider.get_block_number().await?;
    tracing::info!(chain_head, "Starting StateSpaceBuilder::sync()");

    // 3) 生产同路径：HTTP init 端点 + 批量初始化
    let manager = StateSpaceBuilder::new(provider)
        .with_amms(amms)
        .with_init_http_endpoint(rpc_http)
        .sync()
        .await?;

    let elapsed = start.elapsed();
    let state = manager.state.read().await;
    tracing::info!(
        elapsed_secs = elapsed.as_secs_f64(),
        synced_pools = state.state.len(),
        block = state.realtime_head.load(std::sync::atomic::Ordering::Relaxed),
        "PancakeV3 batch init complete"
    );

    Ok(())
}

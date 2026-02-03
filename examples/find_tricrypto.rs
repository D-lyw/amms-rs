use alloy::eips::BlockId;
use alloy::primitives::address;
use alloy::providers::ProviderBuilder;
use amms::amms::curve_ng::factory::CurveNGFactory;
use amms::amms::curve_ng::types::CurveNGPoolType;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let rpc_url = std::env::var("ETHEREUM_PROVIDER")?;
    let provider = ProviderBuilder::new().on_http(rpc_url.parse()?);

    // TriCrypto-NG Factory 地址 (Ethereum Mainnet)
    let tricrypto_factory_address = address!("0c0e5f2fF0ff18a3be9b835635039256dC4B4963");
    let factory = CurveNGFactory::new(
        tricrypto_factory_address,
        CurveNGPoolType::TriCrypto,
        18_500_000,
    );

    println!("Querying TriCrypto Factory at {:?}", factory.address);

    let pools = factory.get_pools(BlockId::latest(), provider).await?;

    println!("Found {} pools", pools.len());
    for pool in pools.iter().take(5) {
        println!("Pool: {:?}", pool.address);
        println!("  Coins: {:?}", pool.coins);
        println!("  D: {:?}", pool.d);
    }

    Ok(())
}

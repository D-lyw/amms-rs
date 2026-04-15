use std::env;

pub fn provider_url() -> Option<String> {
    dotenv::dotenv().ok();
    env::var("ETHEREUM_PROVIDER")
        .or_else(|_| env::var("ETHEREUM_RPC_URL"))
        .ok()
}

pub fn provider_url_required() -> eyre::Result<String> {
    provider_url().ok_or_else(|| {
        eyre::eyre!("ETHEREUM_PROVIDER or ETHEREUM_RPC_URL must be set for this test")
    })
}

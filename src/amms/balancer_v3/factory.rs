use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, B256, U256},
    providers::Provider,
    rpc::types::{Filter, Log},
    sol,
    sol_types::{SolEvent, SolValue},
};
use futures::{stream::FuturesUnordered, StreamExt};
use std::collections::HashMap;
use std::future::Future;

use super::{BalancerV3Pool, BalancerV3PoolType, V3TokenState};
use crate::amms::{
    amm::{AutomatedMarketMaker, AMM},
    error::AMMError,
    factory::{AutomatedMarketMakerFactory, DiscoverySync},
};

sol! {
    #[derive(Debug, PartialEq, Eq)]
    event PoolCreated(address indexed pool);

    struct PoolData {
        address poolAddress;
        uint8 poolType;
        address[] tokens;
        uint8[] decimals;
        uint256[] balances;
        uint256[] weights;
        uint256 amp;
        uint256 swapFee;
        address[] rateProviders;
        uint256[] rates;
    }
}

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    IGetBalancerV3PoolDataBatchRequest,
    "src/amms/abi/GetBalancerV3PoolDataBatchRequest.json"
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct BalancerV3Factory {
    pub address: Address,
    pub creation_block: u64,
    pub vault_address: Address,
}

impl BalancerV3Factory {
    pub fn new(address: Address, creation_block: u64, vault_address: Address) -> Self {
        Self {
            address,
            creation_block,
            vault_address,
        }
    }

    pub async fn get_all_pools<N, P>(
        &self,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        // Discovery logic reusing create_pool
        let disc_filter = Filter::new()
            .event_signature(PoolCreated::SIGNATURE_HASH)
            .address(self.address);

        let sync_provider = provider.clone();
        let mut futures = FuturesUnordered::new();

        let sync_step = 100_000;
        let mut latest_block = self.creation_block;
        let target_block = block_number.as_u64().unwrap_or(u64::MAX);

        while latest_block < target_block {
            let mut block_filter = disc_filter.clone();
            let from_block = latest_block;
            let to_block = (from_block + sync_step).min(target_block);

            block_filter = block_filter.from_block(from_block);
            block_filter = block_filter.to_block(to_block);

            let sync_provider = sync_provider.clone();

            futures.push(async move { sync_provider.get_logs(&block_filter).await });

            latest_block = to_block + 1;
        }

        let mut pools = vec![];
        while let Some(res) = futures.next().await {
            let logs = res?;
            for log in logs {
                if let Ok(amm) = self.create_pool(log) {
                    pools.push(amm);
                }
            }
        }

        Ok(pools)
    }

    pub async fn sync_all_pools<N, P>(
        amms: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        if amms.is_empty() {
            return Ok(vec![]);
        }

        let chain_id = provider.get_chain_id().await?;
        let vault_explorer_address = super::get_vault_explorer_address(chain_id)
            .ok_or_else(|| AMMError::Msg(format!("Unsupported chain id: {}", chain_id)))?;

        let batch_size = 20;
        let mut futures = FuturesUnordered::new();

        for chunk in amms.chunks(batch_size) {
            let chunk_pools: Vec<Address> = chunk
                .iter()
                .filter_map(|amm| {
                    if let AMM::BalancerV3Pool(pool) = amm {
                        Some(pool.address)
                    } else {
                        None
                    }
                })
                .collect();

            if chunk_pools.is_empty() {
                continue;
            }

            let deployer = IGetBalancerV3PoolDataBatchRequest::deploy_builder(
                provider.clone(),
                vault_explorer_address,
                chunk_pools,
            );

            futures.push(async move {
                let res = deployer.call_raw().block(block_number).await?;
                let data = <Vec<PoolData> as SolValue>::abi_decode(&res)?;
                Ok::<Vec<PoolData>, AMMError>(data)
            });
        }

        let mut synced_amms = HashMap::new();
        // Pre-populate with existing AMMs to preserve structure if needed, but we rebuild mostly
        for amm in amms {
            synced_amms.insert(amm.address(), amm);
        }

        while let Some(res) = futures.next().await {
            let pool_data_list = res?;

            for data in pool_data_list {
                if let Some(AMM::BalancerV3Pool(pool)) = synced_amms.get_mut(&data.poolAddress) {
                    pool.swap_fee = data.swapFee;

                    match data.poolType {
                        0 => {
                            pool.pool_type = BalancerV3PoolType::Weighted;
                            pool.weights = Some(data.weights);
                        }
                        1 => {
                            pool.pool_type = BalancerV3PoolType::Stable;
                            pool.amp = Some(data.amp);
                        }
                        _ => {} // Keep existing or default
                    }

                    for (i, &token_addr) in data.tokens.iter().enumerate() {
                        let balance = if i < data.balances.len() {
                            data.balances[i]
                        } else {
                            U256::ZERO
                        };
                        let rate = if i < data.rates.len() {
                            data.rates[i]
                        } else {
                            U256::from(10).pow(U256::from(18))
                        };
                        let rate_provider = if i < data.rateProviders.len() {
                            let rp = data.rateProviders[i];
                            if rp == Address::ZERO {
                                None
                            } else {
                                Some(rp)
                            }
                        } else {
                            None
                        };

                        if let Some(state) = pool.tokens.get_mut(&token_addr) {
                            state.balance = balance;
                            state.rate = rate;
                            state.rate_provider = rate_provider;
                        }
                    }
                }
            }
        }

        Ok(synced_amms.into_values().collect())
    }

    pub async fn init_batch<N, P>(
        amms: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        if amms.is_empty() {
            return Ok(vec![]);
        }

        let total = amms.len();

        let chain_id = provider.get_chain_id().await?;
        let vault_explorer_address = super::get_vault_explorer_address(chain_id)
            .ok_or_else(|| AMMError::Msg(format!("Unsupported chain id: {}", chain_id)))?;

        let batch_size = 20;
        let mut futures = FuturesUnordered::new();

        for chunk in amms.chunks(batch_size) {
            let chunk_pools: Vec<Address> = chunk
                .iter()
                .filter_map(|amm| {
                    if let AMM::BalancerV3Pool(pool) = amm {
                        Some(pool.address)
                    } else {
                        None
                    }
                })
                .collect();

            if chunk_pools.is_empty() {
                continue;
            }

            let deployer = IGetBalancerV3PoolDataBatchRequest::deploy_builder(
                provider.clone(),
                vault_explorer_address,
                chunk_pools,
            );

            futures.push(async move {
                let res = deployer.call_raw().block(block_number).await?;
                let data = <Vec<PoolData> as SolValue>::abi_decode(&res)?;
                Ok::<Vec<PoolData>, AMMError>(data)
            });
        }

        let mut synced_amms = HashMap::new();
        for amm in amms {
            synced_amms.insert(amm.address(), amm);
        }

        while let Some(res) = futures.next().await {
            let pool_data_list = res?;

            for data in pool_data_list {
                if let Some(AMM::BalancerV3Pool(pool)) = synced_amms.get_mut(&data.poolAddress) {
                    pool.swap_fee = data.swapFee;

                    match data.poolType {
                        0 => {
                            pool.pool_type = BalancerV3PoolType::Weighted;
                            pool.weights = Some(data.weights.clone());
                        }
                        1 => {
                            pool.pool_type = BalancerV3PoolType::Stable;
                            pool.amp = Some(data.amp);
                        }
                        _ => {}
                    }

                    pool.token_list = data.tokens.clone();
                    pool.tokens.clear();

                    for (i, &token_addr) in data.tokens.iter().enumerate() {
                        let decimals = if i < data.decimals.len() {
                            data.decimals[i]
                        } else {
                            0
                        };

                        if decimals == 0 {
                            tracing::warn!(?data.poolAddress, ?token_addr, "Skipping token with 0 decimals in Balancer V3 pool");
                            continue;
                        }
                        let balance = if i < data.balances.len() {
                            data.balances[i]
                        } else {
                            U256::ZERO
                        };
                        let rate = if i < data.rates.len() {
                            data.rates[i]
                        } else {
                            U256::from(10).pow(U256::from(18))
                        };
                        let scaling_factor =
                            U256::from(10).pow(U256::from(18u8.saturating_sub(decimals)));

                        let rate_provider = if i < data.rateProviders.len() {
                            let rp = data.rateProviders[i];
                            if rp == Address::ZERO {
                                None
                            } else {
                                Some(rp)
                            }
                        } else {
                            None
                        };

                        pool.tokens.insert(
                            token_addr,
                            V3TokenState {
                                address: token_addr,
                                decimals,
                                index: i,
                                balance,
                                scaling_factor,
                                rate,
                                rate_provider,
                            },
                        );
                    }
                }
            }
        }

        // Filter out invalid pools (no tokens populated or all-zero balances)
        let (mut valid_amms, invalid_amms): (Vec<_>, Vec<_>) =
            synced_amms.into_values().partition(|amm| match amm {
                AMM::BalancerV3Pool(pool) => {
                    !pool.tokens.is_empty() && pool.tokens.values().any(|t| !t.balance.is_zero())
                }
                _ => false,
            });

        if !invalid_amms.is_empty() {
            for amm in &invalid_amms {
                tracing::info!(
                    target: "amms::balancer_v3::init_batch",
                    address = ?amm.address(),
                    "Filtering out Balancer V3 pool with no valid tokens"
                );
            }
        }

        for amm in valid_amms.iter_mut() {
            if let AMM::BalancerV3Pool(pool) = amm {
                pool.update_spot_prices();
            }
        }

        let valid = valid_amms.len();
        let invalid = total.saturating_sub(valid);
        tracing::info!(
            target: "amms::balancer_v3::init_batch",
            total = total,
            valid = valid,
            invalid = invalid,
            "Batch initialization complete"
        );

        Ok(valid_amms)
    }
}

impl AutomatedMarketMakerFactory for BalancerV3Factory {
    type PoolVariant = BalancerV3Pool;

    fn address(&self) -> Address {
        self.address
    }

    fn creation_block(&self) -> u64 {
        self.creation_block
    }

    fn pool_creation_event(&self) -> B256 {
        PoolCreated::SIGNATURE_HASH
    }

    fn create_pool(&self, log: Log) -> Result<AMM, AMMError> {
        let pool_created = PoolCreated::decode_log(&log.inner)?;
        Ok(AMM::BalancerV3Pool(BalancerV3Pool::new(
            pool_created.pool,
            self.vault_address,
            BalancerV3PoolType::Weighted, // Default
        )))
    }
}

impl DiscoverySync for BalancerV3Factory {
    fn discover<N, P>(
        &self,
        to_block: BlockId,
        provider: P,
    ) -> impl Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        self.get_all_pools(to_block, provider)
    }

    fn sync<N, P>(
        &self,
        amms: Vec<AMM>,
        to_block: BlockId,
        provider: P,
    ) -> impl Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        Self::init_batch(amms, to_block, provider)
    }
}

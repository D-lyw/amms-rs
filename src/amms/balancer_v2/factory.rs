use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, B256, U256},
    providers::Provider,
    rpc::types::{Filter, FilterSet},
    sol,
    sol_types::SolValue,
};
use futures::{stream::FuturesUnordered, StreamExt};

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;
use tokio::time::sleep;

use super::{AmpState, BalancerV2Factory, BalancerV2Pool, BalancerV2PoolType, TokenState};
use crate::amms::{
    amm::{AutomatedMarketMaker, AMM},
    error::AMMError,
    factory::DiscoverySync,
};

sol! {
    #[sol(rpc)]
    interface IGetPoolId {
        function getPoolId() external view returns (bytes32);
    }

    struct PoolData {
        bytes32 poolId;
        address poolAddress;
        uint16 poolType;
        address[] tokens;
        uint256[] balances;
        uint16[] decimals;
        uint256[] weights;
        uint256 amp;
        uint256 swapFee;
        uint256 bptIndex;
        address[] rateProviders;
        uint256[] rates;
    }
}

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    IGetBalancerV2PoolDataBatchRequest,
    "src/amms/abi/GetBalancerV2PoolDataBatchRequest.json"
);

impl DiscoverySync for BalancerV2Factory {
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

impl BalancerV2Factory {
    pub async fn get_all_pools<N, P>(
        &self,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let disc_filter = Filter::new()
            .event_signature(FilterSet::from(vec![self.pool_creation_event()]))
            .address(vec![self.address()]); // Factory Address

        let sync_provider = provider.clone();
        let mut futures = FuturesUnordered::new();

        let sync_step = 100_000;
        let mut latest_block = self.creation_block;
        let target_block = block_number.as_u64().unwrap_or_default();
        let mut addresses = vec![];

        while latest_block < target_block {
            let mut block_filter = disc_filter.clone();
            let from_block = latest_block;
            let to_block = (from_block + sync_step).min(target_block);

            block_filter = block_filter.from_block(from_block);
            block_filter = block_filter.to_block(to_block);

            let sync_provider = sync_provider.clone();

            futures.push(async move { sync_provider.get_logs(&block_filter).await });

            latest_block = to_block + 1;

            // 添加批次间延迟，避免 RPS 超限
            if futures.len() >= 5 {
                while let Some(res) = futures.next().await {
                    let logs = res?;
                    for log in logs {
                        let topic1 = log.topics()[1];
                        let address = Address::from_word(topic1);
                        addresses.push(address);
                    }
                }
                sleep(Duration::from_millis(100)).await;
            }
        }

        // 处理剩余的 futures
        while let Some(res) = futures.next().await {
            let logs = res?;
            for log in logs {
                let topic1 = log.topics()[1];
                let address = Address::from_word(topic1);
                addresses.push(address);
            }
        }

        let mut pools = vec![];
        let mut futures_ids = FuturesUnordered::new();

        for addr in addresses {
            let provider = provider.clone();
            futures_ids.push(async move {
                let pool = IGetPoolId::new(addr, provider);
                // getPoolId returns a struct IGetPoolId::getPoolIdReturn with a single field usually named after the return param or _0
                // Assuming return type is bytes32, wrapped in a struct.
                // Or IGetPoolId::getPoolIdCall?
                // Alloy sol! generates method that returns a builder. call() returns Result<ReturnStruct>.
                // Return struct usually derefs to tuple if unnamed, or fields.
                // Let's assume it returns a value directly if it's a simple type? No, always struct.
                // Let's try .call().await?.0 if it's a tuple-like struct for single return.
                // Or ._0.
                let ret = pool.getPoolId().call().await?;
                Ok::<(Address, B256), AMMError>((addr, ret))
            });
        }

        while let Some(res) = futures_ids.next().await {
            if let Ok((addr, pool_id)) = res {
                pools.push(AMM::BalancerV2Pool(BalancerV2Pool {
                    address: addr,
                    last_synced_block: 0,
                    pool_id,
                    pool_type: self.pool_type,
                    vault_address: self.vault_address,
                    tokens: HashMap::new(),
                    token_list: Vec::new(),
                    swap_fee: U256::ZERO,
                    amp_state: None,
                    bpt_index: None,
                    spot_prices: HashMap::new(),
                }));
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

        let vault_address = if let AMM::BalancerV2Pool(pool) = &amms[0] {
            pool.vault_address
        } else {
            return Err(AMMError::IncompatibleAMMVariant);
        };

        let step = 10; // 减小批次大小以避免 max code size exceeded

        let mut amms_map = amms
            .into_iter()
            .map(|amm| (amm.address(), amm))
            .collect::<HashMap<_, _>>();

        // 收集所有地址用于分批处理
        let addresses: Vec<Address> = amms_map.keys().copied().collect();
        let address_chunks: Vec<_> = addresses.chunks(step).collect();

        // 串行处理每个批次，避免并发请求过多
        for chunk in address_chunks {
            let mut pool_ids = Vec::new();
            let mut pool_addresses = Vec::new();
            let mut pool_types = Vec::new();

            for &addr in chunk {
                if let Some(AMM::BalancerV2Pool(pool)) = amms_map.get(&addr) {
                    pool_ids.push(pool.pool_id);
                    pool_addresses.push(pool.address);
                    let type_u16: u16 = match pool.pool_type {
                        BalancerV2PoolType::Weighted => 0,
                        BalancerV2PoolType::Stable => 1,
                        BalancerV2PoolType::ComposableStable => 2,
                    };
                    pool_types.push(type_u16);
                }
            }

            let deployer = IGetBalancerV2PoolDataBatchRequest::deploy_builder(
                provider.clone(),
                vault_address,
                pool_ids,
                pool_addresses,
                pool_types,
            );

            let res = deployer.call_raw().block(block_number).await?;
            let pool_data_list = <Vec<PoolData> as SolValue>::abi_decode(&res)?;

            for data in pool_data_list {
                let pool_addr = data.poolAddress;
                let tokens = data.tokens;
                let balances = data.balances;
                let decimals = data.decimals;
                let weights = data.weights;
                let amp_val = data.amp;
                let swap_fee = data.swapFee;
                let bpt_idx_val = data.bptIndex;
                let rate_providers = data.rateProviders;
                let rates = data.rates;

                if let Some(AMM::BalancerV2Pool(pool)) = amms_map.get_mut(&pool_addr) {
                    pool.swap_fee = swap_fee;

                    for (i, &token_addr) in tokens.iter().enumerate() {
                        let balance = balances[i];
                        let decimal = if i < decimals.len() {
                            decimals[i] as u8
                        } else {
                            0
                        };

                        if decimal == 0 {
                            tracing::warn!(
                                ?pool_addr,
                                ?token_addr,
                                "Skipping token with 0 decimals in Balancer V2 pool"
                            );
                            continue;
                        }
                        let weight = if pool.pool_type == BalancerV2PoolType::Weighted
                            && i < weights.len()
                        {
                            Some(weights[i])
                        } else {
                            None
                        };

                        let rate_provider = if i < rate_providers.len() {
                            let rp = rate_providers[i];
                            if rp.is_zero() {
                                None
                            } else {
                                Some(rp)
                            }
                        } else {
                            None
                        };

                        let rate = if i < rates.len() {
                            let r = rates[i];
                            if r.is_zero() {
                                None
                            } else {
                                Some(r)
                            }
                        } else {
                            None
                        };

                        if let Some(state) = pool.tokens.get_mut(&token_addr) {
                            state.balance = balance;
                            state.decimals = decimal;
                            state.weight = weight;
                            state.rate_provider = rate_provider;
                            state.rate = rate;
                        }
                    }

                    // Update Amp (Simplified: just setting current value as static)
                    if matches!(
                        pool.pool_type,
                        BalancerV2PoolType::Stable | BalancerV2PoolType::ComposableStable
                    ) {
                        pool.amp_state = Some(AmpState {
                            initial_value: amp_val,
                            end_value: amp_val,
                            start_time: U256::ZERO,
                            end_time: U256::ZERO,
                        });
                    }

                    // Update BPT Index
                    if pool.pool_type == BalancerV2PoolType::ComposableStable {
                        if bpt_idx_val != U256::MAX {
                            pool.bpt_index = Some(bpt_idx_val.to::<usize>());
                        }
                    }
                }
            }

            // 批次间延迟，避免 RPS 超限
            sleep(Duration::from_millis(100)).await;
        }

        Ok(amms_map.into_values().collect())
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

        let vault_address = if let AMM::BalancerV2Pool(pool) = &amms[0] {
            pool.vault_address
        } else {
            return Err(AMMError::IncompatibleAMMVariant);
        };

        let step = 10; // 减小批次大小以避免 max code size exceeded

        let mut amms_map = amms
            .into_iter()
            .map(|amm| (amm.address(), amm))
            .collect::<HashMap<_, _>>();

        // 收集所有地址用于分批处理
        let addresses: Vec<Address> = amms_map.keys().copied().collect();
        let address_chunks: Vec<_> = addresses.chunks(step).collect();

        // 串行处理每个批次，避免并发请求过多
        for chunk in address_chunks {
            let mut pool_ids = Vec::new();
            let mut pool_addresses = Vec::new();
            let mut pool_types = Vec::new();

            for &addr in chunk {
                if let Some(AMM::BalancerV2Pool(pool)) = amms_map.get(&addr) {
                    pool_ids.push(pool.pool_id);
                    pool_addresses.push(pool.address);
                    let type_u16: u16 = match pool.pool_type {
                        BalancerV2PoolType::Weighted => 0,
                        BalancerV2PoolType::Stable => 1,
                        BalancerV2PoolType::ComposableStable => 2,
                    };
                    pool_types.push(type_u16);
                }
            }

            let deployer = IGetBalancerV2PoolDataBatchRequest::deploy_builder(
                provider.clone(),
                vault_address,
                pool_ids,
                pool_addresses,
                pool_types,
            );

            let res = deployer.call_raw().block(block_number).await?;
            let pool_data_list = <Vec<PoolData> as SolValue>::abi_decode(&res)?;

            for data in pool_data_list {
                let pool_addr = data.poolAddress;
                let tokens = data.tokens;
                let balances = data.balances;
                let decimals = data.decimals;
                let weights = data.weights;
                let amp_val = data.amp;
                let swap_fee = data.swapFee;
                let bpt_idx_val = data.bptIndex;
                let rate_providers = data.rateProviders;
                let rates = data.rates;

                if let Some(AMM::BalancerV2Pool(pool)) = amms_map.get_mut(&pool_addr) {
                    pool.swap_fee = swap_fee;

                    pool.tokens.clear();
                    pool.token_list.clear();

                    for (i, &token_addr) in tokens.iter().enumerate() {
                        let balance = balances[i];
                        let weight = if pool.pool_type == BalancerV2PoolType::Weighted
                            && i < weights.len()
                        {
                            Some(weights[i])
                        } else {
                            None
                        };

                        let decimal = if i < decimals.len() {
                            decimals[i] as u8
                        } else {
                            // Missing decimal data - CRITICAL FAILURE
                            tracing::warn!(
                                target = "amms::balancer_v2::factory",
                                pool = ?pool.address,
                                token_index = i,
                                "Skipping pool due to missing decimal data for token"
                            );
                            // Do not default to 18. Abort this pool.
                            pool.tokens.clear();
                            pool.token_list.clear();
                            break;
                        };

                        // Also skip if decimal is 0 (unlikely for valid tokens, but safety check)
                        if decimal == 0 {
                            tracing::warn!(
                                target = "amms::balancer_v2::factory",
                                pool = ?pool.address,
                                token_index = i,
                                "Skipping pool due to zero decimal for token"
                            );
                            pool.tokens.clear();
                            pool.token_list.clear();
                            break;
                        }

                        let rate_provider = if i < rate_providers.len() {
                            let rp = rate_providers[i];
                            if rp.is_zero() {
                                None
                            } else {
                                Some(rp)
                            }
                        } else {
                            None
                        };

                        let rate = if i < rates.len() {
                            let r = rates[i];
                            if r.is_zero() {
                                None
                            } else {
                                Some(r)
                            }
                        } else {
                            None
                        };

                        pool.tokens.insert(
                            token_addr,
                            TokenState {
                                address: token_addr,
                                balance,
                                decimals: decimal,
                                weight,
                                rate_provider,
                                rate,
                                index: i,
                            },
                        );
                        pool.token_list.push(token_addr);
                    }

                    // Update Amp (Simplified: just setting current value as static)
                    if matches!(
                        pool.pool_type,
                        BalancerV2PoolType::Stable | BalancerV2PoolType::ComposableStable
                    ) {
                        pool.amp_state = Some(AmpState {
                            initial_value: amp_val,
                            end_value: amp_val,
                            start_time: U256::ZERO,
                            end_time: U256::ZERO,
                        });
                    }

                    // Update BPT Index
                    if pool.pool_type == BalancerV2PoolType::ComposableStable {
                        if bpt_idx_val != U256::MAX {
                            pool.bpt_index = Some(bpt_idx_val.to::<usize>());
                        }
                    }

                    pool.update_spot_prices();
                }
            }

            // 批次间延迟，避免 RPS 超限
            sleep(Duration::from_millis(100)).await;
        }

        Ok(amms_map.into_values().collect())
    }
}

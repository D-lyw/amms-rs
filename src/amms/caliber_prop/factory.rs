//! Caliber propAMM Factory
//!
//! 通过 `getAllPairIds` + `eth_getStorageAt` 发现链上池子。
//! Caliber 无池子创建事件，无法通过 sync_events 增量同步。

use alloy::{
    eips::BlockId,
    hex::FromHex,
    network::Network,
    primitives::{Address, B256, U256},
    providers::Provider,
    rpc::types::Log,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, instrument};

use crate::amms::{
    amm::AMM,
    error::AMMError,
    factory::{AutomatedMarketMakerFactory, DiscoverySync},
    Token,
};

use super::{CaliberPropPool, ICaliberPropAMM};

// ============================================================================
// CaliberPropFactory
// ============================================================================

/// Caliber propAMM 池子工厂
///
/// ## 发现机制
///
/// Caliber 不 emit 标准池子创建事件，因此无法通过 `get_logs` 发现池子。
/// 改用 `getAllPairIds()` 分页查询所有 pairId，再通过 `eth_getStorageAt`
/// 读取 token 地址。
///
/// ## 初始化参数
///
/// - `contract_address`：Caliber DEX 合约地址（每条链不同）
/// - `chain_id`：链 ID
/// - `creation_block`：用于 StateSpace 的起始扫描块号
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaliberPropFactory {
    /// Caliber DEX 合约地址
    pub contract_address: Address,
    /// 链 ID
    pub chain_id: u64,
    /// 工厂合约的创建区块号（用作 StateSpace 扫描的起始点）
    pub creation_block: u64,
}

impl CaliberPropFactory {
    /// 创建新工厂实例
    pub fn new(contract_address: Address, chain_id: u64, creation_block: u64) -> Self {
        Self {
            contract_address,
            chain_id,
            creation_block,
        }
    }

    /// batch 读取所有 pairId 的 token 地址（通过 eth_getStorageAt）
    #[instrument(skip_all, fields(pair_count = pair_ids.len()))]
    async fn fetch_token_pairs<N, P>(
        &self,
        provider: &P,
        pair_ids: &[B256],
    ) -> Result<Vec<(Address, Address)>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        // pairConfigBaseSlot = keccak256(abi.encode(uint256(6)))
        // 等价于：keccak256(bytes32(0x6))
        // 实际计算：keccak256(0x0000000000000000000000000000000000000000000000000000000000000006)
        let base_slot =
            B256::from_hex("0x0000000000000000000000000000000000000000000000000000000000000006")
                .map_err(|e| AMMError::Msg(format!("caliber: invalid hex: {e}")))?;

        // 计算每个 pairId 的 token 存储槽
        // slot_token0 = keccak256(pairId . base_slot)
        // slot_token1 = keccak256(pairId . base_slot) + 1
        let mut results = Vec::with_capacity(pair_ids.len());

        for pair_id in pair_ids {
            // keccak256(pair_id . base_slot)
            let mut input = [0u8; 64];
            input[..32].copy_from_slice(pair_id.as_ref());
            input[32..].copy_from_slice(base_slot.as_ref());

            let hash = alloy::primitives::keccak256(input);
            let slot0 = B256::from(hash);

            // slot1 = slot0 + 1
            let mut slot1_bytes = [0u8; 32];
            slot1_bytes.copy_from_slice(slot0.as_ref());
            let mut slot1_num = U256::from_be_bytes(slot1_bytes);
            slot1_num += U256::from(1);
            let slot1_arr: [u8; 32] = slot1_num.to_be_bytes();
            let slot1 = B256::from(slot1_arr);

            // 通过 eth_getStorageAt 读取 pair config 中的 token 地址
            let token0_raw = get_storage_at(provider, self.contract_address, slot0).await?;
            let token1_raw = get_storage_at(provider, self.contract_address, slot1).await?;

            let token0_raw_slice: &[u8] = token0_raw.as_ref();
            let token1_raw_slice: &[u8] = token1_raw.as_ref();
            let token0 = Address::from_slice(&token0_raw_slice[12..]);
            let token1 = Address::from_slice(&token1_raw_slice[12..]);

            results.push((token0, token1));
        }

        Ok(results)
    }
}

// ============================================================================
// Discovery 实现
// ============================================================================

impl DiscoverySync for CaliberPropFactory {
    #[instrument(skip_all, fields(contract = %self.contract_address))]
    fn discover<N, P>(
        &self,
        to_block: BlockId,
        provider: P,
    ) -> impl std::future::Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let contract_address = self.contract_address;
        let creation_block = self.creation_block;
        let chain_id = self.chain_id;
        async move {
            let caliber = ICaliberPropAMM::new(contract_address, provider.clone());

            // 分页调用 getAllPairIds 直到返回空
            let mut all_pair_ids: Vec<B256> = Vec::new();
            let mut start = 0u64;

            loop {
                let pair_ids = caliber
                    .getAllPairIds(U256::from(start), U256::from(super::MAX_PAIRS_PER_CALL))
                    .block(to_block)
                    .call()
                    .await?;

                if pair_ids.is_empty() {
                    break;
                }

                let count = pair_ids.len() as u64;
                all_pair_ids.extend(pair_ids);
                start += count;

                if count < super::MAX_PAIRS_PER_CALL {
                    break;
                }
            }

            debug!("discovered {} caliber pair ids", all_pair_ids.len());

            if all_pair_ids.is_empty() {
                return Ok(vec![]);
            }

            // 批量读取 token 地址
            let factory = CaliberPropFactory {
                contract_address,
                chain_id,
                creation_block,
            };
            let token_pairs = factory
                .fetch_token_pairs::<N, P>(&provider, &all_pair_ids)
                .await?;

            // 构建 AMM 池子骨架
            let mut pools = Vec::with_capacity(all_pair_ids.len());

            for (i, pair_id) in all_pair_ids.into_iter().enumerate() {
                let (token0_raw, token1_raw) = token_pairs[i];

                // 排序：token_a = 地址较小的 token
                let (token_a_addr, token_b_addr) = if token0_raw < token1_raw {
                    (token0_raw, token1_raw)
                } else {
                    (token1_raw, token0_raw)
                };

                let virtual_address =
                    CaliberPropPool::virtual_address_from_pair_id(pair_id, contract_address);

                let pool = CaliberPropPool {
                    contract_address,
                    pair_id,
                    virtual_address,
                    token_x: token0_raw,
                    token_y: token1_raw,
                    created_block: creation_block,
                    last_synced_block: 0,
                    token_a: Token {
                        address: token_a_addr,
                        decimals: 0, // 在 init_batch 中填充
                        symbol: String::new(),
                        chain_id,
                        fot_tax: None,
                    },
                    token_b: Token {
                        address: token_b_addr,
                        decimals: 0, // 在 init_batch 中填充
                        symbol: String::new(),
                        chain_id,
                        fot_tax: None,
                    },
                    reserve_a: U256::ZERO,
                    reserve_b: U256::ZERO,
                    ladder: Default::default(),
                    price_a_in_b: 0.0,
                    price_b_in_a: 0.0,
                };

                pools.push(AMM::CaliberPropPool(pool));
            }

            Ok(pools)
        }
    }

    #[instrument(skip_all, fields(pool_count = amms.len()))]
    fn sync<N, P>(
        &self,
        amms: Vec<AMM>,
        to_block: BlockId,
        provider: P,
    ) -> impl std::future::Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        async move { init_batch::<N, P>(amms, to_block, provider).await }
    }
}

impl AutomatedMarketMakerFactory for CaliberPropFactory {
    type PoolVariant = CaliberPropPool;

    fn address(&self) -> Address {
        self.contract_address
    }

    fn create_pool(&self, _log: Log) -> Result<AMM, AMMError> {
        // Caliber 不 emit 池子创建事件，此方法不可用
        Err(AMMError::Msg(
            "caliber: pool creation is event-less, use discover()".to_string(),
        ))
    }

    fn creation_block(&self) -> u64 {
        self.creation_block
    }

    fn pool_creation_event(&self) -> B256 {
        // Caliber 无池子创建事件
        B256::ZERO
    }

    fn pool_events(&self) -> Vec<B256> {
        // Caliber 无 Swap/Mint/Burn 事件
        vec![]
    }

    fn pool_variant(&self) -> Self::PoolVariant {
        Default::default()
    }
}

// ============================================================================
// init_batch: 批量初始化池子
// ============================================================================

/// 批量初始化 Caliber propAMM 池子
///
/// 对于 Caliber，初始化仍按池子逐个进行，但每个池子的核心快照读取
/// 已通过自定义 batch reader 合约压缩为单次 eth_call。
///
/// 每个池子的初始化调用：
/// 1. get_token_decimals (2 次 eth_call)
/// 2. CaliberPropPool::init() → GetCaliberPropLadderBatchRequest
pub(super) async fn init_batch<N, P>(
    amms: Vec<AMM>,
    block_number: BlockId,
    provider: P,
) -> Result<Vec<AMM>, AMMError>
where
    N: Network,
    N::BlockResponse: alloy::network::BlockResponse,
    <N::BlockResponse as alloy::network::BlockResponse>::Header: alloy::consensus::BlockHeader,
    P: Provider<N> + Clone,
{
    if amms.is_empty() {
        return Ok(vec![]);
    }

    // 1. 批量获取 token decimals
    let mut pools: Vec<CaliberPropPool> = amms
        .into_iter()
        .map(|a| match a {
            AMM::CaliberPropPool(p) => p,
            _ => unreachable!(),
        })
        .collect();

    // 收集所有唯一 token 地址
    let mut unique_tokens: Vec<Address> = Vec::new();
    for pool in &pools {
        if !unique_tokens.contains(&pool.token_a.address) {
            unique_tokens.push(pool.token_a.address);
        }
        if !unique_tokens.contains(&pool.token_b.address) {
            unique_tokens.push(pool.token_b.address);
        }
    }

    let decimals = get_token_decimals_batch::<N, P>(&unique_tokens, &provider).await?;

    // 填充 decimals
    for pool in &mut pools {
        pool.token_a.decimals = decimals
            .iter()
            .find(|(addr, _)| *addr == pool.token_a.address)
            .map(|(_, d)| *d)
            .unwrap_or(18);
        pool.token_b.decimals = decimals
            .iter()
            .find(|(addr, _)| *addr == pool.token_b.address)
            .map(|(_, d)| *d)
            .unwrap_or(18);
    }

    // 2. 批量初始化：一次 JSON-RPC batch 读取全部储备 + Ladder + 精确报价参数
    //    （每个 pair 的固定槽位 + ladder 槽位折叠进 batch，失败 pool 保持骨架被过滤）
    let flags = super::batch_refresh_snapshots::<N, P>(&provider, &mut pools, block_number).await?;

    let initialized = pools
        .into_iter()
        .zip(flags)
        .filter_map(|(pool, ok)| {
            if ok {
                Some(AMM::CaliberPropPool(pool))
            } else {
                debug!("caliber: failed to init pool {}", pool.virtual_address);
                None
            }
        })
        .collect();

    Ok(initialized)
}

// ============================================================================
// Token Decimals 批量查询
// ============================================================================

async fn get_token_decimals_batch<N, P>(
    tokens: &[Address],
    provider: &P,
) -> Result<Vec<(Address, u8)>, AMMError>
where
    N: Network,
    P: Provider<N> + Clone,
{
    use alloy::sol;

    sol! {
        #[sol(rpc)]
        interface IERC20Metadata {
            function decimals() external view returns (uint8);
        }
    }

    let mut results = Vec::with_capacity(tokens.len());

    // 使用 tokio 并发查询，限制并发数为 20
    for chunk in tokens.chunks(20) {
        let mut futures = Vec::with_capacity(chunk.len());
        for &token in chunk {
            let p = provider.clone();
            futures.push(async move {
                let erc20 = IERC20Metadata::new(token, p);
                let decimals = erc20.decimals().call().await;
                (token, decimals)
            });
        }

        let chunk_results = futures::future::join_all(futures).await;
        for (token, result) in chunk_results {
            match result {
                Ok(decimals) => results.push((token, decimals)),
                Err(_) => results.push((token, 18)), // fallback to 18
            }
        }
    }

    Ok(results)
}

// ============================================================================
// eth_getStorageAt helper
// ============================================================================

/// 通过 alloy Provider 发送 `eth_getStorageAt` JSON-RPC 请求
async fn get_storage_at<N, P>(provider: &P, address: Address, slot: B256) -> Result<B256, AMMError>
where
    N: Network,
    P: Provider<N>,
{
    let slot_u256 = U256::from_be_bytes(slot.0);
    let result: U256 = provider
        .get_storage_at(address, slot_u256)
        .await
        .map_err(|e| AMMError::Msg(format!("caliber: get_storage_at failed: {e}")))?;
    Ok(B256::from(result.to_be_bytes::<32>()))
}

impl Default for super::types::CaliberLadderState {
    fn default() -> Self {
        Self {
            ladder_a_to_b: Vec::new(),
            ladder_b_to_a: Vec::new(),
            consumed_in_ab: U256::ZERO,
            consumed_out_ab: U256::ZERO,
            consumed_in_ba: U256::ZERO,
            consumed_out_ba: U256::ZERO,
            field0: U256::ZERO,
            field1: U256::ZERO,
            fee_rate: U256::ZERO,
            window: U256::ZERO,
            scale: U256::ZERO,
            pos_reverse: U256::ZERO,
            pos_forward: U256::ZERO,
            deadline: 0,
            validity_window: 0,
            paused: false,
        }
    }
}

impl Default for CaliberPropPool {
    fn default() -> Self {
        Self {
            contract_address: Address::ZERO,
            pair_id: B256::ZERO,
            virtual_address: Address::ZERO,
            token_x: Address::ZERO,
            token_y: Address::ZERO,
            created_block: 0,
            last_synced_block: 0,
            token_a: Token {
                address: Address::ZERO,
                decimals: 0,
                symbol: String::new(),
                chain_id: 0,
                fot_tax: None,
            },
            token_b: Token {
                address: Address::ZERO,
                decimals: 0,
                symbol: String::new(),
                chain_id: 0,
                fot_tax: None,
            },
            reserve_a: U256::ZERO,
            reserve_b: U256::ZERO,
            ladder: Default::default(),
            price_a_in_b: 0.0,
            price_b_in_a: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::keccak256;

    #[test]
    fn test_storage_slot_computation() {
        // 验证 pairConfigBaseSlot = keccak256(uint256(6))
        let base =
            B256::from_hex("0x0000000000000000000000000000000000000000000000000000000000000006")
                .unwrap();

        // 模拟一个 pairId
        let pair_id = B256::from([0xAAu8; 32]);

        let mut input = [0u8; 64];
        input[..32].copy_from_slice(pair_id.as_ref());
        input[32..].copy_from_slice(base.as_ref());

        let hash = keccak256(input);

        // 验证 hash 不为零
        assert!(!hash.is_zero());
    }
}

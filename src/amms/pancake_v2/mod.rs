use super::{
    amm::{AutomatedMarketMaker, SyncAction, AMM},
    consts::{MIN_POOL_RESERVE, MPFR_T_PRECISION, U128_0X10000000000000000, U256_100000},
    error::AMMError,
    factory::{AutomatedMarketMakerFactory, DiscoverySync},
    uniswap_v2::{
        div_uu, IGetUniswapV2PoolDataBatchRequestInstance, IUniswapV2Factory, IUniswapV2Pair,
        UniswapV2Factory,
    },
    Token,
};
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, Bytes, B256, U256},
    providers::Provider,
    rpc::types::Log,
    sol_types::{SolCall, SolEvent, SolValue},
};
use futures::{stream::FuturesUnordered, StreamExt};
use rug::ops::Pow;
use rug::Float;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use tracing::info;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PancakeV2Pool {
    pub address: Address,
    #[serde(default)]
    pub last_synced_block: u64,
    pub token_a: Token,
    pub token_b: Token,
    pub reserve_0: u128,
    pub reserve_1: u128,
    pub fee: usize,
    #[serde(default)]
    pub token_a_price: f64,
    #[serde(default)]
    pub token_b_price: f64,
}

impl Hash for PancakeV2Pool {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.address.hash(state);
    }
}

impl PartialEq for PancakeV2Pool {
    fn eq(&self, other: &Self) -> bool {
        self.address == other.address
    }
}

impl Eq for PancakeV2Pool {}

impl PancakeV2Pool {
    /// Creates a new PancakeV2Pool with the standard 0.25% fee.
    /// PancakeSwap V2 uses a fixed fee of 0.25%.
    pub fn new(address: Address) -> Self {
        Self {
            address,
            fee: 250, // 0.25% = 250 / 100000
            token_a_price: 0.0,
            token_b_price: 0.0,
            ..Default::default()
        }
    }

    pub fn get_amount_out(&self, amount_in: U256, reserve_in: U256, reserve_out: U256) -> U256 {
        if amount_in.is_zero() || reserve_in.is_zero() || reserve_out.is_zero() {
            return U256::ZERO;
        }
        let fee = U256_100000 - U256::from(self.fee);
        let amount_in = amount_in * fee;
        let numerator = amount_in * reserve_out;
        let denominator = reserve_in * U256_100000 + amount_in;

        numerator / denominator
    }

    pub fn calculate_price_64_x_64(&self, base_token: Address) -> Result<u128, AMMError> {
        let decimal_shift = self.token_a.decimals as i8 - self.token_b.decimals as i8;

        let (r_0, r_1) = if decimal_shift < 0 {
            (
                U256::from(self.reserve_0)
                    * U256::from(10u128.pow(decimal_shift.unsigned_abs() as u32)),
                U256::from(self.reserve_1),
            )
        } else {
            (
                U256::from(self.reserve_0),
                U256::from(self.reserve_1) * U256::from(10u128.pow(decimal_shift as u32)),
            )
        };

        if base_token == self.token_a.address {
            if r_0.is_zero() {
                Ok(U128_0X10000000000000000)
            } else {
                div_uu(r_1, r_0)
            }
        } else if r_1.is_zero() {
            Ok(U128_0X10000000000000000)
        } else {
            div_uu(r_0, r_1)
        }
    }

    pub fn swap_calldata(
        &self,
        amount_0_out: U256,
        amount_1_out: U256,
        to: Address,
        calldata: Vec<u8>,
    ) -> Result<Bytes, AMMError> {
        Ok(IUniswapV2Pair::swapCall {
            amount0Out: amount_0_out,
            amount1Out: amount_1_out,
            to,
            data: calldata.into(),
        }
        .abi_encode()
        .into())
    }
}

impl AutomatedMarketMaker for PancakeV2Pool {
    fn address(&self) -> Address {
        self.address
    }

    fn last_synced_block(&self) -> u64 {
        self.last_synced_block
    }

    fn set_last_synced_block(&mut self, block_number: u64) {
        self.last_synced_block = self.last_synced_block.max(block_number);
    }

    fn sync_events(&self) -> Vec<B256> {
        vec![IUniswapV2Pair::Sync::SIGNATURE_HASH]
    }

    fn tokens(&self) -> Vec<Address> {
        vec![self.token_a.address, self.token_b.address]
    }

    fn has_sufficient_liquidity(&self) -> bool {
        self.token_a.has_sufficient_liquidity(self.reserve_0)
            && self.token_b.has_sufficient_liquidity(self.reserve_1)
    }

    fn decimals(&self, token: Address) -> u8 {
        if token == self.token_a.address {
            self.token_a.decimals
        } else if token == self.token_b.address {
            self.token_b.decimals
        } else {
            0
        }
    }

    fn calculate_price(&self, base_token: Address, _quote_token: Address) -> Result<f64, AMMError> {
        if self.reserve_0 < MIN_POOL_RESERVE || self.reserve_1 < MIN_POOL_RESERVE {
            return Ok(0.0);
        }

        let r0_str = self.reserve_0.to_string();
        let r0_val = Float::parse_radix(&r0_str, 10)
            .map_err(|e| AMMError::Msg(format!("Float parse error: {}", e)))?;
        let r0 = Float::with_val(MPFR_T_PRECISION, r0_val);

        let r1_str = self.reserve_1.to_string();
        let r1_val = Float::parse_radix(&r1_str, 10)
            .map_err(|e| AMMError::Msg(format!("Float parse error: {}", e)))?;
        let r1 = Float::with_val(MPFR_T_PRECISION, r1_val);

        let shift = self.token_a.decimals as i32 - self.token_b.decimals as i32;
        let scale_factor = Float::with_val(MPFR_T_PRECISION, 10).pow(shift);

        let price_a: Float = (r1 / r0) * scale_factor;
        let price_a_f64 = price_a.to_f64();

        if base_token == self.token_a.address {
            Ok(price_a_f64)
        } else {
            if price_a_f64 == 0.0 {
                Ok(0.0)
            } else {
                Ok(1.0 / price_a_f64)
            }
        }
    }

    fn spot_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
        // 必须验证 BOTH base_token AND quote_token 都存在于池子中
        let base_is_a = base_token == self.token_a.address;
        let base_is_b = base_token == self.token_b.address;
        let quote_is_a = quote_token == self.token_a.address;
        let quote_is_b = quote_token == self.token_b.address;

        if !base_is_a && !base_is_b {
            return Err(AMMError::TokenNotFound(base_token));
        }
        if !quote_is_a && !quote_is_b {
            return Err(AMMError::TokenNotFound(quote_token));
        }
        if base_token == quote_token {
            return Err(AMMError::Msg(
                "base and quote tokens are the same".to_string(),
            ));
        }

        let price = if base_is_a {
            self.token_a_price
        } else {
            self.token_b_price
        };

        // 价格有效性校验：0 或非有限值表示价格未初始化或计算失败
        if price <= 0.0 || !price.is_finite() {
            return Err(AMMError::Msg("Invalid cached spot price".to_string()));
        }

        Ok(price)
    }

    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError> {
        let signature = log.topics()[0];
        if signature == IUniswapV2Pair::Sync::SIGNATURE_HASH {
            let sync_event = IUniswapV2Pair::Sync::decode_log(&log.inner)?;
            let (reserve_0, reserve_1) = (
                sync_event.reserve0.to::<u128>(),
                sync_event.reserve1.to::<u128>(),
            );
            info!(target = "amm::pancake_v2::sync", block_number = ?log.block_number, address = ?self.address, reserve_0, reserve_1, "Sync");
            self.reserve_0 = reserve_0;
            self.reserve_1 = reserve_1;

            if let Ok(p) = self.calculate_price(self.token_a.address, self.token_b.address) {
                self.token_a_price = p;
                if p != 0.0 {
                    self.token_b_price = 1.0 / p;
                } else {
                    self.token_b_price = 0.0;
                }
            }
        }
        Ok(SyncAction::None)
    }

    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let deployer = IGetUniswapV2PoolDataBatchRequestInstance::deploy_builder(
            provider.clone(),
            vec![self.address()],
        );
        let res = deployer.call_raw().block(block_number).await?;
        let pool_data =
            <Vec<(Address, Address, u128, u128, u32, u32)> as SolValue>::abi_decode(&res)?[0];
        if pool_data.0.is_zero() {
            return Err(AMMError::SyncError(self.address));
        }
        self.token_a = Token::new_with_decimals(pool_data.0, pool_data.4 as u8);
        self.token_b = Token::new_with_decimals(pool_data.1, pool_data.5 as u8);
        self.reserve_0 = pool_data.2;
        self.reserve_1 = pool_data.3;
        // PancakeV2 uses fixed 0.25% fee
        if self.fee == 0 {
            self.fee = 250; // 0.25%
        }

        if let Ok(p) = self.calculate_price(self.token_a.address, self.token_b.address) {
            self.token_a_price = p;
            if p != 0.0 {
                self.token_b_price = 1.0 / p;
            } else {
                self.token_b_price = 0.0;
            }
        }

        Ok(self)
    }

    fn simulate_swap(
        &self,
        base_token: Address,
        _quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        if self.token_a.address == base_token {
            Ok(self.get_amount_out(
                amount_in,
                U256::from(self.reserve_0),
                U256::from(self.reserve_1),
            ))
        } else {
            Ok(self.get_amount_out(
                amount_in,
                U256::from(self.reserve_1),
                U256::from(self.reserve_0),
            ))
        }
    }

    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        _quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        if self.token_a.address == base_token {
            let amount_out = self.get_amount_out(
                amount_in,
                U256::from(self.reserve_0),
                U256::from(self.reserve_1),
            );

            self.reserve_0 += amount_in.to::<u128>();
            self.reserve_1 -= amount_out.to::<u128>();

            Ok(amount_out)
        } else {
            let amount_out = self.get_amount_out(
                amount_in,
                U256::from(self.reserve_1),
                U256::from(self.reserve_0),
            );

            self.reserve_0 -= amount_out.to::<u128>();
            self.reserve_1 += amount_in.to::<u128>();

            Ok(amount_out)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct PancakeV2Factory {
    pub address: Address,
    pub fee: usize,
    pub creation_block: u64,
}

impl PancakeV2Factory {
    pub fn new(address: Address, fee: usize, creation_block: u64) -> Self {
        Self {
            address,
            fee,
            creation_block,
        }
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

        let step = 120;
        let pairs = amms
            .iter()
            .map(|amm| amm.address())
            .collect::<Vec<Address>>();

        let pair_chunks = pairs
            .chunks(step)
            .map(|chunk| chunk.to_vec())
            .collect::<Vec<_>>();

        let mut futures_unordered = FuturesUnordered::new();
        for group in pair_chunks {
            let deployer = IGetUniswapV2PoolDataBatchRequestInstance::deploy_builder(
                provider.clone(),
                group.clone(),
            );

            futures_unordered.push(async move {
                let res = deployer.call_raw().block(block_number).await?;

                let return_data =
                    <Vec<(Address, Address, u128, u128, u32, u32)> as SolValue>::abi_decode(&res)?;

                Ok::<(Vec<Address>, Vec<(Address, Address, u128, u128, u32, u32)>), AMMError>((
                    group,
                    return_data,
                ))
            });
        }

        let mut amms_map = amms
            .into_iter()
            .map(|amm| (amm.address(), amm))
            .collect::<HashMap<_, _>>();

        while let Some(res) = futures_unordered.next().await {
            let (group, return_data) = res?;
            for (pool_data, pool_address) in return_data.iter().zip(group.iter()) {
                if pool_data.0.is_zero() {
                    continue;
                }

                let amm = amms_map.get_mut(pool_address).unwrap();

                let AMM::PancakeV2Pool(pool) = amm else {
                    continue;
                };

                if pool.token_a.address.is_zero() || pool.token_b.address.is_zero() {
                    pool.token_a = Token::new_with_decimals(pool_data.0, pool_data.4 as u8);
                    pool.token_b = Token::new_with_decimals(pool_data.1, pool_data.5 as u8);
                } else {
                    if pool.token_a.decimals == 0 {
                        pool.token_a.decimals = pool_data.4 as u8;
                    }
                    if pool.token_b.decimals == 0 {
                        pool.token_b.decimals = pool_data.5 as u8;
                    }
                }

                pool.reserve_0 = pool_data.2;
                pool.reserve_1 = pool_data.3;

                if pool.fee == 0 {
                    pool.fee = 250;
                }
            }
        }

        let (valid_amms, invalid_amms): (Vec<_>, Vec<_>) = amms_map
            .into_iter()
            .partition(|(_, amm)| !amm.tokens().iter().any(|t| t.is_zero()));

        if !invalid_amms.is_empty() {
            for (_, amm) in &invalid_amms {
                info!(
                    target: "amms::pancake_v2::sync",
                    address = ?amm.address(),
                    tokens = ?amm.tokens(),
                    "Filtering out V2 pool with zero address token"
                );
            }
        }

        let amms = valid_amms.into_iter().map(|(_, amm)| amm).collect();

        Ok(amms)
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

        let step = 120;
        let pairs = amms
            .iter()
            .map(|amm| amm.address())
            .collect::<Vec<Address>>();

        let pair_chunks = pairs
            .chunks(step)
            .map(|chunk| chunk.to_vec())
            .collect::<Vec<_>>();

        let mut futures_unordered = FuturesUnordered::new();
        for group in pair_chunks {
            let deployer = IGetUniswapV2PoolDataBatchRequestInstance::deploy_builder(
                provider.clone(),
                group.clone(),
            );

            futures_unordered.push(async move {
                let res = deployer.call_raw().block(block_number).await?;

                let return_data =
                    <Vec<(Address, Address, u128, u128, u32, u32)> as SolValue>::abi_decode(&res)?;

                Ok::<(Vec<Address>, Vec<(Address, Address, u128, u128, u32, u32)>), AMMError>((
                    group,
                    return_data,
                ))
            });
        }

        let mut amms_map = amms
            .into_iter()
            .map(|amm| (amm.address(), amm))
            .collect::<HashMap<_, _>>();

        while let Some(res) = futures_unordered.next().await {
            let (group, return_data) = res?;
            for (pool_data, pool_address) in return_data.iter().zip(group.iter()) {
                if pool_data.0.is_zero() {
                    continue;
                }

                let amm = amms_map.get_mut(pool_address).unwrap();

                let AMM::PancakeV2Pool(pool) = amm else {
                    continue;
                };

                pool.token_a = Token::new_with_decimals(pool_data.0, pool_data.4 as u8);
                pool.token_b = Token::new_with_decimals(pool_data.1, pool_data.5 as u8);
                pool.reserve_0 = pool_data.2;
                pool.reserve_1 = pool_data.3;
                if pool.fee == 0 {
                    pool.fee = 250;
                }

                // Init prices
                if let Ok(pa) = pool.calculate_price(pool.token_a.address, pool.token_b.address) {
                    pool.token_a_price = pa;
                    if pa != 0.0 {
                        pool.token_b_price = 1.0 / pa;
                    }
                }
            }
        }

        let (valid_amms, invalid_amms): (Vec<_>, Vec<_>) = amms_map
            .into_iter()
            .partition(|(_, amm)| !amm.tokens().iter().any(|t| t.is_zero()));

        if !invalid_amms.is_empty() {
            for (_, amm) in &invalid_amms {
                info!(
                    target: "amms::pancake_v2::init_batch",
                    address = ?amm.address(),
                    tokens = ?amm.tokens(),
                    "Filtering out V2 pool with zero address token"
                );
            }
        }

        let amms = valid_amms.into_iter().map(|(_, amm)| amm).collect();

        Ok(amms)
    }
}

impl AutomatedMarketMakerFactory for PancakeV2Factory {
    type PoolVariant = PancakeV2Pool;

    fn address(&self) -> Address {
        self.address
    }

    fn pool_creation_event(&self) -> B256 {
        IUniswapV2Factory::PairCreated::SIGNATURE_HASH
    }

    fn create_pool(&self, log: Log) -> Result<AMM, AMMError> {
        let event = IUniswapV2Factory::PairCreated::decode_log(&log.inner)?;
        Ok(AMM::PancakeV2Pool(PancakeV2Pool {
            address: event.pair,
            last_synced_block: 0,
            token_a: event.token0.into(),
            token_b: event.token1.into(),
            reserve_0: 0,
            reserve_1: 0,
            fee: 250, // Fixed 0.25% fee
            token_a_price: 0.0,
            token_b_price: 0.0,
        }))
    }

    fn creation_block(&self) -> u64 {
        self.creation_block
    }
}

impl DiscoverySync for PancakeV2Factory {
    fn discover<N, P>(
        &self,
        to_block: BlockId,
        provider: P,
    ) -> impl Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let address = self.address;
        // Reuse UniswapV2Factory::get_all_pairs but map results to PancakeV2Pool
        let future = UniswapV2Factory::get_all_pairs::<N, _>(address, to_block, provider.clone());

        async move {
            let pairs = future.await?;
            Ok(pairs
                .into_iter()
                .map(|pair| {
                    AMM::PancakeV2Pool(PancakeV2Pool {
                        address: pair,
                        last_synced_block: 0,
                        token_a: Address::default().into(),
                        token_b: Address::default().into(),
                        reserve_0: 0,
                        reserve_1: 0,
                        fee: 250, // Fixed 0.25% fee
                        token_a_price: 0.0,
                        token_b_price: 0.0,
                    })
                })
                .collect())
        }
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
        info!(
            target: "amms::pancake_v2::sync",
            address = ?self.address,
            "Syncing all pools"
        );
        PancakeV2Factory::init_batch(amms, to_block, provider)
    }
}

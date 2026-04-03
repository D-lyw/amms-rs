use super::{
    amm::{AutomatedMarketMaker, SyncAction, AMM},
    consts::{
        MIN_POOL_RESERVE, MPFR_T_PRECISION, U128_0X10000000000000000, U256_0X100, U256_0X10000,
        U256_0X100000000, U256_0XFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF,
        U256_0XFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF, U256_1, U256_100000, U256_128,
        U256_16, U256_191, U256_192, U256_2, U256_255, U256_32, U256_4, U256_64, U256_8,
    },
    error::AMMError,
    factory::{AutomatedMarketMakerFactory, DiscoverySync},
    Token,
};
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, Bytes, B256, U256},
    providers::Provider,
    rpc::types::Log,
    sol,
    sol_types::{SolCall, SolEvent, SolValue},
};
use futures::{stream::FuturesUnordered, StreamExt};
use itertools::Itertools;
use rug::ops::Pow;
use rug::Float;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, future::Future, hash::Hash};
use thiserror::Error;
use tracing::info;
pub use IGetUniswapV2PoolDataBatchRequest::IGetUniswapV2PoolDataBatchRequestInstance;

use IUniswapV2Factory::IUniswapV2FactoryInstance;

sol!(
// UniswapV2Factory
#[allow(missing_docs)]
#[derive(Debug)]
#[sol(rpc)]
contract IUniswapV2Factory {
    event PairCreated(address indexed token0, address indexed token1, address pair, uint256);
    function allPairs(uint256) external view returns (address pair);
    function allPairsLength() external view returns (uint256);

}

#[derive(Debug, PartialEq, Eq)]
#[sol(rpc)]
contract IUniswapV2Pair {
    event Sync(uint112 reserve0, uint112 reserve1);
    function token0() external view returns (address);
    function token1() external view returns (address);
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data);
    function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
});

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    IGetUniswapV2PairsBatchRequest,
    "src/amms/abi/GetUniswapV2PairsBatchRequest.json"
);

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    IGetUniswapV2PoolDataBatchRequest,
    "src/amms/abi/GetUniswapV2PoolDataBatchRequest.json"
);

#[derive(Error, Debug)]
pub enum UniswapV2Error {
    #[error("Division by zero")]
    DivisionByZero,
    #[error("Rounding Error")]
    RoundingError,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UniswapV2Pool {
    pub address: Address,
    #[serde(default)]
    pub last_synced_block: u64,
    pub token_a: Token,
    pub token_b: Token,
    pub reserve_0: u128,
    pub reserve_1: u128,
    pub fee: usize,
    #[serde(default)]
    pub token_a_price: f64, // Price of 1 TokenA in terms of TokenB
    #[serde(default)]
    pub token_b_price: f64, // Price of 1 TokenB in terms of TokenA
}

impl AutomatedMarketMaker for UniswapV2Pool {
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

    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError> {
        let signature = log.topics()[0];
        if signature == IUniswapV2Pair::Sync::SIGNATURE_HASH {
            let sync_event = IUniswapV2Pair::Sync::decode_log(&log.inner)?;

            let (reserve_0, reserve_1) = (
                sync_event.reserve0.to::<u128>(),
                sync_event.reserve1.to::<u128>(),
            );

            info!(
                target = "amms::uniswap_v2::sync",
                block_number = ?log.block_number,
                address = ?self.address,
                reserve_0, reserve_1, "Sync"
            );

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

            let amount_in_u128: u128 = amount_in.try_into().map_err(|_| {
                AMMError::Msg("amount_in exceeds u128 in simulate_swap_mut".to_string())
            })?;
            let amount_out_u128: u128 = amount_out.try_into().map_err(|_| {
                AMMError::Msg("amount_out exceeds u128 in simulate_swap_mut".to_string())
            })?;

            self.reserve_0 = self
                .reserve_0
                .checked_add(amount_in_u128)
                .ok_or(AMMError::Msg("reserve_0 overflow".to_string()))?;
            self.reserve_1 = self
                .reserve_1
                .checked_sub(amount_out_u128)
                .ok_or(AMMError::Msg("reserve_1 underflow".to_string()))?;

            Ok(amount_out)
        } else {
            let amount_out = self.get_amount_out(
                amount_in,
                U256::from(self.reserve_1),
                U256::from(self.reserve_0),
            );

            let amount_in_u128: u128 = amount_in.try_into().map_err(|_| {
                AMMError::Msg("amount_in exceeds u128 in simulate_swap_mut".to_string())
            })?;
            let amount_out_u128: u128 = amount_out.try_into().map_err(|_| {
                AMMError::Msg("amount_out exceeds u128 in simulate_swap_mut".to_string())
            })?;

            self.reserve_0 = self
                .reserve_0
                .checked_sub(amount_out_u128)
                .ok_or(AMMError::Msg("reserve_0 underflow".to_string()))?;
            self.reserve_1 = self
                .reserve_1
                .checked_add(amount_in_u128)
                .ok_or(AMMError::Msg("reserve_1 overflow".to_string()))?;

            Ok(amount_out)
        }
    }

    fn simulate_swap_exact_out(
        &self,
        base_token: Address,
        _quote_token: Address,
        amount_out: U256,
    ) -> Result<U256, AMMError> {
        if self.token_a.address == base_token {
            self.get_amount_in(
                amount_out,
                U256::from(self.reserve_0),
                U256::from(self.reserve_1),
            )
        } else {
            self.get_amount_in(
                amount_out,
                U256::from(self.reserve_1),
                U256::from(self.reserve_0),
            )
        }
    }

    fn tokens(&self) -> Vec<Address> {
        vec![self.token_a.address, self.token_b.address]
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

        // base_token -> quote_token 的价格
        let price = if base_is_a {
            self.token_a_price // token_a -> token_b
        } else {
            self.token_b_price // token_b -> token_a
        };

        // 价格有效性校验：0 或非有限值表示价格未初始化或计算失败
        if price <= 0.0 || !price.is_finite() {
            return Err(AMMError::Msg("Invalid cached spot price".to_string()));
        }

        Ok(price)
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
            todo!("Return error");
        }

        self.token_a = Token::new_with_decimals(pool_data.0, pool_data.4 as u8);
        self.token_b = Token::new_with_decimals(pool_data.1, pool_data.5 as u8);
        self.reserve_0 = pool_data.2;
        self.reserve_1 = pool_data.3;

        // If fee is zero (default), set it to 300 (0.3%) which is the standard Uniswap V2 fee.
        if self.fee == 0 {
            self.fee = 300;
        }

        if self.reserve_0 > 0 && self.reserve_1 > 0 {
            if let Ok(price) = self.calculate_price(self.token_a.address, self.token_b.address) {
                self.token_a_price = price;
                if price != 0.0 {
                    self.token_b_price = 1.0 / price;
                } else {
                    self.token_b_price = 0.0;
                }
            }
        }

        Ok(self)
    }
}

pub fn u128_to_float(num: u128) -> Result<Float, AMMError> {
    let value_string = num.to_string();
    let parsed_value = Float::parse_radix(value_string, 10)?;
    Ok(Float::with_val(MPFR_T_PRECISION, parsed_value))
}

impl UniswapV2Pool {
    // Create a new, unsynced UniswapV2 pool
    // TODO: update the init function to derive the fee
    pub fn new(address: Address) -> Self {
        Self {
            address,
            token_a_price: 0.0,
            token_b_price: 0.0,
            ..Default::default()
        }
    }

    /// Calculates the amount received for a given `amount_in` `reserve_in` and `reserve_out`.
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

    /// Calculates the minimum amount_in required to receive `amount_out`.
    pub fn get_amount_in(
        &self,
        amount_out: U256,
        reserve_in: U256,
        reserve_out: U256,
    ) -> Result<U256, AMMError> {
        if amount_out.is_zero() {
            return Ok(U256::ZERO);
        }
        if reserve_in.is_zero() || reserve_out.is_zero() || amount_out >= reserve_out {
            return Err(AMMError::Msg(
                "insufficient liquidity for exact out".to_string(),
            ));
        }

        let fee_base = U256_100000;
        let fee_factor = fee_base
            .checked_sub(U256::from(self.fee))
            .ok_or(AMMError::ArithmeticError)?;
        if fee_factor.is_zero() {
            return Err(AMMError::ArithmeticError);
        }

        let numerator = reserve_in
            .checked_mul(fee_base)
            .and_then(|v| v.checked_mul(amount_out))
            .ok_or(AMMError::ArithmeticError)?;
        let denominator = reserve_out
            .checked_sub(amount_out)
            .and_then(|v| v.checked_mul(fee_factor))
            .ok_or(AMMError::ArithmeticError)?;

        Ok(Self::ceil_div_u256(numerator, denominator))
    }

    fn ceil_div_u256(numerator: U256, denominator: U256) -> U256 {
        let (q, r) = (numerator / denominator, numerator % denominator);
        if r.is_zero() {
            q
        } else {
            q + U256_1
        }
    }

    /// Calculates the price of the base token in terms of the quote token.
    ///
    /// Returned as a Q64 fixed point number.
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

pub fn div_uu(x: U256, y: U256) -> Result<u128, AMMError> {
    if !y.is_zero() {
        let mut answer;

        if x <= U256_0XFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF {
            answer = (x << U256_64) / y;
        } else {
            let mut msb = U256_192;
            let mut xc = x >> U256_192;

            if xc >= U256_0X100000000 {
                xc >>= U256_32;
                msb += U256_32;
            }

            if xc >= U256_0X10000 {
                xc >>= U256_16;
                msb += U256_16;
            }

            if xc >= U256_0X100 {
                xc >>= U256_8;
                msb += U256_8;
            }

            if xc >= U256_16 {
                xc >>= U256_4;
                msb += U256_4;
            }

            if xc >= U256_4 {
                xc >>= U256_2;
                msb += U256_2;
            }

            if xc >= U256_2 {
                msb += U256_1;
            }

            answer = (x << (U256_255 - msb)) / (((y - U256_1) >> (msb - U256_191)) + U256_1);
        }

        if answer > U256_0XFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF {
            return Ok(0);
        }

        let hi = answer * (y >> U256_128);
        let mut lo = answer * (y & U256_0XFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF);

        let mut xh = x >> U256_192;
        let mut xl = x << U256_64;

        if xl < lo {
            xh -= U256_1;
        }

        xl = xl.overflowing_sub(lo).0;
        lo = hi << U256_128;

        if xl < lo {
            xh -= U256_1;
        }

        xl = xl.overflowing_sub(lo).0;

        if xh != hi >> U256_128 {
            return Err(UniswapV2Error::RoundingError.into());
        }

        answer += xl / y;

        if answer > U256_0XFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF {
            return Ok(0_u128);
        }

        Ok(answer.to::<u128>())
    } else {
        Err(UniswapV2Error::DivisionByZero.into())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct UniswapV2Factory {
    pub address: Address,
    pub fee: usize,
    pub creation_block: u64,
}

impl UniswapV2Factory {
    pub fn new(address: Address, fee: usize, creation_block: u64) -> Self {
        Self {
            address,
            creation_block,
            fee,
        }
    }

    pub async fn get_all_pairs<N, P>(
        factory_address: Address,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<Address>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let factory = IUniswapV2FactoryInstance::new(factory_address, provider.clone());
        let pairs_length = factory
            .allPairsLength()
            .call()
            .block(block_number)
            .await?
            .to::<usize>();

        let step = 200;
        let mut futures_unordered = FuturesUnordered::new();
        let mut i = 0usize;
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(20));

        let mut pairs = Vec::new();
        loop {
            tokio::select! {
                _ = interval.tick(), if i < pairs_length => {
                    let provider = provider.clone();
                    let deployer = IGetUniswapV2PairsBatchRequest::deploy_builder(
                        provider,
                        U256::from(i),
                        U256::from(step),
                        factory_address,
                    );

                    futures_unordered.push(async move {
                        let res = deployer.call_raw().block(block_number).await?;
                        let return_data = <Vec<Address> as SolValue>::abi_decode(&res)?;

                        Ok::<Vec<Address>, AMMError>(return_data)
                    });

                    i = i.saturating_add(step);
                },
                res = futures_unordered.next(), if !futures_unordered.is_empty() => {
                    if let Some(res) = res {
                        let tokens = res?;
                        for token in tokens {
                            if !token.is_zero() {
                                pairs.push(token);
                            }
                        }
                    }
                }
            }

            if i >= pairs_length && futures_unordered.is_empty() {
                break;
            }
        }

        Ok(pairs)
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
            .chunks(step)
            .into_iter()
            .map(|chunk| chunk.map(|amm| amm.address()).collect())
            .collect::<Vec<Vec<Address>>>();

        let mut futures_unordered = FuturesUnordered::new();
        for group in pairs {
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

        let mut amms = amms
            .into_iter()
            .map(|amm| (amm.address(), amm))
            .collect::<HashMap<_, _>>();

        while let Some(res) = futures_unordered.next().await {
            let (group, return_data) = res?;
            for (pool_data, pool_address) in return_data.iter().zip(group.iter()) {
                if pool_data.0.is_zero() {
                    continue;
                }

                let amm = amms.get_mut(pool_address).unwrap();

                let AMM::UniswapV2Pool(pool) = amm else {
                    panic!("Unexpected pool type")
                };

                if pool.token_a.address.is_zero() || pool.token_b.address.is_zero() {
                    let d0 = pool_data.4 as u8;
                    let d1 = pool_data.5 as u8;
                    if d0 == 0 || d1 == 0 {
                        tracing::warn!(
                            ?pool_address,
                            "Skipping pool with 0 decimals (A: {}, B: {})",
                            d0,
                            d1
                        );
                        continue;
                    }
                    pool.token_a = Token::new_with_decimals(pool_data.0, d0);
                    pool.token_b = Token::new_with_decimals(pool_data.1, d1);
                } else {
                    if pool.token_a.decimals == 0 {
                        let d0 = pool_data.4 as u8;
                        if d0 == 0 {
                            tracing::warn!(
                                ?pool_address,
                                "Skipping pool update with 0 decimals for Token A"
                            );
                            continue;
                        }
                        pool.token_a.decimals = d0;
                    }
                    if pool.token_b.decimals == 0 {
                        let d1 = pool_data.5 as u8;
                        if d1 == 0 {
                            tracing::warn!(
                                ?pool_address,
                                "Skipping pool update with 0 decimals for Token B"
                            );
                            continue;
                        }
                        pool.token_b.decimals = d1;
                    }
                }

                pool.reserve_0 = pool_data.2;
                pool.reserve_1 = pool_data.3;

                if pool.reserve_0 > 0 && pool.reserve_1 > 0 {
                    if let Ok(price) =
                        pool.calculate_price(pool.token_a.address, pool.token_b.address)
                    {
                        pool.token_a_price = price;
                        if price != 0.0 {
                            pool.token_b_price = 1.0 / price;
                        } else {
                            pool.token_b_price = 0.0;
                        }
                    }
                }

                if pool.fee == 0 {
                    pool.fee = 300;
                }
            }
        }

        let (valid_amms, invalid_amms): (Vec<_>, Vec<_>) = amms
            .into_iter()
            .partition(|(_, amm)| !amm.tokens().iter().any(|t| t.is_zero()));

        if !invalid_amms.is_empty() {
            for (_, amm) in &invalid_amms {
                info!(
                    target: "amms::uniswap_v2::sync",
                    address = ?amm.address(),
                    tokens = ?amm.tokens(),
                    "Filtering out V2 pool with zero address token"
                );
            }
        }

        let amms: Vec<AMM> = valid_amms.into_iter().map(|(_, amm)| amm).collect();

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
        let total = amms.len();
        let step = 120;
        let pairs = amms
            .iter()
            .chunks(step)
            .into_iter()
            .map(|chunk| chunk.map(|amm| amm.address()).collect())
            .collect::<Vec<Vec<Address>>>();

        let mut futures_unordered = FuturesUnordered::new();
        for group in pairs {
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

        let mut amms = amms
            .into_iter()
            .map(|amm| (amm.address(), amm))
            .collect::<HashMap<_, _>>();

        while let Some(res) = futures_unordered.next().await {
            let (group, return_data) = res?;
            for (pool_data, pool_address) in return_data.iter().zip(group.iter()) {
                // If the pool token A is not zero, signaling that the pool data was polulated

                if pool_data.0.is_zero() {
                    continue;
                }

                let amm = amms.get_mut(pool_address).unwrap();

                let AMM::UniswapV2Pool(pool) = amm else {
                    // TODO:: We should never receive a non UniswapV2Pool AMM here, we can handle this more gracefully in the future
                    panic!("Unexpected pool type")
                };

                let d0 = pool_data.4 as u8;
                let d1 = pool_data.5 as u8;
                if d0 == 0 || d1 == 0 {
                    tracing::warn!(
                        ?pool_address,
                        "Skipping init pool with 0 decimals (A: {}, B: {})",
                        d0,
                        d1
                    );
                    continue;
                }
                pool.token_a = Token::new_with_decimals(pool_data.0, d0);
                pool.token_b = Token::new_with_decimals(pool_data.1, d1);
                pool.reserve_0 = pool_data.2;
                pool.reserve_1 = pool_data.3;

                if pool.fee == 0 {
                    pool.fee = 300;
                }

                if let Ok(p) = pool.calculate_price(pool.token_a.address, pool.token_b.address) {
                    pool.token_a_price = p;
                    if p != 0.0 {
                        pool.token_b_price = 1.0 / p;
                    } else {
                        pool.token_b_price = 0.0;
                    }
                }
            }
        }

        let (valid_amms, invalid_amms): (Vec<_>, Vec<_>) = amms
            .into_iter()
            .partition(|(_, amm)| !amm.tokens().iter().any(|t| t.is_zero()));

        if !invalid_amms.is_empty() {
            for (_, amm) in &invalid_amms {
                info!(
                    target: "amms::uniswap_v2::init_batch",
                    address = ?amm.address(),
                    tokens = ?amm.tokens(),
                    "Filtering out V2 pool with zero address token"
                );
            }
        }

        let amms: Vec<AMM> = valid_amms.into_iter().map(|(_, amm)| amm).collect();

        let valid = amms.len();
        let invalid = invalid_amms.len();
        info!(
            target: "amms::uniswap_v2::init_batch",
            total = total,
            valid = valid,
            invalid = invalid,
            "Batch initialization complete"
        );

        Ok(amms)
    }
}

impl AutomatedMarketMakerFactory for UniswapV2Factory {
    type PoolVariant = UniswapV2Pool;

    fn address(&self) -> Address {
        self.address
    }

    fn pool_creation_event(&self) -> B256 {
        IUniswapV2Factory::PairCreated::SIGNATURE_HASH
    }

    fn create_pool(&self, log: Log) -> Result<AMM, AMMError> {
        let event = IUniswapV2Factory::PairCreated::decode_log(&log.inner)?;
        Ok(AMM::UniswapV2Pool(UniswapV2Pool {
            address: event.pair,
            last_synced_block: 0,
            token_a: event.token0.into(),
            token_b: event.token1.into(),
            reserve_0: 0,
            reserve_1: 0,
            fee: self.fee,
            token_a_price: 0.0,
            token_b_price: 0.0,
        }))
    }

    fn creation_block(&self) -> u64 {
        self.creation_block
    }
}

impl DiscoverySync for UniswapV2Factory {
    fn discover<N, P>(
        &self,
        to_block: BlockId,
        provider: P,
    ) -> impl Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        info!(
            target = "amms::uniswap_v2::discover",
            address = ?self.address,
            "Discovering all pools"
        );

        let provider = provider.clone();
        async move {
            let pairs =
                UniswapV2Factory::get_all_pairs(self.address, to_block, provider.clone()).await?;

            Ok(pairs
                .into_iter()
                .map(|pair| {
                    AMM::UniswapV2Pool(UniswapV2Pool {
                        address: pair,
                        last_synced_block: 0,
                        token_a: Address::default().into(),
                        token_b: Address::default().into(),
                        reserve_0: 0,
                        reserve_1: 0,
                        fee: self.fee,
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
            target: "amms::uniswap_v2::sync",
            address = ?self.address,
            "Syncing all pools"
        );

        UniswapV2Factory::init_batch(amms, to_block, provider)
    }
}

#[cfg(test)]
mod tests {
    use crate::amms::{
        amm::AutomatedMarketMaker, consts::U256_100000, uniswap_v2::UniswapV2Pool, Token,
    };
    use alloy::eips::BlockId;
    use alloy::primitives::{address, Address, U256};
    use alloy::providers::{Provider, ProviderBuilder};
    use alloy::rpc::client::ClientBuilder;
    use alloy::sol;
    use alloy::transports::layers::{RetryBackoffLayer, ThrottleLayer};

    sol! {
        #[sol(rpc)]
        contract IUniswapV2Router02 {
            function getAmountsIn(uint amountOut, address[] calldata path) external view returns (uint[] memory amounts);
        }
    }

    #[tokio::test]
    async fn test_calculate_price_with_init() -> eyre::Result<()> {
        dotenv::dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        // USDC/WETH V2 Pool
        let pool_address = address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc");
        // Using the same block as V3 test for consistency, or we can use latest.
        // Let's use a specific block to be safe, but we don't know the exact price.
        // For now, we'll check sanity.
        let block_number = BlockId::from(22000114);

        let pool = UniswapV2Pool::new(pool_address)
            .init(block_number, provider.clone())
            .await?;

        let float_price_a = pool.calculate_price(pool.token_a.address, Address::default())?;
        let float_price_b = pool.calculate_price(pool.token_b.address, Address::default())?;

        println!("V2 Token A ({}): {}", pool.token_a.symbol, float_price_a);
        println!("V2 Token B ({}): {}", pool.token_b.symbol, float_price_b);

        assert!(float_price_a > 0.0);
        assert!(float_price_b > 0.0);

        // Basic consistency check: price_a should be approx 1/price_b
        // Allow for small floating point error
        let product = float_price_a * float_price_b;
        assert!(
            product > 0.99 && product < 1.01,
            "Prices should be inverse of each other"
        );

        Ok(())
    }

    #[test]
    fn test_get_amount_out() {
        let fees = [125, 150, 300, 1000]; // 0.125%, 0.15%, 0.3%, 1%
        let amount_in = U256::from(10).pow(U256::from(18));
        let reserve_in = U256::from(100).pow(U256::from(18));
        let reserve_out = U256::from(100).pow(U256::from(18));
        let amount_out_no_fee = (reserve_out * amount_in) / (reserve_in + amount_in);
        for fee in fees {
            let pool = UniswapV2Pool {
                fee,
                token_a_price: 0.0,
                token_b_price: 0.0,
                ..Default::default()
            };

            let res = pool.get_amount_out(amount_in, reserve_in, reserve_out);
            assert!(amount_out_no_fee * (U256_100000 - U256::from(fee)) / U256_100000 == res);
        }
    }

    #[test]
    fn test_get_amount_in_exact_out_inverse() {
        let pool = UniswapV2Pool {
            fee: 300,
            token_a_price: 0.0,
            token_b_price: 0.0,
            ..Default::default()
        };

        let reserve_in = U256::from(1_000_000u64);
        let reserve_out = U256::from(2_000_000u64);
        let target_out = U256::from(123_456u64);

        let amount_in = pool
            .get_amount_in(target_out, reserve_in, reserve_out)
            .expect("exact out should be solvable");

        let out_with_in = pool.get_amount_out(amount_in, reserve_in, reserve_out);
        assert!(out_with_in >= target_out);

        if amount_in > U256::ZERO {
            let out_with_less =
                pool.get_amount_out(amount_in - U256::from(1u8), reserve_in, reserve_out);
            assert!(out_with_less < target_out);
        }
    }

    #[test]
    fn test_get_amount_in_insufficient_liquidity() {
        let pool = UniswapV2Pool {
            fee: 300,
            token_a_price: 0.0,
            token_b_price: 0.0,
            ..Default::default()
        };

        let err = pool
            .get_amount_in(U256::from(100u64), U256::from(1000u64), U256::from(100u64))
            .expect_err("must fail when amount_out >= reserve_out");
        assert!(format!("{err}").contains("insufficient liquidity"));
    }

    #[test]
    fn test_calculate_price_edge_case() {
        let token_a = address!("0d500b1d8e8ef31e21c99d1db9a6444d3adf1270");
        let token_b = address!("8f18dc399594b451eda8c5da02d0563c0b2d0f16");
        let pool = UniswapV2Pool {
            address: address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc"),
            last_synced_block: 0,
            token_a: Token::new_with_decimals(
                address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
                6,
            ),
            token_b: Token::new_with_decimals(
                address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
                18,
            ),
            reserve_0: 23595096345912178729927,
            reserve_1: 154664232014390554564,
            fee: 300,
            token_a_price: 1658.3725965327264, // Not used but needed for init
            token_b_price: 0.0006030007985483893,
        };

        assert!(pool.calculate_price(token_a, Address::default()).unwrap() != 0.0);
        assert!(pool.calculate_price(token_b, Address::default()).unwrap() != 0.0);
    }

    #[tokio::test]
    async fn test_calculate_price() {
        let pool = UniswapV2Pool {
            address: address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc"),
            last_synced_block: 0,
            token_a: Token::new_with_decimals(
                address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
                6,
            ),
            token_b: Token::new_with_decimals(
                address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
                18,
            ),
            reserve_0: 47092140895915,
            reserve_1: 28396598565590008529300,
            fee: 300,
            token_a_price: 1658.3725965327264, // Not used but needed for init
            token_b_price: 0.0006030007985483893,
        };

        let price_a_64_x = pool
            .calculate_price(pool.token_a.address, Address::default())
            .unwrap();
        let price_b_64_x = pool
            .calculate_price(pool.token_b.address, Address::default())
            .unwrap();

        // No precision loss: 30591574867092394336528 / 2**64
        assert_eq!(1658.3725965327264, price_b_64_x);
        // Precision loss: 11123401407064628 / 2**64
        assert_eq!(0.0006030007985483893, price_a_64_x);
    }

    #[tokio::test]
    async fn test_calculate_price_64_x_64() {
        let pool = UniswapV2Pool {
            address: address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc"),
            last_synced_block: 0,
            token_a: Token::new_with_decimals(
                address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
                6,
            ),
            token_b: Token::new_with_decimals(
                address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
                18,
            ),
            reserve_0: 47092140895915,
            reserve_1: 28396598565590008529300,
            fee: 300,
            token_a_price: 1658.3725965327264, // Not used but needed for init
            token_b_price: 0.0006030007985483893,
        };

        let price_a_64_x = pool.calculate_price_64_x_64(pool.token_a.address).unwrap();
        let price_b_64_x = pool.calculate_price_64_x_64(pool.token_b.address).unwrap();

        assert_eq!(30591574867092394336528, price_b_64_x);
        assert_eq!(11123401407064628, price_a_64_x);
    }

    #[tokio::test]
    async fn test_simulate_swap_exact_out_matches_router() -> eyre::Result<()> {
        dotenv::dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);
        let provider = ProviderBuilder::new().connect_client(client);

        let router = IUniswapV2Router02::new(
            address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D"),
            provider.clone(),
        );
        let block = BlockId::from(provider.get_block_number().await?);

        // Source: local DB query (2026-03-24)
        // psql -d pools_index
        // SELECT address FROM pools WHERE dex_type='uniswap_v2' AND chain_id=1
        //   AND token0/token1 in {USDC,USDT,WETH,WBTC,DAI}
        let pool_addresses = [
            address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc"), // USDC/WETH
            address!("0d4a11d5EEaaC28EC3F61d100daF4d40471f1852"), // WETH/USDT
            address!("A478c2975Ab1Ea89e8196811F51A7B7Ade33eB11"), // DAI/WETH
            address!("BB2b8038a1640196FbE3e38816F3e67Cba72D940"), // WBTC/WETH
            address!("3041CbD36888bECc7bbCBc0045E3B1f144466f5f"), // USDC/USDT
        ];

        for pool_address in pool_addresses {
            let pool = UniswapV2Pool::new(pool_address)
                .init(block, provider.clone())
                .await?;

            let unit_a = U256::from(10u64).pow(U256::from(pool.token_a.decimals));
            let unit_b = U256::from(10u64).pow(U256::from(pool.token_b.decimals));
            let reserve_a = U256::from(pool.reserve_0);
            let reserve_b = U256::from(pool.reserve_1);

            let amount_out_ab = std::cmp::max(
                U256::from(1u8),
                std::cmp::min(
                    unit_b / U256::from(1_000u64),
                    reserve_b / U256::from(100_000u64),
                ),
            );
            let amount_out_ba = std::cmp::max(
                U256::from(1u8),
                std::cmp::min(
                    unit_a / U256::from(1_000u64),
                    reserve_a / U256::from(100_000u64),
                ),
            );

            if amount_out_ab > U256::ZERO {
                let local_in = pool.simulate_swap_exact_out(
                    pool.token_a.address,
                    pool.token_b.address,
                    amount_out_ab,
                )?;
                let path = vec![pool.token_a.address, pool.token_b.address];
                let chain = router
                    .getAmountsIn(amount_out_ab, path)
                    .block(block)
                    .call()
                    .await?;
                assert_eq!(local_in, chain[0], "pool={} direction=a->b", pool.address);
            }

            if amount_out_ba > U256::ZERO {
                let local_in = pool.simulate_swap_exact_out(
                    pool.token_b.address,
                    pool.token_a.address,
                    amount_out_ba,
                )?;
                let path = vec![pool.token_b.address, pool.token_a.address];
                let chain = router
                    .getAmountsIn(amount_out_ba, path)
                    .block(block)
                    .call()
                    .await?;
                assert_eq!(local_in, chain[0], "pool={} direction=b->a", pool.address);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests_price {
    use super::*;
    use alloy::{
        primitives::address,
        providers::ProviderBuilder,
        rpc::client::ClientBuilder,
        transports::layers::{RetryBackoffLayer, ThrottleLayer},
    };

    #[tokio::test]
    async fn test_calculate_price() -> eyre::Result<()> {
        dotenv::dotenv().ok();
        let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        let block_number = BlockId::from(18000000); // Fixed block for deterministic results
        let mut pool = UniswapV2Pool::new(address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc"));
        pool = pool.init(block_number, provider.clone()).await?;

        let price_a = pool.calculate_price(pool.token_a.address, Address::default())?;
        let price_b = pool.calculate_price(pool.token_b.address, Address::default())?;

        // WETH/USDC prices at block 18000000
        // WETH (token0) decimals 18
        // USDC (token1) decimals 6
        // Reserve0 (WETH): ~...
        // Reserve1 (USDC): ~...
        // Price should be roughly 1600 USDC per ETH

        println!("Token A (WETH) Price: {}", price_a);
        println!("Token B (USDC) Price: {}", price_b);

        assert!(price_a > 1000.0 && price_a < 2000.0); // Rough sanity check for block 18000000
        assert!(price_b > 0.0005 && price_b < 0.001);

        Ok(())
    }
}

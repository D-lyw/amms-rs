use crate::amms::{
    aerodrome_slipstream::pool::AerodromeSlipstreamError, balancer_v3::BalancerV3Error,
    pancake_infinity::PancakeInfinityError, pendle::PendleError, rocketpool::RocketPoolError,
    uniswap_v4::UniswapV4Error,
};

use super::{erc_4626::ERC4626VaultError, uniswap_v2::UniswapV2Error, uniswap_v3::UniswapV3Error};
use alloy::{primitives::FixedBytes, transports::TransportErrorKind};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AMMError {
    #[error(transparent)]
    TransportError(#[from] alloy::transports::RpcError<TransportErrorKind>),
    #[error(transparent)]
    ContractError(#[from] alloy::contract::Error),
    #[error(transparent)]
    ABIError(#[from] alloy::dyn_abi::Error),
    #[error(transparent)]
    SolTypesError(#[from] alloy::sol_types::Error),
    #[error(transparent)]
    UniswapV2Error(#[from] UniswapV2Error),
    #[error(transparent)]
    UniswapV3Error(#[from] UniswapV3Error),
    #[error(transparent)]
    UniswapV4Error(#[from] UniswapV4Error),
    #[error(transparent)]
    PancakeInfinityError(#[from] PancakeInfinityError),
    #[error(transparent)]
    BalancerV2Error(#[from] crate::amms::balancer_v2::BalancerV2Error),
    #[error(transparent)]
    BalancerV3Error(#[from] BalancerV3Error),
    #[error(transparent)]
    ERC4626VaultError(#[from] ERC4626VaultError),
    #[error(transparent)]
    RocketPoolError(#[from] RocketPoolError),
    #[error(transparent)]
    PendleError(#[from] PendleError),
    #[error(transparent)]
    AerodromeSlipstreamError(#[from] AerodromeSlipstreamError),
    #[error(transparent)]
    BatchContractError(#[from] BatchContractError),
    #[error(transparent)]
    ParseFloatError(#[from] rug::float::ParseFloatError),
    #[error("Unrecognized Event Signature {0}")]
    UnrecognizedEventSignature(FixedBytes<32>),
    #[error(transparent)]
    JoinError(#[from] tokio::task::JoinError),
    #[error("Snapshot Error: {0}")]
    SnapshotError(#[from] serde_json::Error),
    #[error("Snapshot Error: {0}")]
    SnapshotIOError(#[from] std::io::Error),
    #[error("Sync Error: {0}")]
    SyncError(alloy::primitives::Address),
    #[error("Incompatible AMM Variant")]
    IncompatibleAMMVariant,
    #[error("Division By Zero")]
    DivisionByZero,
    #[error("Token not found: {0}")]
    TokenNotFound(alloy::primitives::Address),
    #[error("Arithmetic error (overflow/underflow)")]
    ArithmeticError,
    #[error("requested block {requested_block} ahead of storage RPC head {storage_head}")]
    BlockNotAvailable { requested_block: u64, storage_head: u64 },
    #[error("AMM Error: {0}")]
    Msg(String),
    #[error("Token Out Does Not Exist")]
    TokenOutDoesNotExist,
    #[error("SwapExactOut not supported for this AMM")]
    UnsupportedSwapExactOut,
}

#[derive(Error, Debug)]
pub enum BatchContractError {
    #[error(transparent)]
    ContractError(#[from] alloy::contract::Error),
    #[error(transparent)]
    DynABIError(#[from] alloy::dyn_abi::Error),
}

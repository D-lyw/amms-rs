//! Ekubo V2 Type Definitions
//!
//! This module contains type definitions for Ekubo V2 AMM:
//! - PoolConfig: Packed bytes32 configuration
//! - EkuboPoolKey: Pool identifier
//! - EkuboSwapEvent: Log0 swap event data
//! - TickInfo: Tick state for CLMM

use alloy::primitives::{Address, B256, U256};
use serde::{Deserialize, Serialize};

// ========== PoolConfig ==========
// PoolConfig 是一个打包的 bytes32 (32 字节)
//
// V2 结构 (EVM Contracts V2):
//   Bits 256-97 (160 bits): extension address
//   Bits 96-33 (64 bits): fee (uint64)
//   Bits 32-0 (32 bits): tick_spacing (uint32)
//
// V3 结构 (EVM Contracts V3):
//   Bits 256-97 (160 bits): extension address
//   Bits 96-33 (64 bits): fee (uint64)
//   Bits 32-0 (32 bits): pool type config
//     Bit 31: discriminator (1 = concentrated, 0 = stableswap)
//     For concentrated (bit 31 = 1): bits 30-0 are tick spacing
//     For stableswap (bit 31 = 0): bits 30-24 are amplification, bits 23-0 are center tick
//
// 注意: 当前实现使用 V2 格式,因为实际流动性主要在 V2 池子中

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolConfig {
    pub extension: Address,
    pub fee: u64,
    pub tick_spacing: i32,
}

impl PoolConfig {
    /// 创建 V2 格式的 PoolConfig (直接使用 tick_spacing,无 discriminator)
    /// 对应 Solidity V2: toConfig(uint64 fee, uint32 tickSpacing, address extension)
    pub fn create_v2(fee: u64, tick_spacing: i32, extension: Address) -> U256 {
        // 确保 tick_spacing 是正数且在合理范围内
        assert!(tick_spacing > 0, "Invalid tick spacing: must be positive");

        // Pack structure (V2 格式，匹配链上观察到的 Extension 优先布局):
        // | extension (160 bits) | fee (64 bits) | tick_spacing (32 bits) |
        // Bytes: [0..20]         [20..28]        [28..32]
        let mut config_bytes = [0u8; 32];

        // Extension (160 bits = 20 bytes) - 放在最高位 [0-20]
        config_bytes[0..20].copy_from_slice(extension.as_slice());

        // Fee (64 bits = 8 bytes) - 放在 [20-28]
        config_bytes[20..28].copy_from_slice(&fee.to_be_bytes());

        // Tick spacing (32 bits = 4 bytes) - 放在 [28-32],直接使用值,无 discriminator
        config_bytes[28..32].copy_from_slice(&(tick_spacing as u32).to_be_bytes());

        U256::from_be_slice(&config_bytes)
    }

    /// 创建 V3 格式的 PoolConfig (带 discriminator)
    /// 对应 Solidity V3: createConcentratedPoolConfig(uint64 fee, uint32 tickSpacing, address extension)
    pub fn create_v3_concentrated(fee: u64, tick_spacing: i32, extension: Address) -> U256 {
        // 确保 tick_spacing 是正数且在 31 位范围内
        assert!(
            tick_spacing > 0 && tick_spacing < 0x7fffffff,
            "Invalid tick spacing"
        );

        // Pack structure (V3 格式):
        // typeConfig (32 bits): bit 31 = 1 (concentrated), bits 30-0 = tick spacing
        let type_config: u32 = 0x80000000 | (tick_spacing as u32 & 0x7fffffff);

        // Pack into bytes32:
        // | extension (160 bits) | fee (64 bits) | type_config (32 bits) |
        let mut config_bytes = [0u8; 32];

        // Extension (160 bits = 20 bytes) - 放在最高位 [0-20]
        config_bytes[0..20].copy_from_slice(extension.as_slice());

        // Fee (64 bits = 8 bytes) - 放在 [20-28]
        config_bytes[20..28].copy_from_slice(&fee.to_be_bytes());

        // Type config (32 bits = 4 bytes) - 放在 [28-32]
        config_bytes[28..32].copy_from_slice(&type_config.to_be_bytes());

        U256::from_be_slice(&config_bytes)
    }

    /// 从 bytes32 解析 PoolConfig (自动检测 V2/V3 格式)
    pub fn from_bytes32(config: U256) -> Self {
        let config_bytes = config.to_be_bytes::<32>();

        // Extension: bytes 0-20 (160 bits)
        let extension = Address::from_slice(&config_bytes[0..20]);

        // Fee: bytes 20-28 (64 bits)
        let fee = u64::from_be_bytes(config_bytes[20..28].try_into().unwrap());

        // Type config / tick_spacing: bytes 28-32 (32 bits)
        let type_config = u32::from_be_bytes(
            config_bytes[28..32]
                .try_into()
                .expect("slice should be 4 bytes"),
        );

        // 检测是否是 V3 格式 (bit 31 是 discriminator)
        // 注意：这种检测假设 V2 的 tick_spacing 不会设置最高位。
        // 如果 V2 tick_spacing 很大，可能会误判。但在 Concentrated liquidity 中 TS 通常较小。
        let is_v3_format = (type_config & 0x80000000) != 0;

        let tick_spacing = if is_v3_format {
            // V3 格式: 提取 bits 30-0 作为 tick_spacing
            (type_config & 0x7fffffff) as i32
        } else {
            // V2 格式: 直接使用整个值作为 tick_spacing
            type_config as i32
        };

        PoolConfig {
            extension,
            fee,
            tick_spacing,
        }
    }
}

// ========== EkuboPoolKey ==========

/// Ekubo Pool Key
/// 注意: Ekubo V3 使用 PoolConfig (bytes32) 而不是独立的 fee + tickSpacing
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
pub struct EkuboPoolKey {
    pub token0: Address,
    pub token1: Address,
    /// PoolConfig 打包后的 bytes32 (U256)
    /// 使用 PoolConfig::create_concentrated() 创建
    pub config: U256,
}

impl EkuboPoolKey {
    /// 创建 concentrated liquidity pool key (使用 V2 格式)
    /// V2 是实际流动性所在的主要版本
    pub fn new_concentrated(
        token0: Address,
        token1: Address,
        fee: u64,
        tick_spacing: i32,
        extension: Address,
    ) -> Self {
        let config = PoolConfig::create_v2(fee, tick_spacing, extension);
        Self {
            token0,
            token1,
            config,
        }
    }

    /// 创建 V3 格式的 concentrated liquidity pool key
    pub fn new_v3_concentrated(
        token0: Address,
        token1: Address,
        fee: u64,
        tick_spacing: i32,
        extension: Address,
    ) -> Self {
        let config = PoolConfig::create_v3_concentrated(fee, tick_spacing, extension);
        Self {
            token0,
            token1,
            config,
        }
    }

    /// 从原始值创建 pool key
    pub fn from_raw(token0: Address, token1: Address, config: U256) -> Self {
        Self {
            token0,
            token1,
            config,
        }
    }

    /// 计算 pool_id (使用 keccak256 + ABI 编码,与 UniswapV4 相同)
    /// 对应 Solidity: keccak256(abi.encode(PoolKey))
    pub fn pool_id(&self) -> B256 {
        use alloy::sol_types::SolValue;
        alloy::primitives::keccak256((&self.token0, &self.token1, &self.config).abi_encode())
    }

    /// 解析 config 获取 PoolConfig
    pub fn parse_config(&self) -> PoolConfig {
        PoolConfig::from_bytes32(self.config)
    }
}

// ========== EkuboSwapEvent ==========

/// Ekubo V2 Core Swap Event (parsed from Log0)
/// Ekubo V2 使用 Log0 匿名事件 (无 topic signature)
/// SwapEvent: 116 字节打包数据
/// 结构: locker(20) + poolId(32) + delta0(16) + delta1(16) + liquidityAfter(16) + sqrtRatioAfter(12) + tickAfter(4)
#[derive(Debug, Clone)]
pub struct EkuboSwapEvent {
    pub locker: Address,
    pub pool_id: B256,
    pub delta0: i128,
    pub delta1: i128,
    pub liquidity_after: u128,
    pub sqrt_ratio_after: U256, // Converted to fixed point
    pub tick_after: i32,
}

/// 解析 Ekubo V2 Log0 SwapEvent (116 字节)
/// 数据按大端序打包: locker(20) + poolId(32) + delta0(16) + delta1(16) + liquidityAfter(16) + sqrtRatioAfter(12) + tickAfter(4)
pub fn parse_swap_event_log0(data: &[u8]) -> Result<EkuboSwapEvent, &'static str> {
    if data.len() != 116 {
        return Err("Invalid swap event data length: expected 116 bytes");
    }

    // locker: bytes 0-20
    let locker = Address::from_slice(&data[0..20]);

    // poolId: bytes 20-52
    let pool_id = B256::from_slice(&data[20..52]);

    // delta0: bytes 52-68 (int128, big endian)
    let delta0_bytes: [u8; 16] = data[52..68].try_into().unwrap();
    let delta0 = i128::from_be_bytes(delta0_bytes);

    // delta1: bytes 68-84 (int128, big endian)
    let delta1_bytes: [u8; 16] = data[68..84].try_into().unwrap();
    let delta1 = i128::from_be_bytes(delta1_bytes);

    // liquidityAfter: bytes 84-100 (uint128, big endian)
    let liquidity_bytes: [u8; 16] = data[84..100].try_into().unwrap();
    let liquidity_after = u128::from_be_bytes(liquidity_bytes);

    // sqrtRatioAfter: bytes 100-112 (uint96 float, big endian)
    // 注意: 这个值是 Ekubo 内部的压缩浮点格式，解析极其复杂且容易出错。
    // 我们选择忽略这个值，而是使用 tick_after 来计算标准的 Q64.128 sqrt_price。
    // 这样可以保证与 poolPrice (Q64.128) 的一致性，并避免解析错误导致的虚假套利。
    let sqrt_ratio_after = U256::ZERO; // Placeholder

    // tickAfter: bytes 112-116 (int32, big endian)
    let tick_bytes: [u8; 4] = data[112..116].try_into().unwrap();
    let tick_after = i32::from_be_bytes(tick_bytes);

    Ok(EkuboSwapEvent {
        locker,
        pool_id,
        delta0,
        delta1,
        liquidity_after,
        sqrt_ratio_after,
        tick_after,
    })
}

// ========== TickInfo ==========

/// Tick Info for CLMM pools
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TickInfo {
    pub liquidity_gross: u128,
    pub liquidity_net: i128,
    pub initialized: bool,
}

// ========== EkuboPositionUpdatedEvent ==========

/// Ekubo V2 PositionUpdated Event (parsed from Log0)
/// Event: PositionUpdated(address locker, bytes32 poolId, UpdatePositionParameters params, int128 delta0, int128 delta1)
///
/// UpdatePositionParameters struct:
///   - salt: bytes32 (32 bytes)
///   - bounds.lower: int32 (4 bytes)  
///   - bounds.upper: int32 (4 bytes)
///   - liquidityDelta: int128 (16 bytes)
///
/// Total: 140 bytes
/// Layout: locker(20) + poolId(32) + salt(32) + lower(4) + upper(4) + liquidityDelta(16) + delta0(16) + delta1(16)
#[derive(Debug, Clone)]
pub struct EkuboPositionUpdatedEvent {
    pub locker: Address,
    pub pool_id: B256,
    pub salt: B256,
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub liquidity_delta: i128,
    pub delta0: i128,
    pub delta1: i128,
}

/// 解析 Ekubo V2 Log0 PositionUpdated (140 字节)
/// 数据按大端序打包
pub fn parse_position_updated_log0(data: &[u8]) -> Result<EkuboPositionUpdatedEvent, &'static str> {
    if data.len() != 140 {
        return Err("Invalid PositionUpdated event data length: expected 140 bytes");
    }

    // locker: bytes 0-20
    let locker = Address::from_slice(&data[0..20]);

    // poolId: bytes 20-52
    let pool_id = B256::from_slice(&data[20..52]);

    // salt: bytes 52-84
    let salt = B256::from_slice(&data[52..84]);

    // bounds.lower: bytes 84-88 (int32, big endian)
    let lower_bytes: [u8; 4] = data[84..88].try_into().unwrap();
    let tick_lower = i32::from_be_bytes(lower_bytes);

    // bounds.upper: bytes 88-92 (int32, big endian)
    let upper_bytes: [u8; 4] = data[88..92].try_into().unwrap();
    let tick_upper = i32::from_be_bytes(upper_bytes);

    // liquidityDelta: bytes 92-108 (int128, big endian)
    let liquidity_delta_bytes: [u8; 16] = data[92..108].try_into().unwrap();
    let liquidity_delta = i128::from_be_bytes(liquidity_delta_bytes);

    // delta0: bytes 108-124 (int128, big endian)
    let delta0_bytes: [u8; 16] = data[108..124].try_into().unwrap();
    let delta0 = i128::from_be_bytes(delta0_bytes);

    // delta1: bytes 124-140 (int128, big endian)
    let delta1_bytes: [u8; 16] = data[124..140].try_into().unwrap();
    let delta1 = i128::from_be_bytes(delta1_bytes);

    Ok(EkuboPositionUpdatedEvent {
        locker,
        pool_id,
        salt,
        tick_lower,
        tick_upper,
        liquidity_delta,
        delta0,
        delta1,
    })
}

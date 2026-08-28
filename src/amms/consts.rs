use alloy::primitives::U256;

// commonly used U256s
pub const U256_10E_10: U256 = U256::from_limbs([10000000000, 0, 0, 0]);
pub const U256_0X100000000: U256 = U256::from_limbs([4294967296, 0, 0, 0]);
pub const U256_0X10000: U256 = U256::from_limbs([65536, 0, 0, 0]);
pub const U256_0X100: U256 = U256::from_limbs([256, 0, 0, 0]);
pub const U256_1000: U256 = U256::from_limbs([1000, 0, 0, 0]);
pub const U256_10000: U256 = U256::from_limbs([10000, 0, 0, 0]);
pub const U256_100000: U256 = U256::from_limbs([100000, 0, 0, 0]);
pub const U256_255: U256 = U256::from_limbs([255, 0, 0, 0]);
pub const U256_192: U256 = U256::from_limbs([192, 0, 0, 0]);
pub const U256_191: U256 = U256::from_limbs([191, 0, 0, 0]);
pub const U256_128: U256 = U256::from_limbs([128, 0, 0, 0]);
pub const U256_64: U256 = U256::from_limbs([64, 0, 0, 0]);
pub const U256_32: U256 = U256::from_limbs([32, 0, 0, 0]);
pub const U256_16: U256 = U256::from_limbs([16, 0, 0, 0]);
pub const U256_10: U256 = U256::from_limbs([10, 0, 0, 0]);
pub const U256_8: U256 = U256::from_limbs([8, 0, 0, 0]);
pub const U256_4: U256 = U256::from_limbs([4, 0, 0, 0]);
pub const U256_2: U256 = U256::from_limbs([2, 0, 0, 0]);
pub const U256_1: U256 = U256::from_limbs([1, 0, 0, 0]);

// Uniswap V3 specific
pub const POPULATE_TICK_DATA_STEP: u64 = 100000;
pub const Q128: U256 = U256::from_limbs([0, 0, 1, 0]);
pub const Q224: U256 = U256::from_limbs([0, 0, 0, 4294967296]);

// Balancer V2 specific
pub const BONE: U256 = U256::from_limbs([0xDE0B6B3A7640000, 0, 0, 0]);

// Others
pub const U128_0X10000000000000000: u128 = 18446744073709551616;
pub const U256_0XFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF: U256 = U256::from_limbs([
    18446744073709551615,
    18446744073709551615,
    18446744073709551615,
    0,
]);
pub const U256_0XFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF: U256 =
    U256::from_limbs([18446744073709551615, 18446744073709551615, 0, 0]);

pub const DECIMAL_RADIX: i32 = 10;
pub const MPFR_T_PRECISION: u32 = 70;

// Minimum liquidity thresholds to prevent dust pool arbitrage
pub const MIN_POOL_RESERVE: u128 = 100_000;
pub const MIN_V3_LIQUIDITY: u128 = 100_000;

// V3/V4 空洞防御：当前 tick 与可接受 liquidity tick 之间的最大 bitmap word 距离。
// 只有大额流动性却远在多个空 word 之外的池子是"被抽干/迁移"的死池
// （BSC 实锤：0xfda09351 活跃流动性 84 wei，真实流动性 ~19 个空 word 之外），
// 模拟跨空洞会以近零成本移动价格、产出天文数字的虚假利润。
pub const MAX_LIQUIDITY_DISTANCE_WORDS: i32 = 8;

// 模拟步进时允许连续跨越的空 word 数（>此值且跨空洞流动性过低 → Err）。
// 当前 word 空没关系，周围 word 有流动性即可；只有"低流动性长空洞"才判死。
pub const MAX_SIM_EMPTY_WORDS: i32 = 2;

use alloy::{
    primitives::{keccak256, B256, I256, U256},
    sol_types::SolValue,
};

const POOLS_SLOT: U256 = U256::from_limbs([6, 0, 0, 0]);
const TICKS_OFFSET: U256 = U256::from_limbs([4, 0, 0, 0]);
const TICK_BITMAP_OFFSET: U256 = U256::from_limbs([5, 0, 0, 0]);
const POSITIONS_OFFSET: U256 = U256::from_limbs([7, 0, 0, 0]);
const LIQUIDITY_OFFSET: U256 = U256::from_limbs([3, 0, 0, 0]);

pub fn get_pool_state_slot(pool_id: B256) -> U256 {
    U256::from_be_bytes(keccak256((pool_id, POOLS_SLOT).abi_encode()).0)
}

pub fn get_tick_bitmap_slot(pool_id: B256, tick: i16) -> U256 {
    let state_slot = get_pool_state_slot(pool_id);
    let tick_bitmap_mapping = state_slot + TICK_BITMAP_OFFSET;

    U256::from_be_bytes(
        keccak256((I256::try_from(tick).unwrap(), tick_bitmap_mapping).abi_encode()).0,
    )
}

pub fn get_tick_info_slot(pool_id: B256, tick: i32) -> U256 {
    let state_slot = get_pool_state_slot(pool_id);
    let ticks_mapping_slot = state_slot + TICKS_OFFSET;
    U256::from_be_bytes(
        keccak256((I256::try_from(tick).unwrap(), ticks_mapping_slot).abi_encode()).0,
    )
}

pub fn get_liquidity_slot(pool_id: B256) -> U256 {
    let state_slot = get_pool_state_slot(pool_id);
    state_slot + LIQUIDITY_OFFSET
}

pub fn get_position_info_slot(pool_id: B256, position_key: B256) -> U256 {
    let state_slot = get_pool_state_slot(pool_id);
    let position_mapping_slot = state_slot + POSITIONS_OFFSET;
    U256::from_be_bytes(keccak256((position_key, position_mapping_slot).abi_encode()).0)
}

pub const fn decode_liquidity_gross_and_net(word: B256) -> (u128, i128) {
    // In Solidity:
    // liquidityNet := sar(128, value)
    // liquidityGross := and(value, 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF)
    let liquidity_gross = decode_liquidity(word);
    let liquidity_net = unsafe {
        // Create a pointer to the start of the first half of the array
        let net_ptr = word.0.as_ptr() as *const i128;
        // Read the value in big-endian format
        i128::from_be(net_ptr.read_unaligned())
    };
    (liquidity_gross, liquidity_net)
}

pub const fn decode_liquidity(word: B256) -> u128 {
    unsafe {
        // Create a pointer to the start of the second half of the array
        let ptr = word.0.as_ptr().add(16) as *const u128;
        // Read the value in big-endian format
        u128::from_be(ptr.read_unaligned())
    }
}

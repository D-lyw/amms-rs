//SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *       deployment bytecode as payload.
 * @notice This is specifically designed for Aerodrome Slipstream pools which have
 *         a different slot0 return format than Uniswap V3 (no feeProtocol field).
 */
contract GetAerodromeSlipstreamSlot0BatchRequest {
    struct Slot0Data {
        int24 tick;
        uint128 liquidity;
        uint256 sqrtPrice;
    }

    constructor(address[] memory pools) {
        Slot0Data[] memory allSlot0Data = new Slot0Data[](pools.length);

        for (uint256 i = 0; i < pools.length; ++i) {
            Slot0Data memory slot0Data = allSlot0Data[i];
            address poolAddress = pools[i];

            ICLPoolState pool = ICLPoolState(poolAddress);
            slot0Data.liquidity = pool.liquidity();

            // Slipstream slot0 returns 6 values (no feeProtocol)
            (slot0Data.sqrtPrice, slot0Data.tick, , , , ) = pool.slot0();

            allSlot0Data[i] = slot0Data;
        }

        // ensure abi encoding, not needed here but increase reusability for different return types
        // note: abi.encode add a first 32 bytes word with the address of the original data
        bytes memory abiEncodedData = abi.encode(allSlot0Data);

        assembly {
            // Return from the start of the data (discarding the original data address)
            // up to the end of the memory used
            let dataStart := add(abiEncodedData, 0x20)
            return(dataStart, sub(msize(), dataStart))
        }
    }
}

/// @title Aerodrome Slipstream Pool state that can change
/// @notice These methods compose the pool's state, and can change with any frequency
interface ICLPoolState {
    /// @notice The 0th storage slot in the pool stores many values, and is exposed as a single method to save gas
    /// @return sqrtPriceX96 The current price of the pool as a sqrt(token1/token0) Q64.96 value
    /// @return tick The current tick of the pool
    /// @return observationIndex The index of the last oracle observation
    /// @return observationCardinality The current maximum number of observations
    /// @return observationCardinalityNext The next maximum number of observations
    /// @return unlocked Whether the pool is currently unlocked
    function slot0()
        external
        view
        returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            bool unlocked
        );

    /// @notice The currently in range liquidity available to the pool
    function liquidity() external view returns (uint128);
}

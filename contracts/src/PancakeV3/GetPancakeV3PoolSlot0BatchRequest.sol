//SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *       deployment bytecode as payload.
 */
contract GetPancakeV3PoolSlot0BatchRequest {
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

            IPancakeV3PoolState pool = IPancakeV3PoolState(poolAddress);
            slot0Data.liquidity = pool.liquidity();

            (slot0Data.sqrtPrice, slot0Data.tick, , , , , ) = pool.slot0();

            allSlot0Data[i] = slot0Data;
        }

        bytes memory abiEncodedData = abi.encode(allSlot0Data);

        assembly {
            let dataStart := add(abiEncodedData, 0x20)
            return(dataStart, sub(msize(), dataStart))
        }
    }
}

/// @title PancakeV3 Pool state that can change
/// @notice These methods compose the pool's state, and can change with any frequency including multiple times
/// per transaction
/// @dev Key difference from UniswapV3: feeProtocol is uint32 instead of uint8
interface IPancakeV3PoolState {
    struct TickInfo {
        uint128 liquidityGross;
        int128 liquidityNet;
        uint256 feeGrowthOutside0X128;
        uint256 feeGrowthOutside1X128;
        int56 tickCumulativeOutside;
        uint160 secondsPerLiquidityOutsideX128;
        uint32 secondsOutside;
        bool initialized;
    }

    function ticks(int24 tick) external view returns (TickInfo memory);

    function tickBitmap(int16 wordPosition) external view returns (uint256);

    function slot0()
        external
        view
        returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint32 feeProtocol,
            bool unlocked
        );

    function liquidity() external view returns (uint128);
}

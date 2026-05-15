//SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *       deployment bytecode as payload.
 */
contract GetAlgebraPoolTickDataBatchRequest {
    struct TickDataInfo {
        address pool;
        int24[] ticks;
    }

    struct TickInfo {
        bool initialized;
        uint128 liquidityGross;
        int128 liquidityNet;
    }

    constructor(TickDataInfo[] memory allPoolInfo) {
        TickInfo[][] memory tickInfoReturn = new TickInfo[][](allPoolInfo.length);

        for (uint256 i = 0; i < allPoolInfo.length; ++i) {
            TickInfo[] memory tickInfo = new TickInfo[](allPoolInfo[i].ticks.length);
            IAlgebraPoolTicks pool = IAlgebraPoolTicks(allPoolInfo[i].pool);

            for (uint256 j = 0; j < allPoolInfo[i].ticks.length; ++j) {
                (
                    uint256 liquidityTotal,
                    int128 liquidityDelta,
                    int24 prevTick,
                    int24 nextTick,
                    uint256 outerFeeGrowth0Token,
                    uint256 outerFeeGrowth1Token
                ) = pool.ticks(allPoolInfo[i].ticks[j]);
                prevTick;
                nextTick;
                outerFeeGrowth0Token;
                outerFeeGrowth1Token;

                uint128 liquidityGross = liquidityTotal > type(uint128).max
                    ? type(uint128).max
                    : uint128(liquidityTotal);

                tickInfo[j] = TickInfo({
                    initialized: liquidityGross > 0,
                    liquidityGross: liquidityGross,
                    liquidityNet: liquidityDelta
                });
            }

            tickInfoReturn[i] = tickInfo;
        }

        bytes memory abiEncodedData = abi.encode(tickInfoReturn);

        assembly {
            let dataStart := add(abiEncodedData, 0x20)
            return(dataStart, sub(msize(), dataStart))
        }
    }
}

interface IAlgebraPoolTicks {
    function ticks(int24 tick)
        external
        view
        returns (
            uint256 liquidityTotal,
            int128 liquidityDelta,
            int24 prevTick,
            int24 nextTick,
            uint256 outerFeeGrowth0Token,
            uint256 outerFeeGrowth1Token
        );
}

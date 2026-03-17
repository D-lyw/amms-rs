//SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *       deployment bytecode as payload.
 * @notice Aerodrome Slipstream ticks() returns only (liquidityGross, liquidityNet),
 *         which is different from UniswapV3's 8-field return.
 */

contract GetAerodromeSlipstreamPoolTickDataBatchRequest {
    struct TickDataInfo {
        address pool;
        int24[] ticks;
    }

    struct TickInfo {
        uint128 liquidityGross;
        int128 liquidityNet;
    }

    constructor(TickDataInfo[] memory allPoolInfo) {
        TickInfo[][] memory tickInfoReturn = new TickInfo[][](allPoolInfo.length);

        for (uint256 i = 0; i < allPoolInfo.length; ++i) {
            TickInfo[] memory tickInfo = new TickInfo[](allPoolInfo[i].ticks.length);
            for (uint256 j = 0; j < allPoolInfo[i].ticks.length; ++j) {
                (
                    uint128 liquidityGross,
                    int128 liquidityNet
                ) = ICLPoolTick(allPoolInfo[i].pool).ticks(allPoolInfo[i].ticks[j]);

                tickInfo[j] = TickInfo({
                    liquidityGross: liquidityGross,
                    liquidityNet: liquidityNet
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

interface ICLPoolTick {
    function ticks(int24 tick) external view returns (
        uint128 liquidityGross,
        int128 liquidityNet
    );
}

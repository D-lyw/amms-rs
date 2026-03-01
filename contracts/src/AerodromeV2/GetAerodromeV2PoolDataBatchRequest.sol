//SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

interface IAerodromeV2Pool {
    function token0() external view returns (address);
    function token1() external view returns (address);
    function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    function metadata() external view returns (uint256 dec0, uint256 dec1, uint256 r0, uint256 r1, bool st, address t0, address t1);
}

interface IERC20 {
    function decimals() external view returns (uint8);
}

/**
 * @dev Batch contract to fetch Aerodrome V2 pool data efficiently.
 *      This contract is not meant to be deployed. Instead, use a static call with the
 *      deployment bytecode as payload.
 *
 *      Returns: (address tokenA, address tokenB, uint112 reserve0, uint112 reserve1,
 *                uint8 decimals0, uint8 decimals1, bool stable)[]
 */
contract GetAerodromeV2PoolDataBatchRequest {
    struct PoolData {
        address tokenA;
        address tokenB;
        uint112 reserve0;
        uint112 reserve1;
        uint8 tokenADecimals;
        uint8 tokenBDecimals;
        bool stable;
    }

    constructor(address[] memory pools) {
        PoolData[] memory allPoolData = new PoolData[](pools.length);

        for (uint256 i = 0; i < pools.length; ++i) {
            address poolAddress = pools[i];

            if (codeSizeIsZero(poolAddress)) continue;

            PoolData memory poolData;

            // Get metadata which includes all pool data in one call
            (
                uint256 dec0,
                uint256 dec1,
                uint256 r0,
                uint256 r1,
                bool st,
                address t0,
                address t1
            ) = IAerodromeV2Pool(poolAddress).metadata();

            // Check that tokens exist
            if (codeSizeIsZero(t0) || codeSizeIsZero(t1)) {
                continue;
            }

            // Validate decimals
            if (dec0 == 0 || dec0 > 255 || dec1 == 0 || dec1 > 255) {
                continue;
            }

            poolData.tokenA = t0;
            poolData.tokenB = t1;
            poolData.reserve0 = uint112(r0);
            poolData.reserve1 = uint112(r1);
            poolData.tokenADecimals = uint8(dec0);
            poolData.tokenBDecimals = uint8(dec1);
            poolData.stable = st;

            allPoolData[i] = poolData;
        }

        // ensure abi encoding
        bytes memory _abiEncodedData = abi.encode(allPoolData);

        assembly {
            let dataStart := add(_abiEncodedData, 0x20)
            return(dataStart, sub(msize(), dataStart))
        }
    }

    function codeSizeIsZero(address target) internal view returns (bool) {
        if (target.code.length == 0) {
            return true;
        } else {
            return false;
        }
    }
}

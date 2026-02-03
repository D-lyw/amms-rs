//SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *       deployment bytecode as payload.
 */
contract GetUniswapV3PoolStaticMetaBatchRequest {
    struct Meta {
        address token0;
        address token1;
        int24 tickSpacing;
        uint24 fee;
    }

    constructor(address[] memory pools) {
        Meta[] memory allMeta = new Meta[](pools.length);

        for (uint256 i = 0; i < pools.length; ++i) {
            IUniswapV3PoolImmutables pool = IUniswapV3PoolImmutables(pools[i]);

            address token0 = pool.token0();
            address token1 = pool.token1();
            int24 tickSpacing = pool.tickSpacing();
            uint24 fee = pool.fee();

            allMeta[i] = Meta({
                token0: token0,
                token1: token1,
                tickSpacing: tickSpacing,
                fee: fee
            });
        }

        // ensure abi encoding, not needed here but increase reusability for different return types
        // note: abi.encode add a first 32 bytes word with the address of the original data
        bytes memory abiEncodedData = abi.encode(allMeta);

        assembly {
            // Return from the start of the data (discarding the original data address)
            // up to the end of the memory used
            let dataStart := add(abiEncodedData, 0x20)
            return(dataStart, sub(msize(), dataStart))
        }
    }
}

interface IUniswapV3PoolImmutables {
    function token0() external view returns (address);
    function token1() external view returns (address);
    function tickSpacing() external view returns (int24);
    function fee() external view returns (uint24);
}


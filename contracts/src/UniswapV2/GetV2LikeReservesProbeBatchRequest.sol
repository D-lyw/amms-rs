//SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *      deployment bytecode as payload.
 * @notice Batch probe for V2-style pools that only returns reserves.
 */
contract GetV2LikeReservesProbeBatchRequest {
    struct ReserveData {
        bool ok;
        uint112 reserve0;
        uint112 reserve1;
    }

    constructor(address[] memory pools) {
        ReserveData[] memory allData = new ReserveData[](pools.length);

        for (uint256 i = 0; i < pools.length; ++i) {
            IV2LikePairProbe pool = IV2LikePairProbe(pools[i]);

            try pool.getReserves() returns (
                uint112 reserve0,
                uint112 reserve1,
                uint32
            ) {
                allData[i] = ReserveData({
                    ok: true,
                    reserve0: reserve0,
                    reserve1: reserve1
                });
            } catch {
                continue;
            }
        }

        bytes memory abiEncodedData = abi.encode(allData);
        assembly {
            let dataStart := add(abiEncodedData, 0x20)
            return(dataStart, sub(msize(), dataStart))
        }
    }
}

interface IV2LikePairProbe {
    function getReserves()
        external
        view
        returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
}

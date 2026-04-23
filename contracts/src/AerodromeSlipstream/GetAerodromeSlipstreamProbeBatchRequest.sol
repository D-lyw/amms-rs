//SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *      deployment bytecode as payload.
 * @notice Batch probe for Aerodrome Slipstream pools:
 *         returns slot0 + liquidity + dynamic fee in one request.
 */
contract GetAerodromeSlipstreamProbeBatchRequest {
    struct ProbeData {
        bool ok;
        int24 tick;
        uint128 liquidity;
        uint256 sqrtPrice;
        uint24 fee;
    }

    constructor(address[] memory pools) {
        ProbeData[] memory allData = new ProbeData[](pools.length);

        for (uint256 i = 0; i < pools.length; ++i) {
            ICLPoolProbe pool = ICLPoolProbe(pools[i]);

            try pool.liquidity() returns (uint128 liquidity) {
                try pool.slot0() returns (
                    uint160 sqrtPriceX96,
                    int24 tick,
                    uint16,
                    uint16,
                    uint16,
                    bool
                ) {
                    try pool.fee() returns (uint24 fee) {
                        allData[i] = ProbeData({
                            ok: true,
                            tick: tick,
                            liquidity: liquidity,
                            sqrtPrice: uint256(sqrtPriceX96),
                            fee: fee
                        });
                    } catch {
                        continue;
                    }
                } catch {
                    continue;
                }
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

interface ICLPoolProbe {
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

    function liquidity() external view returns (uint128);
    function fee() external view returns (uint24);
}

//SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *       deployment bytecode as payload.
 */
contract GetAlgebraPoolStaticMetaBatchRequest {
    struct Meta {
        bool ok;
        address token0;
        address token1;
        int24 tickSpacing;
        uint16 fee;
    }

    constructor(address[] memory pools) {
        Meta[] memory allMeta = new Meta[](pools.length);

        for (uint256 i = 0; i < pools.length; ++i) {
            IAlgebraPoolImmutables pool = IAlgebraPoolImmutables(pools[i]);

            try pool.token0() returns (address token0) {
                try pool.token1() returns (address token1) {
                    try pool.tickSpacing() returns (int24 tickSpacing) {
                        try pool.fee() returns (uint16 fee) {
                            allMeta[i] = Meta({
                                ok: true,
                                token0: token0,
                                token1: token1,
                                tickSpacing: tickSpacing,
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
            } catch {
                continue;
            }
        }

        bytes memory abiEncodedData = abi.encode(allMeta);

        assembly {
            let dataStart := add(abiEncodedData, 0x20)
            return(dataStart, sub(msize(), dataStart))
        }
    }
}

interface IAlgebraPoolImmutables {
    function token0() external view returns (address);
    function token1() external view returns (address);
    function tickSpacing() external view returns (int24);
    function fee() external view returns (uint16);
}

//SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *      deployment bytecode as payload.
 * @notice Batch probe for V4-style pools backed by a PoolManager.
 */
contract GetV4LitePoolStateBatchRequest {
    uint256 internal constant POOLS_SLOT = 6;
    uint256 internal constant LIQUIDITY_OFFSET = 3;

    struct PoolProbe {
        address manager;
        bytes32 poolId;
    }

    struct ProbeData {
        bool ok;
        int24 tick;
        uint128 liquidity;
        uint256 sqrtPrice;
    }

    constructor(PoolProbe[] memory pools) {
        ProbeData[] memory allData = new ProbeData[](pools.length);

        for (uint256 i = 0; i < pools.length; ++i) {
            PoolProbe memory pool = pools[i];
            bytes32[] memory slots = new bytes32[](2);
            slots[0] = bytes32(getPoolStateSlot(pool.poolId));
            slots[1] = bytes32(getLiquiditySlot(pool.poolId));

            try IExtsloadManager(pool.manager).extsload(slots) returns (
                bytes32[] memory values
            ) {
                if (values.length != 2 || values[0] == bytes32(0)) {
                    continue;
                }

                allData[i] = ProbeData({
                    ok: true,
                    tick: decodeTick(values[0]),
                    liquidity: uint128(uint256(values[1])),
                    sqrtPrice: uint256(uint160(uint256(values[0])))
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

    function getPoolStateSlot(bytes32 poolId) internal pure returns (uint256) {
        return uint256(keccak256(abi.encode(poolId, POOLS_SLOT)));
    }

    function getLiquiditySlot(bytes32 poolId) internal pure returns (uint256) {
        return getPoolStateSlot(poolId) + LIQUIDITY_OFFSET;
    }

    function decodeTick(bytes32 slot0Word) internal pure returns (int24 tick) {
        assembly {
            tick := signextend(2, shr(160, slot0Word))
        }
    }
}

interface IExtsloadManager {
    function extsload(bytes32[] calldata slots)
        external
        view
        returns (bytes32[] memory values);
}

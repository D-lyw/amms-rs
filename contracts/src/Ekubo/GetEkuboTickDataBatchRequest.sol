//SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @title GetEkuboTickDataBatchRequest
 * @notice Batch request to get tick liquidity data from Ekubo Core V2
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *       deployment bytecode as payload.
 *
 * Ekubo V2 uses STANDARD Solidity mapping storage layout!
 * See: https://github.com/EkuboProtocol/evm-contracts/blob/v2.0.0/src/Core.sol
 *
 * Storage layout (slot numbers from variable declaration order):
 *   slot 0: isExtensionRegistered
 *   slot 1: protocolFeesCollected
 *   slot 2: poolState
 *   slot 3: poolFeesPerLiquidity
 *   slot 4: poolPositions
 *   slot 5: poolTicks  <-- TICK DATA
 *   slot 6: poolTickFeesPerLiquidityOutside
 *   slot 7: poolInitializedTickBitmaps
 *
 * For mapping(bytes32 poolId => mapping(int32 tick => TickInfo)):
 *   innerSlot = keccak256(poolId, 5)
 *   finalSlot = keccak256(tick, innerSlot)
 *
 * TickInfo struct (32 bytes total, packed):
 *   int128 liquidityDelta;   // liquidity_net (lower 128 bits)
 *   uint128 liquidityNet;    // liquidity_gross (upper 128 bits)
 */
contract GetEkuboTickDataBatchRequest {
    struct TickDataInfo {
        bytes32 poolId;
        int32[] ticks;
    }

    struct Info {
        bool initialized;
        uint128 liquidityGross;
        int128 liquidityNet;
    }

    // Ekubo V2 Core singleton address (Ethereum Mainnet)
    address constant EKUBO_CORE = 0xe0e0e08A6A4b9Dc7bD67BCB7aadE5cF48157d444;

    // Storage slot for poolTicks mapping in V2 Core.sol
    uint256 constant TICKS_SLOT = 5;

    constructor(TickDataInfo[] memory allPoolInfo) {
        Info[][] memory allTickInfo = new Info[][](allPoolInfo.length);

        for (uint256 i = 0; i < allPoolInfo.length; ++i) {
            bytes32 poolId = allPoolInfo[i].poolId;
            int32[] memory ticks = allPoolInfo[i].ticks;
            
            Info[] memory tickInfo = new Info[](ticks.length);
            
            // Calculate inner slot: keccak256(poolId, TICKS_SLOT)
            // This is the base slot for this pool's ticks mapping
            bytes32 innerSlot = keccak256(abi.encode(poolId, TICKS_SLOT));
            
            // Build storage slots array for batch sload
            bytes32[] memory slots = new bytes32[](ticks.length);
            for (uint256 j = 0; j < ticks.length; ++j) {
                // Final slot: keccak256(tick, innerSlot)
                // tick is int32, need to sign-extend to int256 for abi.encode
                slots[j] = keccak256(abi.encode(int256(ticks[j]), innerSlot));
            }
            
            // Call sload with all slots
            // sload() selector = 0x380eb4e0, expects raw slot bytes after selector
            bytes memory callData = abi.encodePacked(bytes4(0x380eb4e0));
            for (uint256 j = 0; j < slots.length; ++j) {
                callData = abi.encodePacked(callData, slots[j]);
            }
            
            (bool success, bytes memory result) = EKUBO_CORE.staticcall(callData);
            
            if (success && result.length >= ticks.length * 32) {
                for (uint256 j = 0; j < ticks.length; ++j) {
                    uint256 packed;
                    assembly {
                        packed := mload(add(result, add(32, mul(j, 32))))
                    }
                    
                    // Ekubo V2 TickInfo layout matches standard Solidity packing:
                    // uint128 liquidityGross (first member) -> lower 128 bits
                    // int128 liquidityNet  (second member) -> upper 128 bits
                    uint128 liquidityGross = uint128(packed);
                    int128 liquidityNet = int128(int256(packed >> 128));
                    
                    tickInfo[j] = Info({
                        initialized: liquidityGross > 0,
                        liquidityGross: liquidityGross,
                        liquidityNet: liquidityNet
                    });
                }
            }
            
            allTickInfo[i] = tickInfo;
        }

        bytes memory abiEncodedData = abi.encode(allTickInfo);

        assembly {
            let dataStart := add(abiEncodedData, 0x20)
            return(dataStart, sub(msize(), dataStart))
        }
    }
}

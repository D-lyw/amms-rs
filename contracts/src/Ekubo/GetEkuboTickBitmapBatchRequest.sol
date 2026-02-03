//SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @title GetEkuboTickBitmapBatchRequest
 * @notice Batch request to get tick bitmap words from Ekubo Core V2
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *       deployment bytecode as payload.
 *
 * Ekubo V2 uses STANDARD Solidity mapping storage layout!
 * See: https://github.com/EkuboProtocol/evm-contracts/blob/v2.0.0/src/Core.sol
 *
 * IMPORTANT: V2 uses an OFFSET of 89421695 in tick bitmap indexing!
 * See: https://github.com/EkuboProtocol/evm-contracts/blob/v2.0.0/src/math/tickBitmap.sol
 *   rawIndex = (tick / tickSpacing) + 89421695
 *   word = rawIndex / 256
 *
 * Storage layout:
 *   slot 7: poolInitializedTickBitmaps
 *   mapping(bytes32 poolId => mapping(uint256 word => Bitmap))
 */
contract GetEkuboTickBitmapBatchRequest {
    struct TickBitmapInfo {
        bytes32 poolId;
        uint32 tickSpacing;
        int32 minTick;  // Starting tick to fetch bitmaps for
        int32 maxTick;  // Ending tick to fetch bitmaps for
    }

    // Ekubo V2 Core singleton address (Ethereum Mainnet)
    address constant EKUBO_CORE = 0xe0e0e08A6A4b9Dc7bD67BCB7aadE5cF48157d444;

    // Storage slot for poolInitializedTickBitmaps mapping in V2 Core.sol
    uint256 constant TICK_BITMAP_SLOT = 7;

    // Offset used in V2 to convert signed tick indices to unsigned
    uint256 constant BITMAP_OFFSET = 89421695;

    // Convert tick to word position using V2's algorithm
    function tickToWord(int32 tick, uint32 tickSpacing) internal pure returns (uint256) {
        // V2 algorithm: rawIndex = compressed + 89421695
        // where compressed = tick / tickSpacing (rounded towards negative infinity)
        int256 compressed = int256(tick) / int256(uint256(tickSpacing));
        if (tick < 0 && tick % int32(tickSpacing) != 0) {
            compressed -= 1; // Round towards negative infinity
        }
        uint256 rawIndex = uint256(compressed + int256(BITMAP_OFFSET));
        return rawIndex / 256;
    }

    constructor(TickBitmapInfo[] memory allPoolInfo) {
        // First pass: calculate total number of words we need
        uint256 totalPools = allPoolInfo.length;
        
        // For each pool, we'll return the bitmaps as a flat array
        // The structure returned will be: uint256[][] where each inner array is the bitmaps for one pool
        uint256[][] memory allBitmaps = new uint256[][](totalPools);

        for (uint256 i = 0; i < totalPools; ++i) {
            bytes32 poolId = allPoolInfo[i].poolId;
            uint32 tickSpacing = allPoolInfo[i].tickSpacing;
            int32 minTick = allPoolInfo[i].minTick;
            int32 maxTick = allPoolInfo[i].maxTick;
            
            if (tickSpacing == 0) {
                allBitmaps[i] = new uint256[](0);
                continue;
            }

            uint256 minWord = tickToWord(minTick, tickSpacing);
            uint256 maxWord = tickToWord(maxTick, tickSpacing);
            
            if (maxWord < minWord) {
                allBitmaps[i] = new uint256[](0);
                continue;
            }

            uint256 numWords = maxWord - minWord + 1;
            
            // Limit to prevent excessive gas usage
            if (numWords > 5000) {
                numWords = 5000;
                maxWord = minWord + numWords - 1;
            }

            uint256[] memory bitmaps = new uint256[](numWords);
            
            // Calculate inner slot: keccak256(poolId, TICK_BITMAP_SLOT)
            bytes32 innerSlot = keccak256(abi.encode(poolId, TICK_BITMAP_SLOT));
            
            // Build storage slots array for batch sload
            bytes32[] memory slots = new bytes32[](numWords);
            for (uint256 j = 0; j < numWords; ++j) {
                uint256 word = minWord + j;
                slots[j] = keccak256(abi.encode(word, innerSlot));
            }
            
            // Call sload with all slots
            bytes memory callData = abi.encodePacked(bytes4(0x380eb4e0)); // sload() selector
            for (uint256 j = 0; j < slots.length; ++j) {
                callData = abi.encodePacked(callData, slots[j]);
            }
            
            (bool success, bytes memory result) = EKUBO_CORE.staticcall(callData);
            
            if (success && result.length >= numWords * 32) {
                for (uint256 j = 0; j < numWords; ++j) {
                    uint256 value;
                    assembly {
                        value := mload(add(result, add(32, mul(j, 32))))
                    }
                    bitmaps[j] = value;
                }
            }
            
            allBitmaps[i] = bitmaps;
        }

        bytes memory abiEncodedData = abi.encode(allBitmaps);

        assembly {
            let dataStart := add(abiEncodedData, 0x20)
            return(dataStart, sub(msize(), dataStart))
        }
    }
}

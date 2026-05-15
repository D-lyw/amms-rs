//SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *       deployment bytecode as payload.
 */
contract GetAlgebraPoolTickTableBatchRequest {
    int32 private constant TICK_TREE_SHIFT = 3466;
    int32 private constant TICK_TREE_LEAF_WORDS = 6932;

    constructor(address[] memory pools) {
        uint256[][] memory allTickTables = new uint256[][](pools.length);

        for (uint256 i = 0; i < pools.length; ++i) {
            IAlgebraPoolTickTree pool = IAlgebraPoolTickTree(pools[i]);
            uint32 root;
            try pool.tickTreeRoot() returns (uint32 r) {
                root = r;
            } catch {
                continue;
            }

            // Upper bound: 32 root bits * 256 leaf words * 2 packed values (wordPos, bitmap)
            uint256[] memory tickTables = new uint256[](16384);
            uint256 wordIdx = 0;

            for (uint8 nodeIdx = 0; nodeIdx < 32; ++nodeIdx) {
                if ((root & (uint32(1) << nodeIdx)) == 0) continue;

                uint256 secondLayer;
                try pool.tickTreeSecondLayer(int16(uint16(nodeIdx))) returns (uint256 sl) {
                    secondLayer = sl;
                } catch {
                    continue;
                }

                for (uint16 bitIdx = 0; bitIdx < 256; ++bitIdx) {
                    if ((secondLayer & (uint256(1) << bitIdx)) == 0) continue;

                    int32 leafIdx = int32(uint32(nodeIdx)) * 256 + int32(uint32(bitIdx));
                    if (leafIdx < 0 || leafIdx >= TICK_TREE_LEAF_WORDS) continue;

                    int32 wordPos = leafIdx - TICK_TREE_SHIFT;
                    if (wordPos < type(int16).min || wordPos > type(int16).max) continue;

                    uint256 bitmap;
                    try pool.tickTable(int16(wordPos)) returns (uint256 b) {
                        bitmap = b;
                    } catch {
                        continue;
                    }

                    if (bitmap == 0) continue;

                    tickTables[wordIdx] = uint256(int256(wordPos));
                    ++wordIdx;
                    tickTables[wordIdx] = bitmap;
                    ++wordIdx;
                }
            }

            assembly {
                mstore(tickTables, wordIdx)
            }

            allTickTables[i] = tickTables;
        }

        bytes memory abiEncodedData = abi.encode(allTickTables);

        assembly {
            let dataStart := add(abiEncodedData, 0x20)
            return(dataStart, sub(msize(), dataStart))
        }
    }
}

interface IAlgebraPoolTickTree {
    function tickTreeRoot() external view returns (uint32);
    function tickTreeSecondLayer(int16 node) external view returns (uint256);
    function tickTable(int16 wordPosition) external view returns (uint256);
}

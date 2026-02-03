// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

interface IEkuboCoreDataFetcher {
     struct PoolKey {
        address token0;
        address token1;
        bytes32 config;
    }

    struct PoolState {
        uint160 sqrtRatio;
        int32 tick;
        uint128 liquidity;
    }

    // V2 Data Fetcher poolState function
    function poolState(PoolKey calldata key) external view returns (PoolState memory);
}

contract GetEkuboPoolStateBatchRequest {
    // Input struct matching Rust side
    struct BatchPoolKey {
        address token0;
        address token1;
        bytes32 config;
    }

    struct BatchPoolState {
        uint160 sqrtRatio;
        int32 tick;
        uint128 liquidity;
        bool success;
    }

    // Ekubo CoreDataFetcher (V2) on Mainnet
    address constant DATA_FETCHER = 0x208BB00c6b142351e4a431f6Dd323691ebb7C285;

    constructor(BatchPoolKey[] memory keys) {
        BatchPoolState[] memory results = new BatchPoolState[](keys.length);

        for (uint256 i = 0; i < keys.length; i++) {
            IEkuboCoreDataFetcher.PoolKey memory key = IEkuboCoreDataFetcher.PoolKey({
                token0: keys[i].token0,
                token1: keys[i].token1,
                config: keys[i].config
            });

            // Use low-level call or try/catch to ensure one failure doesn't revert whole batch
            try IEkuboCoreDataFetcher(DATA_FETCHER).poolState(key) returns (IEkuboCoreDataFetcher.PoolState memory state) {
                results[i] = BatchPoolState({
                    sqrtRatio: state.sqrtRatio,
                    tick: state.tick,
                    liquidity: state.liquidity,
                    success: true
                });
            } catch {
                 results[i].success = false;
            }
        }

        bytes memory data = abi.encode(results);
        assembly {
            return(add(data, 0x20), mload(data))
        }
    }
}

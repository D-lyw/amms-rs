//SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *       deployment bytecode as payload.
 */
contract GetAlgebraPoolStateBatchRequest {
    struct PoolState {
        bool ok;
        uint256 sqrtPrice;
        int24 tick;
        uint128 activeLiquidity;
        uint16 lastFee;
        uint8 pluginConfig;
        int24 nextTick;
        int24 previousTick;
        uint16 communityFee;
        bool unlocked;
        address plugin;
    }

    constructor(address[] memory pools) {
        PoolState[] memory allState = new PoolState[](pools.length);

        for (uint256 i = 0; i < pools.length; ++i) {
            IAlgebraPoolState pool = IAlgebraPoolState(pools[i]);

            try pool.safelyGetStateOfAMM() returns (
                uint160 sqrtPrice,
                int24 tick,
                uint16 lastFee,
                uint8 pluginConfig,
                uint128 activeLiquidity,
                int24 nextTick,
                int24 previousTick
            ) {
                PoolState memory state;
                state.ok = true;
                state.sqrtPrice = uint256(sqrtPrice);
                state.tick = tick;
                state.activeLiquidity = activeLiquidity;
                state.lastFee = lastFee;
                state.pluginConfig = pluginConfig;
                state.nextTick = nextTick;
                state.previousTick = previousTick;

                try pool.globalState() returns (
                    uint160,
                    int24,
                    uint16,
                    uint8,
                    uint16 communityFee,
                    bool unlocked
                ) {
                    state.communityFee = communityFee;
                    state.unlocked = unlocked;
                } catch {
                    try pool.isUnlocked() returns (bool unlocked2) {
                        state.unlocked = unlocked2;
                    } catch {}
                }

                try pool.plugin() returns (address pluginAddr) {
                    state.plugin = pluginAddr;
                } catch {}

                allState[i] = state;
            } catch {
                continue;
            }
        }

        bytes memory abiEncodedData = abi.encode(allState);

        assembly {
            let dataStart := add(abiEncodedData, 0x20)
            return(dataStart, sub(msize(), dataStart))
        }
    }
}

interface IAlgebraPoolState {
    function safelyGetStateOfAMM()
        external
        view
        returns (
            uint160 sqrtPrice,
            int24 tick,
            uint16 lastFee,
            uint8 pluginConfig,
            uint128 activeLiquidity,
            int24 nextTick,
            int24 previousTick
        );

    function globalState()
        external
        view
        returns (
            uint160 price,
            int24 tick,
            uint16 lastFee,
            uint8 pluginConfig,
            uint16 communityFee,
            bool unlocked
        );

    function isUnlocked() external view returns (bool);

    function plugin() external view returns (address);
}

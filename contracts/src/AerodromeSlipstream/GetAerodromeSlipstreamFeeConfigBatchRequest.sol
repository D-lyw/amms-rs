//SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *       deployment bytecode as payload.
 * @notice Batch-fetches full fee context for multiple Slipstream pools in one call:
 *         - factory
 *         - feeModule
 *         - tickSpacing
 *         - factory.tickSpacingToFee(tickSpacing)
 *         - dynamicFeeConfig(pool)
 */
contract GetAerodromeSlipstreamFeeConfigBatchRequest {
    struct FeeConfigData {
        address factory;
        address feeModule;
        bool factoryOk;
        bool tickSpacingOk;
        bool tickSpacingFeeOk;
        bool feeModuleOk;
        bool dynamicFeeConfigOk;
        int24 tickSpacing;
        uint24 tickSpacingFee;
        uint24 baseFee;
        uint24 feeCap;
        uint64 scalingFactor;
        bool initialFeeEnabled;
        uint24 initialFee;
    }

    constructor(address[] memory pools) {
        FeeConfigData[] memory results = new FeeConfigData[](pools.length);

        for (uint256 i = 0; i < pools.length; ++i) {
            address poolAddr = pools[i];

            // 1. Get factory and tickSpacing from pool
            address factory;
            int24 tickSpacing;
            try ICLPool(poolAddr).factory() returns (address _factory) {
                factory = _factory;
                results[i].factoryOk = true;
            } catch {
                continue;
            }

            if (factory == address(0)) continue;

            try ICLPool(poolAddr).tickSpacing() returns (int24 _ts) {
                tickSpacing = _ts;
                results[i].tickSpacingOk = true;
            } catch {
                results[i].factory = factory;
                continue;
            }

            results[i].factory = factory;
            results[i].tickSpacing = tickSpacing;

            // 2. Resolve default fee mapping from factory
            try ICLFactory(factory).tickSpacingToFee(tickSpacing) returns (uint24 _tsFee) {
                results[i].tickSpacingFee = _tsFee;
                results[i].tickSpacingFeeOk = true;
            } catch {
                // Keep going, dynamicFeeConfig may still be available.
            }

            // 3. Get swapFeeModule from factory
            address feeModule;
            try ICLFactory(factory).swapFeeModule() returns (address _fm) {
                feeModule = _fm;
                results[i].feeModuleOk = true;
            } catch {
                // Still save factory/tickSpacing context even if feeModule fetch fails
                continue;
            }

            results[i].feeModule = feeModule;

            if (feeModule == address(0)) continue;

            // 4. Get dynamicFeeConfig from feeModule
            try IDynamicFeeModule(feeModule).dynamicFeeConfig(poolAddr) returns (
                uint24 baseFee,
                uint24 feeCap,
                uint64 scalingFactor,
                bool initialFeeEnabled,
                uint24 initialFee
            ) {
                results[i].baseFee = baseFee;
                results[i].feeCap = feeCap;
                results[i].scalingFactor = scalingFactor;
                results[i].initialFeeEnabled = initialFeeEnabled;
                results[i].initialFee = initialFee;
                results[i].dynamicFeeConfigOk = true;
            } catch {
                continue;
            }
        }

        bytes memory abiEncodedData = abi.encode(results);

        assembly {
            let dataStart := add(abiEncodedData, 0x20)
            return(dataStart, sub(msize(), dataStart))
        }
    }
}

interface ICLPool {
    function factory() external view returns (address);
    function tickSpacing() external view returns (int24);
}

interface ICLFactory {
    function swapFeeModule() external view returns (address);
    function tickSpacingToFee(int24 tickSpacing) external view returns (uint24);
}

interface IDynamicFeeModule {
    function dynamicFeeConfig(address pool) external view returns (
        uint24 baseFee,
        uint24 feeCap,
        uint64 scalingFactor,
        bool initialFeeEnabled,
        uint24 initialFee
    );
}

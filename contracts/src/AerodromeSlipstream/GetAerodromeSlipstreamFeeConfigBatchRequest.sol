//SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *       deployment bytecode as payload.
 * @notice Batch-fetches factory address, swapFeeModule address, and dynamicFeeConfig
 *         for multiple Slipstream pools in a single call.
 */
contract GetAerodromeSlipstreamFeeConfigBatchRequest {
    struct FeeConfigData {
        address factory;
        address feeModule;
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

            // 1. Get factory from pool
            address factory;
            try ICLPool(poolAddr).factory() returns (address _factory) {
                factory = _factory;
            } catch {
                continue;
            }

            if (factory == address(0)) continue;

            // 2. Get swapFeeModule from factory
            address feeModule;
            try ICLFactory(factory).swapFeeModule() returns (address _fm) {
                feeModule = _fm;
            } catch {
                // Still save factory even if feeModule fetch fails
                results[i].factory = factory;
                continue;
            }

            results[i].factory = factory;
            results[i].feeModule = feeModule;

            if (feeModule == address(0)) continue;

            // 3. Get dynamicFeeConfig from feeModule
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
}

interface ICLFactory {
    function swapFeeModule() external view returns (address);
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

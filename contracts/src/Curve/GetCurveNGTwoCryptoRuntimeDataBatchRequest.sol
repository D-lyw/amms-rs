// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @title GetCurveNGTwoCryptoRuntimeDataBatchRequest
 * @notice Batch fetch runtime-only data for Curve NG TwoCrypto pools
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *      deployment bytecode as payload.
 */

interface ICurveTwoCryptoRuntimePool {
    function balances(uint256 i) external view returns (uint256);
    function price_scale() external view returns (uint256);
    function D() external view returns (uint256);
}

contract GetCurveNGTwoCryptoRuntimeDataBatchRequest {
    struct TwoCryptoRuntimeData {
        address poolAddress;
        uint256[] balances;
        uint256 priceScale;
        uint256 d;
    }

    constructor(address[] memory pools) {
        TwoCryptoRuntimeData[] memory results = new TwoCryptoRuntimeData[](pools.length);

        for (uint256 i = 0; i < pools.length; i++) {
            address poolAddress = pools[i];
            TwoCryptoRuntimeData memory data;
            data.poolAddress = poolAddress;

            data.balances = new uint256[](2);
            for (uint256 j = 0; j < 2; j++) {
                data.balances[j] = safeGetUint256(poolAddress, abi.encodeWithSelector(ICurveTwoCryptoRuntimePool.balances.selector, j));
            }

            data.priceScale = safeGetUint256(poolAddress, abi.encodeWithSelector(ICurveTwoCryptoRuntimePool.price_scale.selector));
            data.d = safeGetUint256(poolAddress, abi.encodeWithSelector(ICurveTwoCryptoRuntimePool.D.selector));

            results[i] = data;
        }

        bytes memory encoded = abi.encode(results);
        assembly {
            return(add(encoded, 32), mload(encoded))
        }
    }

    function safeGetUint256(address target, bytes memory callData) internal view returns (uint256 value) {
        (bool success, bytes memory ret) = target.staticcall(callData);
        if (success && ret.length >= 32) {
            value = abi.decode(ret, (uint256));
        }
    }
}

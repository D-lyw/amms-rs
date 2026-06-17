// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @title GetCurveNGTriCryptoRuntimeDataBatchRequest
 * @notice Batch fetch runtime-only data for Curve NG TriCrypto pools
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *      deployment bytecode as payload.
 */

interface ICurveTriCryptoRuntimePool {
    function balances(uint256 i) external view returns (uint256);
    function price_scale(uint256 i) external view returns (uint256);
    function D() external view returns (uint256);
}

contract GetCurveNGTriCryptoRuntimeDataBatchRequest {
    struct TriCryptoRuntimeData {
        address poolAddress;
        uint256[] balances;
        uint256[] priceScale;
        uint256 d;
    }

    constructor(address[] memory pools) {
        TriCryptoRuntimeData[] memory results = new TriCryptoRuntimeData[](pools.length);

        for (uint256 i = 0; i < pools.length; i++) {
            address poolAddress = pools[i];
            TriCryptoRuntimeData memory data;
            data.poolAddress = poolAddress;

            data.balances = new uint256[](3);
            for (uint256 j = 0; j < 3; j++) {
                data.balances[j] = safeGetUint256(poolAddress, abi.encodeWithSelector(ICurveTriCryptoRuntimePool.balances.selector, j));
            }

            data.priceScale = new uint256[](2);
            for (uint256 j = 0; j < 2; j++) {
                data.priceScale[j] = safeGetUint256(poolAddress, abi.encodeWithSelector(ICurveTriCryptoRuntimePool.price_scale.selector, j));
            }

            data.d = safeGetUint256(poolAddress, abi.encodeWithSelector(ICurveTriCryptoRuntimePool.D.selector));

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

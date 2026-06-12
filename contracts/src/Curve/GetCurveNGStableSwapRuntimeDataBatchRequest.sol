// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @title GetCurveNGStableSwapRuntimeDataBatchRequest
 * @notice Batch fetch runtime-only data for Curve NG StableSwap pools
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *      deployment bytecode as payload.
 */

interface ICurveNGStableRuntimePool {
    function coins(uint256 i) external view returns (address);
    function balances(uint256 i) external view returns (uint256);
    function A() external view returns (uint256);
    function fee() external view returns (uint256);
    function admin_fee() external view returns (uint256);
    function offpeg_fee_multiplier() external view returns (uint256);
    function stored_rates() external view returns (uint256[] memory);
}

interface ICurveNGStableRuntimePoolInt128 {
    function coins(int128 i) external view returns (address);
    function balances(int128 i) external view returns (uint256);
}

contract GetCurveNGStableSwapRuntimeDataBatchRequest {
    struct RuntimePoolData {
        address poolAddress;
        uint256[] balances;
        uint256 amp;
        uint256 fee;
        uint256 adminFee;
        uint256[] rates;
        bool supportsStoredRates;
        bool supportsOffpegFeeMultiplier;
        uint256 offpegFeeMultiplier;
    }

    constructor(address[] memory pools) {
        RuntimePoolData[] memory results = new RuntimePoolData[](pools.length);

        for (uint256 i = 0; i < pools.length; i++) {
            address poolAddress = pools[i];
            RuntimePoolData memory data;
            data.poolAddress = poolAddress;

            ICurveNGStableRuntimePool pool = ICurveNGStableRuntimePool(poolAddress);

            address[] memory tempCoins = new address[](8);
            uint256[] memory tempBalances = new uint256[](8);
            uint8 nCoins = 0;
            bool useInt128 = false;

            for (uint256 j = 0; j < 8; j++) {
                address coin = address(0);

                if (!useInt128) {
                    try pool.coins(j) returns (address c) {
                        coin = c;
                    } catch {
                        if (j == 0) {
                            useInt128 = true;
                        } else {
                            break;
                        }
                    }
                }

                if (useInt128) {
                    try ICurveNGStableRuntimePoolInt128(poolAddress).coins(int128(int256(j))) returns (address c) {
                        coin = c;
                    } catch {
                        break;
                    }
                }

                if (coin == address(0)) break;

                tempCoins[j] = coin;
                nCoins++;

                if (!useInt128) {
                    try pool.balances(j) returns (uint256 balance) {
                        tempBalances[j] = balance;
                    } catch {}
                } else {
                    try ICurveNGStableRuntimePoolInt128(poolAddress).balances(int128(int256(j))) returns (uint256 balance) {
                        tempBalances[j] = balance;
                    } catch {}
                }
            }

            data.balances = new uint256[](nCoins);
            for (uint256 j = 0; j < nCoins; j++) {
                data.balances[j] = tempBalances[j];
            }

            data.amp = safeGetUint256(poolAddress, abi.encodeWithSelector(ICurveNGStableRuntimePool.A.selector));
            data.fee = safeGetUint256(poolAddress, abi.encodeWithSelector(ICurveNGStableRuntimePool.fee.selector));
            data.adminFee = safeGetUint256(poolAddress, abi.encodeWithSelector(ICurveNGStableRuntimePool.admin_fee.selector));

            try pool.stored_rates() returns (uint256[] memory rs) {
                data.rates = rs;
                data.supportsStoredRates = true;
            } catch {
                data.rates = new uint256[](nCoins);
                for (uint256 k = 0; k < nCoins; k++) {
                    data.rates[k] = 1e18;
                }
                data.supportsStoredRates = false;
            }

            try pool.offpeg_fee_multiplier() returns (uint256 m) {
                data.offpegFeeMultiplier = m;
                data.supportsOffpegFeeMultiplier = true;
            } catch {
                data.supportsOffpegFeeMultiplier = false;
            }

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

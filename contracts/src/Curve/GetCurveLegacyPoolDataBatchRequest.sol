// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @title GetCurveLegacyPoolDataBatchRequest
 * @notice Batch fetch pool data for Curve Legacy (V1/V2) pools
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *      deployment bytecode as payload.
 */

interface IERC20 {
    function decimals() external view returns (uint8);
}

interface ICurveLegacyPool {
    // Base methods
    function coins(uint256 i) external view returns (address);
    function balances(uint256 i) external view returns (uint256);
    function A() external view returns (uint256);
    function fee() external view returns (uint256);
    function admin_fee() external view returns (uint256);
    
    // CryptoSwap V2 methods
    function D() external view returns (uint256);
    function gamma() external view returns (uint256);
    function mid_fee() external view returns (uint256);
    function out_fee() external view returns (uint256);
    function fee_gamma() external view returns (uint256);
    function allowed_extra_profit() external view returns (uint256);
    function adjustment_step() external view returns (uint256);
    function ma_half_time() external view returns (uint256);
    // price_scale is handled via manual selector encoding to support both array and scalar
    // function price_scale(uint256 i) external view returns (uint256);
    function stored_rates(uint256 i) external view returns (uint256); // For Lending/Metapools
}

interface ICurveLegacyStableSwap {
    function coins(int128 i) external view returns (address);
    function balances(int128 i) external view returns (uint256);
}

contract GetCurveLegacyPoolDataBatchRequest {
    struct PoolInput {
        address pool;
        uint8 poolType; // 0=StableSwap, 1=CryptoSwap
    }

    struct PoolData {
        address poolAddress;
        uint8 poolType;
        uint8 nCoins;
        address[] coins;
        uint256[] balances;
        uint8[] decimals;
        uint256 amp;
        uint256 fee;
        uint256 adminFee;
        // CryptoSwap specific
        uint256 d;
        uint256 gamma;
        uint256 midFee;
        uint256 outFee;
        uint256 feeGamma;
        uint256 allowedExtraProfit;
        uint256 adjustmentStep;
        uint256 maHalfTime;
        uint256[] priceScale;
        // Rates (default 1e18 for Legacy pools)
        uint256[] rates;
    }

    constructor(PoolInput[] memory inputs) {
        PoolData[] memory results = new PoolData[](inputs.length);

        for (uint256 i = 0; i < inputs.length; i++) {
            PoolInput memory input = inputs[i];
            PoolData memory data;
            
            data.poolAddress = input.pool;
            data.poolType = input.poolType;

            ICurveLegacyPool pool = ICurveLegacyPool(input.pool);

            // 1. Fetch coins and balances (max 8 coins)
            address[] memory tempCoins = new address[](8);
            uint256[] memory tempBalances = new uint256[](8);
            uint8[] memory tempDecimals = new uint8[](8);
            uint8 nCoins = 0;

            bool useInt128 = false;

            for (uint256 j = 0; j < 8; j++) {
                address coin = address(0);
                
                // Determine if we should use uint256 or int128
                if (!useInt128) {
                    try pool.coins(j) returns (address c) {
                        coin = c;
                    } catch {
                        // If failed at index 0, it might be an int128 pool
                        if (j == 0 && input.poolType == 0) {
                            useInt128 = true;
                        } else {
                            // If failed at index > 0, we found all coins (or standard interface end)
                            break;
                        }
                    }
                }

                if (useInt128) {
                    // Try int128 (Legacy StableSwap often uses int128)
                    try ICurveLegacyStableSwap(input.pool).coins(int128(int256(j))) returns (address c) {
                        coin = c;
                    } catch {
                        break;
                    }
                }

                if (coin == address(0)) break;
                
                tempCoins[j] = coin;
                nCoins++;

                // Balance fetching based on interface detected
                if (!useInt128) {
                    try pool.balances(j) returns (uint256 balance) {
                        tempBalances[j] = balance;
                    } catch {}
                } else {
                     try ICurveLegacyStableSwap(input.pool).balances(int128(int256(j))) returns (uint256 balance) {
                        tempBalances[j] = balance;
                    } catch {}
                }

                // Decimals
                if (coin == address(0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE)) {
                    tempDecimals[j] = 18; // Native ETH
                } else {
                    try IERC20(coin).decimals() returns (uint8 dec) {
                        tempDecimals[j] = dec;
                    } catch {
                        tempDecimals[j] = 18;
                    }
                }
            }

            data.nCoins = nCoins;
            
            // Copy to correctly sized arrays
            data.coins = new address[](nCoins);
            data.balances = new uint256[](nCoins);
            data.decimals = new uint8[](nCoins);
            for (uint256 j = 0; j < nCoins; j++) {
                data.coins[j] = tempCoins[j];
                data.balances[j] = tempBalances[j];
                data.decimals[j] = tempDecimals[j];
            }

            // 2. Fetch A parameter
            data.amp = safeGetUint256(input.pool, abi.encodeWithSelector(ICurveLegacyPool.A.selector));
            data.fee = safeGetUint256(input.pool, abi.encodeWithSelector(ICurveLegacyPool.fee.selector));
            data.adminFee = safeGetUint256(input.pool, abi.encodeWithSelector(ICurveLegacyPool.admin_fee.selector));

            // 5. CryptoSwap specific parameters
            if (input.poolType == 1) {
                data.d = safeGetUint256(input.pool, abi.encodeWithSelector(ICurveLegacyPool.D.selector));
                data.gamma = safeGetUint256(input.pool, abi.encodeWithSelector(ICurveLegacyPool.gamma.selector));
                data.midFee = safeGetUint256(input.pool, abi.encodeWithSelector(ICurveLegacyPool.mid_fee.selector));
                data.outFee = safeGetUint256(input.pool, abi.encodeWithSelector(ICurveLegacyPool.out_fee.selector));
                data.feeGamma = safeGetUint256(input.pool, abi.encodeWithSelector(ICurveLegacyPool.fee_gamma.selector));
                data.allowedExtraProfit = safeGetUint256(input.pool, abi.encodeWithSelector(ICurveLegacyPool.allowed_extra_profit.selector));
                data.adjustmentStep = safeGetUint256(input.pool, abi.encodeWithSelector(ICurveLegacyPool.adjustment_step.selector));
                data.maHalfTime = safeGetUint256(input.pool, abi.encodeWithSelector(ICurveLegacyPool.ma_half_time.selector));

                // Price scale: nCoins - 1 values
                uint256 numPriceScales = nCoins > 1 ? nCoins - 1 : 0;
                data.priceScale = new uint256[](numPriceScales);
                for (uint256 k = 0; k < numPriceScales; k++) {
                    // Try array access first: price_scale(k)
                    uint256 val = safeGetUint256(input.pool, abi.encodeWithSignature("price_scale(uint256)", k));
                    
                    // If failed (0) and we only expect 1 value (2-coin pool), try scalar: price_scale()
                    if (val == 0 && numPriceScales == 1) {
                         val = safeGetUint256(input.pool, abi.encodeWithSignature("price_scale()"));
                    }
                    data.priceScale[k] = val;
                }
            } else {
                // StableSwap: empty price scale
                data.priceScale = new uint256[](0);
            }

            // Try to fetch stored_rates (if available)
            // Some Legacy pools (Lending, Metapools) implement stored_rates
            bool hasStoredRates = false;
            // Check index 0 first
            try ICurveLegacyPool(input.pool).stored_rates(0) returns (uint256) {
                hasStoredRates = true;
            } catch {}

            if (hasStoredRates) {
                data.rates = new uint256[](nCoins);
                for (uint256 k = 0; k < nCoins; k++) {
                    try ICurveLegacyPool(input.pool).stored_rates(k) returns (uint256 r) {
                        data.rates[k] = r;
                    } catch {
                        // If fetching generic index fails after index 0 passed, default to 0 (Rust handles fallback)
                        data.rates[k] = 0;
                    }
                }
            } else {
                // Return empty array to signal "use decimals fallback" to Rust
                data.rates = new uint256[](0);
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

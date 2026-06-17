// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @title GetCurveNGPoolDataBatchRequest
 * @notice Batch fetch pool data for Curve NG (Next-Generation) pools
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *      deployment bytecode as payload.
 */

interface IERC20 {
    function decimals() external view returns (uint8);
}

interface ICurveNGPool {
    // Base methods
    function coins(uint256 i) external view returns (address);
    function balances(uint256 i) external view returns (uint256);
    function A() external view returns (uint256);
    function fee() external view returns (uint256);
    function admin_fee() external view returns (uint256);
    function offpeg_fee_multiplier() external view returns (uint256);
    
    // StableSwap-NG: stored_rates() for rebasing tokens
    function stored_rates() external view returns (uint256[] memory);
    function get_dy(uint256 i, uint256 j, uint256 dx) external view returns (uint256);
    
    // CryptoSwap methods
    function D() external view returns (uint256);
    function gamma() external view returns (uint256);
    function mid_fee() external view returns (uint256);
    function out_fee() external view returns (uint256);
    function fee_gamma() external view returns (uint256);
}

interface ICurveNGStableSwap {
    function coins(int128 i) external view returns (address);
    function balances(int128 i) external view returns (uint256);
    function get_dy(int128 i, int128 j, uint256 dx) external view returns (uint256);
}

interface ICurveTwoCrypto {
    function price_scale() external view returns (uint256);
}

interface ICurveTriCrypto {
    function price_scale(uint256 i) external view returns (uint256);
}

contract GetCurveNGPoolDataBatchRequest {
    struct PoolInput {
        address pool;
        uint8 poolType; // 0=StableSwap, 1=TwoCrypto, 2=TriCrypto
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
        uint256[] priceScale;
        // StableSwap-NG: stored_rates for rebasing tokens
        uint256[] rates;
        // Per-coin asset type (0=Standard, 1=Oracle, 2=Rebasing, 3=ERC4626)
        uint8[] assetTypes;
        // Capability profile
        bool supportsStoredRates;
        bool supportsOffpegFeeMultiplier;
        // 0=Unknown, 1=Uint256, 2=Int128
        uint8 coinsIndexSignature;
        uint8 balancesIndexSignature;
        uint8 getDyIndexSignature;
        // capability schema version
        uint8 capabilityVersion;
        // Stable offpeg fee multiplier value when available
        uint256 offpegFeeMultiplier;
    }

    constructor(PoolInput[] memory inputs) {
        PoolData[] memory results = new PoolData[](inputs.length);

        for (uint256 i = 0; i < inputs.length; i++) {
            PoolInput memory input = inputs[i];
            PoolData memory data;
            
            data.poolAddress = input.pool;
            data.poolType = input.poolType;
            data.capabilityVersion = 1;

            ICurveNGPool pool = ICurveNGPool(input.pool);

            // 1. Fetch coins and balances (max 8 coins)
            address[] memory tempCoins = new address[](8);
            uint256[] memory tempBalances = new uint256[](8);
            uint8[] memory tempDecimals = new uint8[](8);
            uint8 nCoins = 0;
            bool useInt128 = false;
            data.coinsIndexSignature = 0;
            data.balancesIndexSignature = 0;
            data.getDyIndexSignature = 0;

            for (uint256 j = 0; j < 8; j++) {
                address coin = address(0);
                
                // Determine if we should use uint256 or int128
                if (!useInt128) {
                    try pool.coins(j) returns (address c) {
                        coin = c;
                        if (data.coinsIndexSignature == 0) {
                            data.coinsIndexSignature = 1; // Uint256
                        }
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
                    // Try int128
                    try ICurveNGStableSwap(input.pool).coins(int128(int256(j))) returns (address c) {
                        coin = c;
                        if (data.coinsIndexSignature == 0) {
                            data.coinsIndexSignature = 2; // Int128
                        }
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
                        if (data.balancesIndexSignature == 0) {
                            data.balancesIndexSignature = 1; // Uint256
                        }
                    } catch {}
                } else {
                     try ICurveNGStableSwap(input.pool).balances(int128(int256(j))) returns (uint256 balance) {
                        tempBalances[j] = balance;
                        if (data.balancesIndexSignature == 0) {
                            data.balancesIndexSignature = 2; // Int128
                        }
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

            // 2. Param fetching using safe calls to avoid reverts/panics on invalid return data
            data.amp = safeGetUint256(input.pool, abi.encodeWithSelector(ICurveNGPool.A.selector));
            data.fee = safeGetUint256(input.pool, abi.encodeWithSelector(ICurveNGPool.fee.selector));
            data.adminFee = safeGetUint256(input.pool, abi.encodeWithSelector(ICurveNGPool.admin_fee.selector));

            // 5. CryptoSwap specific parameters (TwoCrypto or TriCrypto)
            if (input.poolType == 1 || input.poolType == 2) {
                data.d = safeGetUint256(input.pool, abi.encodeWithSelector(ICurveNGPool.D.selector));
                data.gamma = safeGetUint256(input.pool, abi.encodeWithSelector(ICurveNGPool.gamma.selector));
                data.midFee = safeGetUint256(input.pool, abi.encodeWithSelector(ICurveNGPool.mid_fee.selector));
                data.outFee = safeGetUint256(input.pool, abi.encodeWithSelector(ICurveNGPool.out_fee.selector));
                data.feeGamma = safeGetUint256(input.pool, abi.encodeWithSelector(ICurveNGPool.fee_gamma.selector));

                // Price scale
                if (input.poolType == 1) {
                    // TwoCrypto: single price_scale()
                    data.priceScale = new uint256[](1);
                    data.priceScale[0] = safeGetUint256(input.pool, abi.encodeWithSelector(ICurveTwoCrypto.price_scale.selector));
                } else if (input.poolType == 2) {
                    // TriCrypto: price_scale(i) for i in [0, nCoins-2]
                    uint256 numPriceScales = nCoins > 1 ? nCoins - 1 : 0;
                    data.priceScale = new uint256[](numPriceScales);
                    for (uint256 k = 0; k < numPriceScales; k++) {
                        data.priceScale[k] = safeGetUint256(input.pool, abi.encodeWithSelector(ICurveTriCrypto.price_scale.selector, k));
                    }
                }
            } else {
                // StableSwap: empty price scale
                data.priceScale = new uint256[](0);
            }

            // Fetch stored_rates for StableSwap-NG (poolType == 0)
            if (input.poolType == 0) {
                try ICurveNGPool(input.pool).stored_rates() returns (uint256[] memory rs) {
                    data.rates = rs;
                    data.supportsStoredRates = true;
                } catch {
                    // Default to 1e18 for each coin
                    data.rates = new uint256[](nCoins);
                    for (uint256 k = 0; k < nCoins; k++) {
                        data.rates[k] = 1e18;
                    }
                    data.supportsStoredRates = false;
                }

                try ICurveNGPool(input.pool).offpeg_fee_multiplier() returns (uint256 m) {
                    data.offpegFeeMultiplier = m;
                    data.supportsOffpegFeeMultiplier = true;
                } catch {
                    data.supportsOffpegFeeMultiplier = false;
                }

                // Fetch per-coin asset type (Stableswap-ng only). Default to 0 (Standard) on failure.
                data.assetTypes = new uint8[](nCoins);
                for (uint256 k = 0; k < nCoins; k++) {
                    (bool atSuccess, bytes memory atRet) = input.pool.staticcall(
                        abi.encodeWithSignature("asset_types(uint256)", k)
                    );
                    if (atSuccess && atRet.length >= 32) {
                        data.assetTypes[k] = abi.decode(atRet, (uint8));
                    } else {
                        data.assetTypes[k] = 0;
                    }
                }

                if (nCoins > 1) {
                    try ICurveNGStableSwap(input.pool).get_dy(0, 1, 1) returns (uint256) {
                        data.getDyIndexSignature = 2; // Int128
                    } catch {
                        try ICurveNGPool(input.pool).get_dy(0, 1, 1) returns (uint256) {
                            data.getDyIndexSignature = 1; // Uint256
                        } catch {
                            data.getDyIndexSignature = 0;
                        }
                    }
                }
            } else {
                // CryptoSwap: rates are handled via price_scale
                data.rates = new uint256[](nCoins);
                for (uint256 k = 0; k < nCoins; k++) {
                    data.rates[k] = 1e18;
                }
                data.assetTypes = new uint8[](nCoins);
                data.supportsStoredRates = false;
                data.supportsOffpegFeeMultiplier = false;
                data.getDyIndexSignature = 1; // uint256
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

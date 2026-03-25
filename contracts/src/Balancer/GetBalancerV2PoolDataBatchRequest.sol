// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

interface IVault {
    function getPoolTokens(bytes32 poolId) external view returns (address[] memory tokens, uint256[] memory balances, uint256 lastChangeBlock);
}

interface IWeightedPool {
    function getNormalizedWeights() external view returns (uint256[] memory);
    function getSwapFeePercentage() external view returns (uint256);
    function getRateProviders() external view returns (address[] memory);
}

interface IStablePool {
    function getAmplificationParameter() external view returns (uint256 value, bool isUpdating, uint256 precision);
    function getSwapFeePercentage() external view returns (uint256);
    function getRateProviders() external view returns (address[] memory);
    function getScalingFactors() external view returns (uint256[] memory);
}

interface IComposableStablePool {
    function getBptIndex() external view returns (uint256);
    function getTokenRateCache(IERC20 token)
        external
        view
        returns (
            uint256 rate,
            uint256 oldRate,
            uint256 duration,
            uint256 expires
        );
}

interface IERC20 {
    function decimals() external view returns (uint8);
}

interface IRateProvider {
    function getRate() external view returns (uint256);
}

contract GetBalancerV2PoolDataBatchRequest {
    struct PoolData {
        bytes32 poolId;
        address poolAddress;
        uint16 poolType;
        address[] tokens;
        uint256[] balances;
        uint16[] decimals;
        uint256[] weights;
        uint256 amp;
        uint256 swapFee;
        uint256 bptIndex;
        address[] rateProviders;
        uint256[] rates;
        uint256[] scalingFactors;
    }

    constructor(address vault, bytes32[] memory poolIds, address[] memory poolAddresses, uint16[] memory poolTypes) {
        PoolData[] memory data = new PoolData[](poolIds.length);
        
        for (uint256 i = 0; i < poolIds.length; i++) {
            bytes32 poolId = poolIds[i];
            address poolAddr = poolAddresses[i];
            uint16 pType = poolTypes[i];
            
            address[] memory tokens;
            uint256[] memory balances;
            
            try IVault(vault).getPoolTokens(poolId) returns (address[] memory t, uint256[] memory b, uint256) {
                tokens = t;
                balances = b;
            } catch {
                // If getPoolTokens fails, skip this pool or return empty
                continue;
            }
            
            data[i].poolId = poolId;
            data[i].poolAddress = poolAddr;
            data[i].poolType = pType;
            data[i].tokens = tokens;
            data[i].balances = balances;
            
            // Fetch Decimals
            uint16[] memory tokenDecimals = new uint16[](tokens.length);
            for(uint256 j=0; j<tokens.length; j++) {
                try IERC20(tokens[j]).decimals() returns (uint8 d) {
                    tokenDecimals[j] = uint16(d);
                } catch {
                    tokenDecimals[j] = 18; // Default to 18 if call fails
                }
            }
            data[i].decimals = tokenDecimals;

            // Fetch Swap Fee
            try IWeightedPool(poolAddr).getSwapFeePercentage() returns (uint256 fee) {
                data[i].swapFee = fee;
            } catch {
                data[i].swapFee = 0;
            }

            // Fetch Rate Providers
            try IStablePool(poolAddr).getRateProviders() returns (address[] memory providers) {
                data[i].rateProviders = providers;
            } catch {
                 // Try WeightedPool interface (same signature)
                 try IWeightedPool(poolAddr).getRateProviders() returns (address[] memory providers) {
                     data[i].rateProviders = providers;
                 } catch {
                     address[] memory zeros = new address[](tokens.length);
                     data[i].rateProviders = zeros;
                 }
             }
             
             // Fetch Rates
             address[] memory rps = data[i].rateProviders;
             uint256[] memory rates = new uint256[](tokens.length);
             for(uint256 j=0; j<tokens.length; j++) {
                 if (j < rps.length && rps[j] != address(0)) {
                     // ComposableStable swaps use pool token-rate cache values.
                     // Use cached rate first to mirror on-chain querySwap path.
                     if (pType == 2) {
                         try IComposableStablePool(poolAddr).getTokenRateCache(IERC20(tokens[j])) returns (
                             uint256 cachedRate,
                             uint256,
                             uint256,
                             uint256
                         ) {
                             rates[j] = cachedRate;
                         } catch {
                             try IRateProvider(rps[j]).getRate() returns (uint256 r) {
                                 rates[j] = r;
                             } catch {
                                 rates[j] = 1e18;
                             }
                         }
                     } else {
                         try IRateProvider(rps[j]).getRate() returns (uint256 r) {
                             rates[j] = r;
                         } catch {
                             rates[j] = 1e18;
                         }
                     }
                 } else {
                     rates[j] = 1e18;
                 }
             }
            data[i].rates = rates;

            // Fetch scaling factors when available (Stable/ComposableStable/MetaStable family).
            try IStablePool(poolAddr).getScalingFactors() returns (uint256[] memory sfs) {
                data[i].scalingFactors = sfs;
            } catch {
                data[i].scalingFactors = new uint256[](0);
            }

             if (pType == 0) { // Weighted
                try IWeightedPool(poolAddr).getNormalizedWeights() returns (uint256[] memory w) {
                    data[i].weights = w;
                } catch {}
            } else if (pType == 1 || pType == 2) { // Stable or ComposableStable
                try IStablePool(poolAddr).getAmplificationParameter() returns (uint256 value, bool isUpdating, uint256 precision) {
                    data[i].amp = value;
                } catch {}
                
                if (pType == 2) { // ComposableStable
                     try IComposableStablePool(poolAddr).getBptIndex() returns (uint256 idx) {
                        data[i].bptIndex = idx;
                    } catch {
                        data[i].bptIndex = type(uint256).max;
                    }
                } else {
                     data[i].bptIndex = type(uint256).max;
                }
            } else {
                data[i].bptIndex = type(uint256).max;
            }
        }
        
        bytes memory encoded = abi.encode(data);
        assembly {
            return(add(encoded, 32), mload(encoded))
        }
    }
}

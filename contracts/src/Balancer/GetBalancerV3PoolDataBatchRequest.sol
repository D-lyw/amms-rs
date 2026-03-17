// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

// VaultExplorer is the helper contract for reading Balancer V3 state
interface IVaultExplorer {
    struct TokenInfo {
        uint8 tokenType;
        address rateProvider;
        bool paysYieldFees;
    }
    
    function getPoolTokenInfo(address pool) external view returns (
        address[] memory tokens,
        TokenInfo[] memory tokenInfo,
        uint256[] memory balancesRaw,
        uint256[] memory lastLiveBalances
    );
}

interface IBalancerV3Pool {
    function getTokens() external view returns (address[] memory);
    function getSwapFeePercentage() external view returns (uint256);
    // Some pools use static fee instead of dynamic fee
    function getStaticSwapFeePercentage() external view returns (uint256);
    // Weighted
    function getNormalizedWeights() external view returns (uint256[] memory);
    // Stable
    function getAmplificationParameter() external view returns (uint256 value, bool isUpdating, uint256 precision);
    // Rates
    function getRateProviders() external view returns (address[] memory);
}

interface IERC20 {
    function decimals() external view returns (uint8);
}

interface IRateProvider {
    function getRate() external view returns (uint256);
}

contract GetBalancerV3PoolDataBatchRequest {
    struct PoolData {
        address poolAddress;
        uint8 poolType; // 0: Weighted, 1: Stable, 2: Unknown
        address[] tokens;
        uint8[] decimals;
        uint256[] balances;
        uint256[] weights;
        uint256 amp;
        uint256 swapFee;
        address[] rateProviders;
        uint256[] rates;
    }

    constructor(address vaultExplorer, address[] memory pools) {
        // vaultExplorer: chain-specific VaultExplorer address
        // - Ethereum Mainnet: 0xFc2986feAB34713E659da84F3B1FA32c1da95832
        // - Base: 0xaD89051bEd8d96f045E8912aE1672c6C0bF8a85E
        
        PoolData[] memory data = new PoolData[](pools.length);
        
        for (uint256 i = 0; i < pools.length; i++) {
            address pool = pools[i];
            data[i].poolAddress = pool;
            
            // 1. Try to get data from VaultExplorer first (most complete)
            bool explorerSuccess = false;
            try IVaultExplorer(vaultExplorer).getPoolTokenInfo(pool) returns (
                address[] memory tokens,
                IVaultExplorer.TokenInfo[] memory tokenInfo,
                uint256[] memory balancesRaw,
                uint256[] memory lastLiveBalances
            ) {
                data[i].tokens = tokens;
                
                // Use lastLiveBalances for more accurate simulation
                uint256 len = tokens.length;
                data[i].decimals = new uint8[](len);
                data[i].balances = new uint256[](len);
                data[i].rates = new uint256[](len);
                data[i].rateProviders = new address[](len);
                
                for (uint256 j = 0; j < len; j++) {
                    // Always use raw balances (not scaled) to avoid double-scaling in Rust simulation
            if (j < balancesRaw.length) {
                data[i].balances[j] = balancesRaw[j];
            } else {
                data[i].balances[j] = 0;
            }        
                    // Get decimals
                    try IERC20(tokens[j]).decimals() returns (uint8 d) {
                        data[i].decimals[j] = d;
                    } catch {
                        data[i].decimals[j] = 18;
                    }
                    
                    // Get rate provider from tokenInfo
                    if (j < tokenInfo.length) {
                        data[i].rateProviders[j] = tokenInfo[j].rateProvider;
                    }
                }
                
                explorerSuccess = true;
            } catch {}
            
            // If VaultExplorer failed, try fallback to pool directly
            if (!explorerSuccess) {
                try IBalancerV3Pool(pool).getTokens() returns (address[] memory t) {
                    data[i].tokens = t;
                    
                    uint256 len = t.length;
                    data[i].decimals = new uint8[](len);
                    data[i].balances = new uint256[](len);
                    data[i].rates = new uint256[](len);
                    
                    for (uint256 j = 0; j < len; j++) {
                        try IERC20(t[j]).decimals() returns (uint8 d) {
                            data[i].decimals[j] = d;
                        } catch {
                            data[i].decimals[j] = 18;
                        }
                        // Balances will be 0 if we couldn't get them from VaultExplorer
                        data[i].rates[j] = 1e18;
                    }
                } catch { 
                    continue; 
                }
            }
            
            // 2. Swap Fee (with fallback to static fee)
            try IBalancerV3Pool(pool).getSwapFeePercentage() returns (uint256 fee) {
                data[i].swapFee = fee;
            } catch {
                // Fallback: some pools use getStaticSwapFeePercentage instead
                try IBalancerV3Pool(pool).getStaticSwapFeePercentage() returns (uint256 staticFee) {
                    data[i].swapFee = staticFee;
                } catch {}
            }

            // 3. Determine Type & Type-specific data
            // Try Weighted
            try IBalancerV3Pool(pool).getNormalizedWeights() returns (uint256[] memory w) {
                data[i].weights = w;
                data[i].poolType = 0;
            } catch {
                // Try Stable
                try IBalancerV3Pool(pool).getAmplificationParameter() returns (uint256 value, bool, uint256) {
                    data[i].amp = value;
                    data[i].poolType = 1;
                } catch {
                    data[i].poolType = 2; 
                }
            }

            // 4. Fetch Rates from Rate Providers (if not already fetched via tokenInfo)
            if (!explorerSuccess) {
                try IBalancerV3Pool(pool).getRateProviders() returns (address[] memory rp) {
                    data[i].rateProviders = rp;
                } catch {}
            }
            
            // Fetch rates from rate providers
            for (uint256 j = 0; j < data[i].tokens.length; j++) {
                if (j < data[i].rateProviders.length && data[i].rateProviders[j] != address(0)) {
                    try IRateProvider(data[i].rateProviders[j]).getRate() returns (uint256 r) {
                        data[i].rates[j] = r;
                    } catch {
                        data[i].rates[j] = 1e18;
                    }
                } else {
                    data[i].rates[j] = 1e18;
                }
            }
        }

        bytes memory encoded = abi.encode(data);
        assembly {
            return(add(encoded, 32), mload(encoded))
        }
    }
}

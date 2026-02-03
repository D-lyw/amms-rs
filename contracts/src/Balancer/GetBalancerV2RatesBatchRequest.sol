// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

interface IRateProvider {
    function getRate() external view returns (uint256);
}

contract GetBalancerV2RatesBatchRequest {
    constructor(address[] memory rateProviders) {
        uint256[] memory rates = new uint256[](rateProviders.length);
        
        for (uint256 i = 0; i < rateProviders.length; i++) {
            if (rateProviders[i] != address(0)) {
                try IRateProvider(rateProviders[i]).getRate() returns (uint256 r) {
                    rates[i] = r;
                } catch {
                    rates[i] = 0; // Return 0 to indicate failure/no rate (caller should handle)
                }
            } else {
                rates[i] = 0;
            }
        }
        
        bytes memory encoded = abi.encode(rates);
        assembly {
            return(add(encoded, 32), mload(encoded))
        }
    }
}

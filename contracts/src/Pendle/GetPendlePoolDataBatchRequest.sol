// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *       deployment bytecode as payload.
 */
contract GetPendlePoolDataBatchRequest {
    struct PendlePoolData {
        int256 totalPt;
        int256 totalSy;
        int256 scalarRoot;
        uint256 expiry;
        uint256 lnFeeRateRoot;
        uint256 reserveFeePercent;
        uint256 lastLnImpliedRate;
        address pt;
        address sy;
        uint256 syExchangeRate;
        address underlying;
        uint8 underlyingDecimals;
        uint8 ptDecimals;
        uint256 blockTimestamp;
    }

    constructor(address[] memory markets) {
        PendlePoolData[] memory allData = new PendlePoolData[](markets.length);

        for (uint256 i = 0; i < markets.length; ++i) {
            address marketAddress = markets[i];

            if (marketAddress.code.length == 0) continue;

            PendlePoolData memory data;

            // Get market state from IPMarket.readState()
            {
                (
                    int256 totalPt,
                    int256 totalSy,
                    , /* totalLp */
                    , /* treasury */
                    int256 scalarRoot,
                    uint256 expiry,
                    uint256 lnFeeRateRoot,
                    uint256 reserveFeePercent,
                    uint256 lastLnImpliedRate
                ) = IMarketState(marketAddress).readState(address(0));

                data.totalPt = totalPt;
                data.totalSy = totalSy;
                data.scalarRoot = scalarRoot;
                data.expiry = expiry;
                data.lnFeeRateRoot = lnFeeRateRoot;
                data.reserveFeePercent = reserveFeePercent;
                data.lastLnImpliedRate = lastLnImpliedRate;
            }

            // Get tokens from IPMarket.readTokens()
            {
                (address sy, address pt, ) = IMarketState(marketAddress).readTokens();
                data.sy = sy;
                data.pt = pt;
            }

            // Get SY exchange rate
            if (data.sy.code.length > 0) {
                data.syExchangeRate = ISYState(data.sy).exchangeRate();

                // Get underlying asset
                (, data.underlying, ) = ISYState(data.sy).assetInfo();

                // Get underlying decimals (low-level call for safety)
                if (data.underlying.code.length > 0) {
                    (bool success, bytes memory result) = data.underlying.call(
                        abi.encodeWithSignature("decimals()")
                    );
                    if (success && result.length == 32) {
                        data.underlyingDecimals = abi.decode(result, (uint8));
                    }
                }
            }

            // Get PT decimals (low-level call for safety)
            if (data.pt.code.length > 0) {
                (bool success, bytes memory result) = data.pt.call(
                    abi.encodeWithSignature("decimals()")
                );
                if (success && result.length == 32) {
                    data.ptDecimals = abi.decode(result, (uint8));
                }
            }

            data.blockTimestamp = block.timestamp;

            allData[i] = data;
        }

        bytes memory abiEncoded = abi.encode(allData);
        assembly {
            return(add(abiEncoded, 0x20), sub(msize(), add(abiEncoded, 0x20)))
        }
    }
}

interface IMarketState {
    function readState(address router) external view returns (
        int256 totalPt,
        int256 totalSy,
        int256 totalLp,
        address treasury,
        int256 scalarRoot,
        uint256 expiry,
        uint256 lnFeeRateRoot,
        uint256 reserveFeePercent,
        uint256 lastLnImpliedRate
    );

    function readTokens() external view returns (
        address _SY,
        address _PT,
        address _YT
    );
}

interface ISYState {
    function exchangeRate() external view returns (uint256);
    function assetInfo() external view returns (
        uint8 assetType,
        address assetAddress,
        uint8 assetDecimals
    );
}

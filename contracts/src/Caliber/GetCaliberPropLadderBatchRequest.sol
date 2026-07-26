// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *      deployment bytecode as payload.
 *
 * @notice Builds a full Caliber ladder snapshot for one pair in a single eth_call:
 *         - getPoolBalances(pairId)
 *         - base batchQuote sampling for both directions
 *         - one midpoint refinement pass for segments whose interpolation error > 1 bps
 */
contract GetCaliberPropLadderBatchRequest {
    uint256 internal constant BASIS_POINTS = 10_000;
    uint256 internal constant MAX_SEGMENT_INTERP_ERROR_BPS = 1;

    struct QuoteRequest {
        bytes32 pairId;
        address tokenIn;
        address tokenOut;
        uint256 amountIn;
    }

    struct QuoteResult {
        uint256 amountOut;
        bool success;
    }

    struct LadderPoint {
        uint256 amountIn;
        uint256 amountOut;
    }

    struct Snapshot {
        uint256 reserveA;
        uint256 reserveB;
        LadderPoint[] ladderAToB;
        LadderPoint[] ladderBToA;
    }

    constructor(
        address caliber,
        bytes32 pairId,
        address tokenA,
        address tokenB,
        bool tokenAIsReserveX
    ) {
        ICaliberPropAMM dex = ICaliberPropAMM(caliber);
        Snapshot memory snapshot = buildSnapshot(
            dex,
            pairId,
            tokenA,
            tokenB,
            tokenAIsReserveX
        );
        bytes memory encoded = abi.encode(snapshot);

        assembly {
            return(add(encoded, 32), mload(encoded))
        }
    }

    function buildSnapshot(
        ICaliberPropAMM dex,
        bytes32 pairId,
        address tokenA,
        address tokenB,
        bool tokenAIsReserveX
    ) internal view returns (Snapshot memory snapshot) {
        (uint256 reserveX, uint256 reserveY) = dex.getPoolBalances(pairId);

        snapshot.reserveA = tokenAIsReserveX ? reserveX : reserveY;
        snapshot.reserveB = tokenAIsReserveX ? reserveY : reserveX;

        uint256[] memory amountsAB = buildSampleGrid(snapshot.reserveA);
        uint256[] memory amountsBA = buildSampleGrid(snapshot.reserveB);
        (snapshot.ladderAToB, snapshot.ladderBToA) = batchQuoteLadders(
            dex,
            pairId,
            tokenA,
            tokenB,
            amountsAB,
            amountsBA
        );

        if (snapshot.ladderAToB.length >= 2) {
            LadderPoint[] memory refinementAB = buildRefinementPoints(
                dex,
                pairId,
                tokenA,
                tokenB,
                snapshot.ladderAToB
            );
            if (refinementAB.length != 0) {
                snapshot.ladderAToB = mergeSortedLadders(snapshot.ladderAToB, refinementAB);
            }
        }

        if (snapshot.ladderBToA.length >= 2) {
            LadderPoint[] memory refinementBA = buildRefinementPoints(
                dex,
                pairId,
                tokenB,
                tokenA,
                snapshot.ladderBToA
            );
            if (refinementBA.length != 0) {
                snapshot.ladderBToA = mergeSortedLadders(snapshot.ladderBToA, refinementBA);
            }
        }
    }

    function buildRefinementPoints(
        ICaliberPropAMM dex,
        bytes32 pairId,
        address tokenIn,
        address tokenOut,
        LadderPoint[] memory baseLadder
    ) internal view returns (LadderPoint[] memory) {
        uint256[] memory midpoints = collectMidpoints(baseLadder);
        if (midpoints.length == 0) {
            return new LadderPoint[](0);
        }

        LadderPoint[] memory midpointLadder = batchQuoteSingleDirection(
            dex,
            pairId,
            tokenIn,
            tokenOut,
            midpoints
        );
        if (midpointLadder.length == 0) {
            return new LadderPoint[](0);
        }

        LadderPoint[] memory selected = new LadderPoint[](midpointLadder.length);
        uint256 count = 0;
        for (uint256 i = 0; i < midpointLadder.length; ++i) {
            uint256 estimated = quoteAmountOut(baseLadder, midpointLadder[i].amountIn);
            if (interpolationErrorAboveThreshold(midpointLadder[i].amountOut, estimated)) {
                selected[count++] = midpointLadder[i];
            }
        }

        return shrinkLadder(selected, count);
    }

    function batchQuoteLadders(
        ICaliberPropAMM dex,
        bytes32 pairId,
        address tokenA,
        address tokenB,
        uint256[] memory amountsAB,
        uint256[] memory amountsBA
    ) internal view returns (LadderPoint[] memory ladderAB, LadderPoint[] memory ladderBA) {
        uint256 total = amountsAB.length + amountsBA.length;
        if (total == 0) {
            return (new LadderPoint[](0), new LadderPoint[](0));
        }

        QuoteRequest[] memory requests = new QuoteRequest[](total);
        uint256 cursor = 0;

        for (uint256 i = 0; i < amountsAB.length; ++i) {
            requests[cursor++] = QuoteRequest({
                pairId: pairId,
                tokenIn: tokenA,
                tokenOut: tokenB,
                amountIn: amountsAB[i]
            });
        }

        for (uint256 i = 0; i < amountsBA.length; ++i) {
            requests[cursor++] = QuoteRequest({
                pairId: pairId,
                tokenIn: tokenB,
                tokenOut: tokenA,
                amountIn: amountsBA[i]
            });
        }

        QuoteResult[] memory results = dex.batchQuote(requests);
        ladderAB = collectSuccessfulQuotes(amountsAB, results, 0);
        ladderBA = collectSuccessfulQuotes(amountsBA, results, amountsAB.length);
    }

    function batchQuoteSingleDirection(
        ICaliberPropAMM dex,
        bytes32 pairId,
        address tokenIn,
        address tokenOut,
        uint256[] memory amounts
    ) internal view returns (LadderPoint[] memory ladder) {
        if (amounts.length == 0) {
            return new LadderPoint[](0);
        }

        QuoteRequest[] memory requests = new QuoteRequest[](amounts.length);
        for (uint256 i = 0; i < amounts.length; ++i) {
            requests[i] = QuoteRequest({
                pairId: pairId,
                tokenIn: tokenIn,
                tokenOut: tokenOut,
                amountIn: amounts[i]
            });
        }

        QuoteResult[] memory results = dex.batchQuote(requests);
        ladder = collectSuccessfulQuotes(amounts, results, 0);
    }

    function collectSuccessfulQuotes(
        uint256[] memory amounts,
        QuoteResult[] memory results,
        uint256 offset
    ) internal pure returns (LadderPoint[] memory ladder) {
        LadderPoint[] memory temp = new LadderPoint[](amounts.length);
        uint256 count = 0;

        for (uint256 i = 0; i < amounts.length; ++i) {
            QuoteResult memory result = results[offset + i];
            if (result.success && result.amountOut != 0) {
                temp[count++] = LadderPoint({amountIn: amounts[i], amountOut: result.amountOut});
            }
        }

        ladder = shrinkLadder(temp, count);
    }

    function buildSampleGrid(uint256 reserve) internal pure returns (uint256[] memory grid) {
        if (reserve == 0) {
            return new uint256[](0);
        }

        uint16[32] memory sampleBps = [
            uint16(1),
            2,
            3,
            5,
            7,
            10,
            15,
            20,
            25,
            30,
            40,
            50,
            75,
            100,
            150,
            200,
            250,
            300,
            400,
            500,
            750,
            1000,
            1500,
            2000,
            3000,
            4000,
            5000,
            6000,
            7000,
            8000,
            9000,
            9900
        ];
        uint256[] memory temp = new uint256[](sampleBps.length);
        uint256 count = 0;
        uint256 last = 0;

        for (uint256 i = 0; i < sampleBps.length; ++i) {
            uint256 amount = (reserve * sampleBps[i]) / BASIS_POINTS;
            if (amount == 0) {
                continue;
            }
            if (last != 0 && amount <= last) {
                continue;
            }
            temp[count++] = amount;
            last = amount;
        }

        grid = new uint256[](count);
        for (uint256 i = 0; i < count; ++i) {
            grid[i] = temp[i];
        }
    }

    function collectMidpoints(LadderPoint[] memory ladder) internal pure returns (uint256[] memory) {
        if (ladder.length < 2) {
            return new uint256[](0);
        }

        uint256[] memory temp = new uint256[](ladder.length - 1);
        uint256 count = 0;

        for (uint256 i = 0; i + 1 < ladder.length; ++i) {
            uint256 lo = ladder[i].amountIn;
            uint256 hi = ladder[i + 1].amountIn;
            if (hi <= lo) {
                continue;
            }

            uint256 mid = lo + ((hi - lo) / 2);
            if (mid > lo && mid < hi) {
                temp[count++] = mid;
            }
        }

        uint256[] memory midpoints = new uint256[](count);
        for (uint256 i = 0; i < count; ++i) {
            midpoints[i] = temp[i];
        }
        return midpoints;
    }

    function quoteAmountOut(LadderPoint[] memory ladder, uint256 amountIn) internal pure returns (uint256) {
        if (ladder.length == 0 || amountIn == 0) {
            return 0;
        }

        uint256 lo = 0;
        uint256 hi = ladder.length;
        while (lo < hi) {
            uint256 mid = lo + (hi - lo) / 2;
            if (ladder[mid].amountIn < amountIn) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }

        if (lo < ladder.length && ladder[lo].amountIn == amountIn) {
            return ladder[lo].amountOut;
        }
        if (lo == 0) {
            return (amountIn * ladder[0].amountOut) / ladder[0].amountIn;
        }
        if (lo >= ladder.length) {
            return 0;
        }

        LadderPoint memory left = ladder[lo - 1];
        LadderPoint memory right = ladder[lo];
        uint256 dx = amountIn - left.amountIn;
        uint256 rangeIn = right.amountIn - left.amountIn;
        uint256 rangeOut = right.amountOut - left.amountOut;
        return left.amountOut + ((dx * rangeOut) / rangeIn);
    }

    function interpolationErrorAboveThreshold(uint256 exact, uint256 estimated) internal pure returns (bool) {
        if (exact == 0) {
            return false;
        }

        uint256 diff = exact > estimated ? exact - estimated : estimated - exact;
        return diff * BASIS_POINTS > exact * MAX_SEGMENT_INTERP_ERROR_BPS;
    }

    function mergeSortedLadders(
        LadderPoint[] memory baseLadder,
        LadderPoint[] memory refinement
    ) internal pure returns (LadderPoint[] memory merged) {
        merged = new LadderPoint[](baseLadder.length + refinement.length);

        uint256 i = 0;
        uint256 j = 0;
        uint256 k = 0;

        while (i < baseLadder.length && j < refinement.length) {
            if (baseLadder[i].amountIn <= refinement[j].amountIn) {
                merged[k++] = baseLadder[i++];
            } else {
                merged[k++] = refinement[j++];
            }
        }

        while (i < baseLadder.length) {
            merged[k++] = baseLadder[i++];
        }

        while (j < refinement.length) {
            merged[k++] = refinement[j++];
        }

        if (k <= 1) {
            return shrinkLadder(merged, k);
        }

        LadderPoint[] memory deduped = new LadderPoint[](k);
        uint256 count = 0;
        for (uint256 idx = 0; idx < k; ++idx) {
            if (count == 0 || deduped[count - 1].amountIn != merged[idx].amountIn) {
                deduped[count++] = merged[idx];
            }
        }

        return shrinkLadder(deduped, count);
    }

    function shrinkLadder(
        LadderPoint[] memory ladder,
        uint256 count
    ) internal pure returns (LadderPoint[] memory shrunk) {
        shrunk = new LadderPoint[](count);
        for (uint256 i = 0; i < count; ++i) {
            shrunk[i] = ladder[i];
        }
    }
}

interface ICaliberPropAMM {
    function getPoolBalances(bytes32 pairId)
        external
        view
        returns (uint256 reserveX, uint256 reserveY);

    function batchQuote(
        GetCaliberPropLadderBatchRequest.QuoteRequest[] memory requests
    ) external view returns (GetCaliberPropLadderBatchRequest.QuoteResult[] memory results);
}

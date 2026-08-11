//SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @dev Not meant to be deployed. Use a static call with the deployment bytecode
 *      as payload (deploy_builder().call_raw()).
 *
 * @notice Fetches a full BinaryFi propAMM state snapshot in a single eth_call:
 *         - getAssets() (when assets not pre-known)
 *         - per-asset decimals + pool balanceOf (ERC20)
 *         - per-asset engine config scale (getAssetConfig)
 *         - engine getAssetReserves() -> (assets[], vaultBalances[])
 *         - engine getFee(recipient) -> (feePpm, 0 if read fails)
 *         - pool.quote() for each requested directed pair (try/catch per pair)
 *
 *         quotePairs are encoded as i * n + j (n = assets.length). Quote
 *         amounts are chosen per direction so every quote stays in the engine
 *         linear region (below the per-asset cap):
 *           - (0 -> j): 10 ** d0 (whole USDT0) -> out = floor(10^(dj+2)/ask)
 *           - (j -> 0): 10 ** (dj-4)           -> out = bid exactly
 *           - (i -> j) cross: 10 ** (di-4)     -> linear anchor only
 *         bigQuotePairs are additional (0 -> j) quotes, encoded as
 *           - n*n + j: 10 ** (d0+4) big amount; combined with the small
 *             (0 -> j) quote they pin the exact ask (unique integer) even for
 *             low-decimal assets where the small quote alone is ambiguous.
 *           - 3*n*n + j: 10 ** (d0+3) mid amount (1000x the small amount);
 *             together with the big quote they detect non-monotonic "ladder
 *             collapse" curves (e.g. NVDAx): when the mid quote is still
 *             linear but the big quote is smaller than the mid output, the
 *             big quote is NOT a valid maxOut for the whole input range and
 *             the Rust side must not cap the linear region with it.
 *         Their results are appended to quotePairs/quotes so the Rust side
 *         can pair them back up.
 *
 *         bigSellPairs are (j -> 0) quotes with 10 ** (dj + 2) (100 whole
 *         units), encoded as 2 * n * n + j. The engine caps the input of a
 *         sell at maxIn = ladderWeight * reserve; a 100-unit quote saturates
 *         whenever maxIn < 100 units, so the Rust side can recover the exact
 *         maxIn (= cappedOut * 10^dj / (bid * 10^(d0-2))) and reproduce the
 *         full-range SELL formula bit-exactly.
 */
contract GetBinaryFiPropStateBatchRequest {
    struct QuoteResult {
        uint256 amountOut;
        bool success;
    }

    struct Snapshot {
        address[] assets;
        uint8[] decimals;
        uint256[] scales;
        uint256[] poolBalances;
        uint256[] vaultReserves;
        uint256[] quotePairs;
        QuoteResult[] quotes;
        uint256 fee;
    }

    constructor(
        address pool,
        address engine,
        address recipient,
        address[] memory assets,
        uint256[] memory quotePairs,
        uint256[] memory bigQuotePairs,
        uint256[] memory bigSellPairs
    ) {
        if (assets.length == 0) {
            assets = IBinaryFiPropPool(pool).getAssets();
        }
        uint256 n = assets.length;

        Snapshot memory snap;
        snap.assets = assets;
        snap.decimals = new uint8[](n);
        snap.scales = new uint256[](n);
        snap.poolBalances = new uint256[](n);

        for (uint256 i = 0; i < n; ++i) {
            (bool decOk, bytes memory decData) = assets[i].call{gas: 20000}(
                abi.encodeWithSignature("decimals()")
            );
            if (decOk && decData.length == 32) {
                uint256 d = abi.decode(decData, (uint256));
                if (d != 0 && d <= 30) snap.decimals[i] = uint8(d);
            }

            (bool balOk, bytes memory balData) = assets[i].call{gas: 20000}(
                abi.encodeWithSignature("balanceOf(address)", pool)
            );
            if (balOk && balData.length == 32) {
                snap.poolBalances[i] = abi.decode(balData, (uint256));
            }
        }

        if (engine != address(0)) {
            for (uint256 i = 0; i < n; ++i) {
                try IBinaryFiEngine(engine).getAssetConfig(uint8(i)) returns (
                    address,
                    uint8,
                    uint256 scale,
                    uint256
                ) {
                    snap.scales[i] = scale;
                } catch {
                    snap.scales[i] = 0;
                }
            }
            try IBinaryFiEngine(engine).getAssetReserves() returns (
                address[] memory,
                uint256[] memory reserves
            ) {
                snap.vaultReserves = reserves;
            } catch {
                snap.vaultReserves = new uint256[](0);
            }
            // 按账户费率（ppm）：recipient = 报价账户（本系统 router）。费率是
            // 引擎 per-account storage，非 0 时 Rust 侧用其做报价输入侧扣费；
            // 读取失败保持 0 → Rust 侧沿用本地已知费率。
            try IBinaryFiEngine(engine).getFee(recipient) returns (uint256 feePpm) {
                snap.fee = feePpm;
            } catch {
                snap.fee = 0;
            }
        }

        uint256 total = quotePairs.length + bigQuotePairs.length + bigSellPairs.length;
        snap.quotePairs = new uint256[](total);
        snap.quotes = new QuoteResult[](total);
        for (uint256 k = 0; k < quotePairs.length; ++k) {
            if (n == 0) break;
            uint256 pair = quotePairs[k];
            uint256 i = pair / n;
            uint256 j = pair % n;
            if (i >= n || j >= n || i == j) continue;
            if (snap.decimals[i] == 0) continue;
            uint256 amountIn;
            if (i == 0) {
                // 0 -> j：整枚 USDT0（线性区，out = floor(10^(dj+2)/ask)）
                amountIn = 10 ** uint256(snap.decimals[0]);
            } else {
                // j -> 0 与跨资产：10^(di-4)（out = bid 精确值 / 线性锚点）
                amountIn = 10 ** uint256(snap.decimals[i] - 4);
            }
            try IBinaryFiPropPool(pool).quote(recipient, assets[i], assets[j], amountIn)
                returns (uint256 amountOut)
            {
                snap.quotes[k] = QuoteResult(amountOut, true);
            } catch {
                snap.quotes[k] = QuoteResult(0, false);
            }
            snap.quotePairs[k] = pair;
        }
        // 追加 (0 -> j) 大额/中额报价：10^(d0+4) 锁定 ask + 恢复 BUY maxOut；
        // 10^(d0+3) 中额用于检测非单调阶梯退化（mid 线性但 big 输出 < mid
        // 输出 → big 不是有效 maxOut，Rust 侧不得用它截断线性区）。
        // bigQuotePairs 编码为 n*n + j / 3*n*n + j（区别于普通 pair 的 i*n+j）
        for (uint256 k = 0; k < bigQuotePairs.length; ++k) {
            uint256 idx = quotePairs.length + k;
            if (n == 0) break;
            uint256 pair = bigQuotePairs[k];
            uint256 amountIn;
            uint256 j;
            if (pair < n * n) continue;
            if (pair < 2 * n * n) {
                j = pair - n * n;
                amountIn = 10 ** (uint256(snap.decimals[0]) + 4);
            } else if (pair >= 3 * n * n && pair < 4 * n * n) {
                j = pair - 3 * n * n;
                amountIn = 10 ** (uint256(snap.decimals[0]) + 3);
            } else {
                continue;
            }
            if (j >= n || j == 0) continue;
            if (snap.decimals[0] == 0) continue;
            try IBinaryFiPropPool(pool).quote(recipient, assets[0], assets[j], amountIn)
                returns (uint256 amountOut)
            {
                snap.quotes[idx] = QuoteResult(amountOut, true);
            } catch {
                snap.quotes[idx] = QuoteResult(0, false);
            }
            snap.quotePairs[idx] = pair;
        }
        // 追加 (j -> 0) 超大额报价：10^(dj+2)（100 整枚），恢复 SELL 侧 maxIn。
        // bigSellPairs 编码为 2*n*n + j
        for (uint256 k = 0; k < bigSellPairs.length; ++k) {
            uint256 idx = quotePairs.length + bigQuotePairs.length + k;
            if (n == 0) break;
            uint256 pair = bigSellPairs[k];
            if (pair < 2 * n * n || pair >= 3 * n * n) continue;
            uint256 j = pair - 2 * n * n;
            if (j >= n || j == 0) continue;
            if (snap.decimals[j] == 0) continue;
            uint256 amountIn = 10 ** (uint256(snap.decimals[j]) + 2);
            try IBinaryFiPropPool(pool).quote(recipient, assets[j], assets[0], amountIn)
                returns (uint256 amountOut)
            {
                snap.quotes[idx] = QuoteResult(amountOut, true);
            } catch {
                snap.quotes[idx] = QuoteResult(0, false);
            }
            snap.quotePairs[idx] = pair;
        }

        bytes memory encoded = abi.encode(snap);
        assembly {
            return(add(encoded, 32), mload(encoded))
        }
    }
}

interface IBinaryFiPropPool {
    function getAssets() external view returns (address[] memory);

    function quote(
        address recipient,
        address tokenIn,
        address tokenOut,
        uint256 amountIn
    ) external view returns (uint256 amountOut);
}

interface IBinaryFiEngine {
    function getAssetReserves()
        external
        view
        returns (address[] memory assets, uint256[] memory reserves);

    function getAssetConfig(uint8 assetId)
        external
        view
        returns (address asset, uint8 decimals, uint256 scale, uint256 cap);

    /// @notice 查询账户费率（ppm，1e6 = 100%）。
    ///         聚合器白名单账户返回 0；普通账户返回当前 setFee 值。
    function getFee(address account) external view returns (uint256 feePpm);
}

//! Fermi propAMM 类型定义：部署地址、合约 ABI、PairParams/lane 结构、事件签名。
//!
//! 链上事实与数据结构说明见 `docs/fermi_prop_internal.md`（长期维护必读）。

use alloy::{
    primitives::{address, b256, keccak256, Address, B256, U256},
    sol,
    sol_types::SolValue,
};
use serde::{Deserialize, Serialize};

// ============================================================================
// 部署地址（Ethereum 主网，2026-08-23 实测）
// ============================================================================

/// Fermi 链 ID（Ethereum 主网）。
pub const FERMI_CHAIN_ID: u64 = 1;
/// engine：pair 管理、quote/swap 核心、读 registry lane。
pub const FERMI_ENGINE_ADDRESS: Address = address!("0x90f73fEA1Ee2Dc514d4dbAc0bfF7ff04b933767f");
/// swapper：执行层（quoteAmounts / fermiSwapWithCallback / getPairsWithStatus）。
pub const FERMI_SWAPPER_ADDRESS: Address = address!("0xb1076fE3AB5e28005C7c323Bac5AC06a680d452e");
/// IPropAMM wrapper：标准 `quote/swap/quoteExactOut/swapExactOut` 入口（Swapped 事件源）。
pub const FERMI_WRAPPER_ADDRESS: Address = address!("0x5979458912F80B96d30D4220af8E2e4925A33320");
/// registry（PrioUpdateRegistry）：lane 存储、EIP-712 签名更新。
pub const FERMI_REGISTRY_ADDRESS: Address = address!("0xDA7AFeEd01fe625cF15D187A19F94B45F00b8C5f");
/// trader vault：全部 pair 共享流动性金库（Safe）。
pub const FERMI_VAULT_ADDRESS: Address = address!("0x585d44727129B9C69791B10238Ca605932938B4F");

/// 计算 engine"最后成交"记录槽（2026-08-25 漂移测试 trace 级实证）。
///
/// 推导（trace 反汇编）：`keccak256(abi.encode(sub_key, keccak256(abi.encode(laneIndex, 7))))`
/// —— engine 存储槽 7 的 `mapping(bytes32 laneIndex => mapping(uint256 => uint256))`。
/// - `sub_key = 0`：正向（token_a→token_b）路径读取，`div1 += last_trade_x`；
/// - `sub_key = 1`：反向（token_b→token_a）路径读取，`a_norm = (A + last_trade_x)*1e18/D`。
/// WETH/USDC sub0 实测 = `0x8d04200e22c0039c0cc745ec44387ca133910b6ef876b574d984f629463d0dd5`，
/// WETH/USDT sub0 实测 = `0x62aec58dde2dfeae0f2f8adf5d9fb00959214dd647a4daa2d5584b28fb579366`，
/// WETH/USDT sub1 实测 = `0xcc22ff99ccfed6a63d6962d27c00523701e6b6585d0c79193be792b6af970b06`，均与推导一致。
///
/// 值布局：`(last_trade_x << 64) | last_trade_block`（low 32 位 = 成交区块号）。
/// 仅当 `last_trade_block == 当前同步块`（成交发生当块）时校正生效；
/// 非成交块该字段被忽略。漂移对拍实证见 `docs/fermi_prop_internal.md` §7。
pub fn fermi_engine_last_trade_slot(token_a: Address, token_b: Address, sub_key: u64) -> B256 {
    let lane = fermi_lane_index(token_a, token_b);
    let mut inner_input = [0u8; 64];
    inner_input[..32].copy_from_slice(lane.as_ref());
    inner_input[63] = 7; // 存储槽 7 的嵌套 mapping
    let inner = keccak256(inner_input);

    let mut input = [0u8; 64];
    // abi.encode(uint256(sub_key), inner)：外层 key = sub_key（0=正向 / 1=反向）。
    input[24..32].copy_from_slice(&sub_key.to_be_bytes());
    input[32..].copy_from_slice(inner.as_ref());
    keccak256(input)
}

// ============================================================================
// 事件签名（keccak256 实测）
// ============================================================================

/// engine `PairRegistered(address indexed, address indexed)`
pub const FERMI_PAIR_REGISTERED_EVENT: B256 =
    b256!("04a8c4a40701c933bc98762acf207e7bb0cc55872b52944ea3f679bf9d41de81");
/// engine `PairUnregistered(address indexed, address indexed)`
pub const FERMI_PAIR_UNREGISTERED_EVENT: B256 =
    b256!("c76e5bce9803daa26e5f8b050cee416f8c094680f99b2c663a2ea44f0de5d546");
/// engine `PairActiveSet(address indexed, address indexed, bool active)`
pub const FERMI_PAIR_ACTIVE_SET_EVENT: B256 =
    b256!("c098775b03191ddef27a0b0b986b185483df93ec65834cfd5cefab841dc556a9");
/// wrapper `Swapped(address indexed sender, address indexed tokenIn, address indexed tokenOut, uint256 amountIn, uint256 amountOut, address recipient)`
pub const FERMI_SWAPPED_EVENT: B256 =
    b256!("1eeaa4acf3c225a4033105c2647625dbb298dec93b14e16253c4231e26c02b1d");
/// ERC20 `Transfer(address indexed, address indexed, uint256)`（vault 余额对账）
pub const ERC20_TRANSFER_EVENT: B256 =
    b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");

// ============================================================================
// 合约 ABI（alloy sol! 生成，供 init/quote/事件解码使用）
// ============================================================================

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IFermiEngine {
        struct TokenPair {
            address token0;
            address token1;
            bool active;
        }

        struct CurveSegment {
            int128 x;
            int128 y;
            int128 a;
            int128 b;
            int128 c;
            int128 d;
        }

        struct PairParams {
            uint128 a;
            uint16 b;
            uint128 c;
            uint128 d;
            CurveSegment[] c1;
            CurveSegment[] c2;
        }

        function getPairs() external view returns (TokenPair[] memory pairs);

        function getPairParams(address baseAsset, address quoteAsset)
            external
            view
            returns (PairParams memory p);

        function isActive(address baseAsset, address quoteAsset) external view returns (bool active);

        function traderVault() external view returns (address vault);

        function prioRegistry() external view returns (address registry);

        function unlocked(address tokenIn, address tokenOut) external view returns (bool unlocked_);

        function quote(
            address tokenIn,
            address tokenOut,
            int256 amountSpecified,
            address sender
        ) external view returns (uint256 amountIn, uint256 amountOut);

        event PairRegistered(address indexed baseAsset, address indexed quoteAsset);

        event PairUnregistered(address indexed baseAsset, address indexed quoteAsset);

        event PairActiveSet(address indexed baseAsset, address indexed quoteAsset, bool active);
    }
}

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IFermiSwapper {
        function quoteAmounts(address tokenIn, address tokenOut, int256 amountSpecified)
            external
            view
            returns (uint256 amountIn, uint256 amountOut);

        function getPairsWithStatus() external view returns (address[] memory, bool[] memory);

        function fermi() external view returns (address);
    }
}

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IFermiRegistry {
        function getState(uint256 laneIndex, uint32 minTimestamp, uint32 maxTimestamp)
            external
            view
            returns (uint32 updateTimestamp, uint8 flag, uint256 fairPriceE8);

        function updateState(
            address target,
            uint256 laneIndex,
            uint32 updateTimestamp,
            uint256[] slots
        ) external;

        struct Update {
            address target;
            address signer;
            uint256 laneIndex;
            uint32 updateTimestamp;
            uint256[] slots;
            bytes signature;
        }

        function batchUpdateStateWithSignature(Update[] updates) external;

        function isUpdater(address target, address updater) external view returns (bool);

        function MAX_UPDATE_AGE() external view returns (uint256);

        function MAX_UPDATE_LEAD_TIME() external view returns (uint256);

        event UpdaterAdded(address indexed target, address indexed updater);

        event UpdaterRemoved(address indexed target, address indexed updater);
    }
}

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IFermiWrapper {
        function quote(address tokenIn, address tokenOut, uint256 amountIn)
            external
            view
            returns (uint256 amountOut);

        function quoteExactOut(address tokenIn, address tokenOut, uint256 amountOut)
            external
            view
            returns (uint256 amountIn);

        function isActive(address tokenIn, address tokenOut) external view returns (bool active);

        function getPairs() external view returns (address[] memory token0, address[] memory token1);

        event Swapped(
            address indexed sender,
            address indexed tokenIn,
            address indexed tokenOut,
            uint256 amountIn,
            uint256 amountOut,
            address recipient
        );
    }
}

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IFermiERC20 {
        function balanceOf(address owner) external view returns (uint256 balance);

        function decimals() external view returns (uint8 decimals_);
    }
}

// ============================================================================
// 状态结构
// ============================================================================

/// PairParams 曲线段（engine `getPairParams` 返回的 c1/c2 元素）。
///
/// 每段为 (x1, x2, a, b, c, d)：x 区间 [x1, x2]（tokenIn 数量），段内输出
/// 曲线由系数 a/b/c/d 决定。c1 为正区间（买入方向），c2 为负区间（卖出方向，
/// x/y 可为负）。精确 quote 数学在 M3 逆向（见文档 §7）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FermiCurveSegment {
    pub x: i128,
    pub y: i128,
    pub a: i128,
    pub b: i128,
    pub c: i128,
    pub d: i128,
}

/// Pair 曲线参数（engine `getPairParams(baseAsset, quoteAsset)`）。
///
/// - `a`：交易对相关规模参数（WETH/WBTC 系 = 5e17，稳定币对 = 0）
/// - `b`：费率/档位参数（WETH 系 = 5000，WBTC 系 = 3333）
/// - `c`/`d`：方向深度参数（同 pair 通常相等，WETH 系 = 3e12）
/// - `c1`：正区间分段曲线（8-10 段）
/// - `c2`：负区间分段曲线（4 段）
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FermiPairParams {
    pub a: u128,
    pub b: u16,
    pub c: u128,
    pub d: u128,
    pub c1: Vec<FermiCurveSegment>,
    pub c2: Vec<FermiCurveSegment>,
}

impl FermiPairParams {
    /// 从 alloy sol! 生成的 `IFermiEngine::PairParams` 转换。
    pub fn from_sol(p: IFermiEngine::PairParams) -> Self {
        Self {
            a: p.a,
            b: p.b,
            c: p.c,
            d: p.d,
            c1: p.c1.into_iter().map(FermiCurveSegment::from_sol).collect(),
            c2: p.c2.into_iter().map(FermiCurveSegment::from_sol).collect(),
        }
    }
}

impl FermiCurveSegment {
    /// getPairParams ABI 实证（2026-08-24 生产对拍 @block 0x18a0d7b + 活链复核）：
    /// 真实字段序 = `(x=下界, y=上界, a, b, c=斜率, d=截距)`；而本地曲线数学
    /// （engine_quote / c2_delta）按 `(x=上界, y=下界, c=截距, d=斜率)` 解释
    /// （与已验证夹具一致）。故此处交换 x↔y、c↔d 转换为内部序。
    fn from_sol(s: IFermiEngine::CurveSegment) -> Self {
        Self {
            x: s.y,
            y: s.x,
            a: s.a,
            b: s.b,
            c: s.d,
            d: s.c,
        }
    }
}

/// Registry lane 状态（单条报价）。
///
/// 存储槽位编码（实测，32 字节）：
/// - 高 32 位：`updateTimestamp`（uint32）
/// - 第 5 字节：`flag`（0x01 = 有效/活跃）
/// - 低 20 字节：`fairPriceE8`（价格 × 1e8 定点）
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FermiLane {
    pub update_timestamp: u32,
    pub flag: u8,
    pub fair_price_e8: u64,
}

impl FermiLane {
    /// 从 registry storage 槽位值解码 lane。
    pub fn from_slot_word(value: U256) -> Option<Self> {
        // 高 32 位 = updateTimestamp
        let update_timestamp = (value >> U256::from(224)).to::<u32>();
        // 第 5 字节（bits 216..224）= flag
        let flag = ((value >> U256::from(216)) & U256::from(0xff)).to::<u8>();
        // 低 20 字节 = fairPriceE8
        let mask = (U256::from(1) << U256::from(160)) - U256::from(1);
        let fair_price_e8 = (value & mask).to::<u64>();
        Some(Self {
            update_timestamp,
            flag,
            fair_price_e8,
        })
    }
}

// ============================================================================
// lane 索引与虚拟地址
// ============================================================================

/// 计算引擎全局输出上限槽位：`keccak256(abi.encode(token, uint256(8)))`。
///
/// engine 的 `maxOutput` 为 mapping(address => uint256) 存储于槽位 8，按 **base 资产**
/// （正向报价的 tokenIn）索引。trace 实测：`maxOutput[WETH] = 1.8e21`
/// （slot `0x5cc08d...cb26` @block 25817758）。IL 检查：
/// `vault(tokenIn) + amountIn > maxOutput[tokenIn]` → revert `IL`。
pub fn fermi_max_output_slot(token: Address) -> B256 {
    let mut input = [0u8; 64];
    input[12..32].copy_from_slice(token.as_ref());
    input[32..].copy_from_slice(&U256::from(8).to_be_bytes::<32>());
    keccak256(input)
}

/// 计算 registry lane 索引：`keccak256(abi.encode(tokenA, tokenB))`。
/// 地址按 ABI 编码左填充为 32 字节（与链上 registry/engine 一致）。
pub fn fermi_lane_index(token_a: Address, token_b: Address) -> B256 {
    let mut input = [0u8; 64];
    input[12..32].copy_from_slice(token_a.as_ref());
    input[44..64].copy_from_slice(token_b.as_ref());
    keccak256(input)
}

/// 计算 registry lane 存储槽位：`keccak256(abi.encode(engine, lane_index))`。
///
/// registry（PrioUpdateRegistry）的 lane 状态槽 = 嵌套映射
/// `mapping(caller => mapping(bytes32 laneIndex => uint256))`：
/// 外层 key = 调用方（engine 地址；engine STATICCALL getState 时 msg.sender = engine），
/// 内层 key = laneIndex（`fermi_lane_index`）。
///
/// 推导实证（2026-08-24，trace 级）：
/// - engine.quote trace 中 registry 帧 KECCAK256 前内存：`MSTORE 0xa0 = engine`、
///   `MSTORE 0xc0 = laneIndex`，随后 `KECCAK256(0xa0, 0x40)` → 结果直接 SLOAD；
/// - 8/8 pair 计算槽位与 Titan stateDiff 绝对槽位逐一命中；
/// - 活链 `eth_getStorageAt(registry, slot)` 返回合法打包 lane（比快照更新）。
pub fn fermi_registry_lane_slot(engine: Address, token_a: Address, token_b: Address) -> B256 {
    let lane = fermi_lane_index(token_a, token_b);
    let mut input = [0u8; 64];
    input[12..32].copy_from_slice(engine.as_ref());
    input[32..].copy_from_slice(lane.as_ref());
    keccak256(input)
}

/// 生成 Fermi 虚拟子池地址（StateSpace key）。
///
/// 同一 engine 部署下的每个 (tokenA, tokenB) 有序对映射一个确定地址；
/// 与 BinaryFi 同款命名空间哈希，避免与真实合约地址冲突。
pub fn fermi_virtual_address(engine: Address, token_a: Address, token_b: Address) -> Address {
    let digest = keccak256(("FermiProp", engine, token_a, token_b).abi_encode());
    Address::from_word(digest)
}

/// 有序 token 对（token0 < token1），与 engine `getPairs` 的排序一致。
pub fn sorted_tokens(token_a: Address, token_b: Address) -> (Address, Address) {
    if token_a < token_b {
        (token_a, token_b)
    } else {
        (token_b, token_a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_index_matches_abi_encode() {
        let a = address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let b = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let idx = fermi_lane_index(a, b);
        // 与 keccak256(abi.encode(a, b)) 一致（独立验证）
        let expected = keccak256((a, b).abi_encode());
        assert_eq!(idx, expected);
        // 长度 32 字节
        assert_eq!(idx.len(), 32);
    }

    #[test]
    fn registry_lane_slot_matches_titan_state_diff() {
        // 2026-08-24 实证：slot = keccak256(abi.encode(engine, laneIndex))。
        // 8/8 pair 与 Titan stateDiff 绝对槽位逐一命中（快照 updateTs=0x6a8ae2b7）。
        let engine = FERMI_ENGINE_ADDRESS;
        let weth = address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let usdc = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let usdt = address!("0xdac17f958d2ee523a2206206994597c13d831ec7");
        let wbtc = address!("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599");
        let cbbtc = address!("0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf");
        let cases: [(Address, Address, &str, u64); 8] = [
            (
                weth,
                usdc,
                "b4db643bb85d3a166ca4d268ff74cb46c29d8efa8fbd86a0d33a0008e5424107",
                242374096470,
            ),
            (
                weth,
                usdt,
                "f6cc8e449bd54b7f3e97f1636eb46750c8a80596e18c002bf431b0b0b848535b",
                242388399276,
            ),
            (
                wbtc,
                usdc,
                "ba06a6ffe29f9d34ffa458c44f99292a168a2248cdf9afd44c7f12b7c26b4baa",
                7710548264404,
            ),
            (
                wbtc,
                usdt,
                "65274715c53b990e51f501502c5d636a8e93fa646305fadb684e766f8f690c69",
                7710737823654,
            ),
            (
                usdc,
                usdt,
                "461a5afe3bfdbe196241a0acc4cdb6da2977a9f81e8c76a449ea5c07d1705329",
                100006465,
            ),
            (
                cbbtc,
                usdc,
                "1ba5a5b4f3238a22bdbfc2cb8c8da3b2407a3cb61bad29d706607de95bf0b58a",
                7714610315319,
            ),
            (
                cbbtc,
                usdt,
                "8292396c1e0f041bd814e37e5db20494b921288dbefb18448cc2909c389c97c7",
                7714799974433,
            ),
            (
                wbtc,
                cbbtc,
                "ec4d70f32c578ad390473e49057410f4642260fbaa02890c22a6aa638db3d23a",
                99947346,
            ),
        ];
        for (a, b, slot_hex, price_e8) in cases {
            let slot = fermi_registry_lane_slot(engine, a, b);
            assert_eq!(
                slot.to_string(),
                format!("0x{slot_hex}"),
                "slot mismatch for pair"
            );
            // 打包值 = (updateTimestamp << 224) | (flag << 216) | fairPriceE8
            let word = (U256::from(0x6a8ae2b7u64) << U256::from(224))
                | (U256::from(0x01u64) << U256::from(216))
                | U256::from(price_e8);
            let lane = FermiLane::from_slot_word(word).unwrap();
            assert_eq!(lane.fair_price_e8, price_e8);
            assert_eq!(lane.update_timestamp, 0x6a8ae2b7);
            assert_eq!(lane.flag, 0x01);
        }
    }

    #[test]
    fn lane_decodes_real_slot_word() {
        // 实测 registry stateDiff（2026-08-23 Titan 快照）：
        // 0x6a8a7d77 01 0000... 06f8628fe3a8
        let word = U256::from_str_radix(
            "6a8a7d770100000000000000000000000000000000000000000006f8628fe3a8",
            16,
        )
        .unwrap();
        let lane = FermiLane::from_slot_word(word).unwrap();
        assert_eq!(lane.update_timestamp, 0x6a8a7d77);
        assert_eq!(lane.flag, 0x01);
        assert_eq!(lane.fair_price_e8, 0x6f8628fe3a8);
    }

    #[test]
    fn sorted_tokens_orders() {
        let usdc = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let weth = address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        assert!(weth > usdc);
        let (t0, t1) = sorted_tokens(weth, usdc);
        assert_eq!(t0, usdc);
        assert_eq!(t1, weth);
        let (t0b, t1b) = sorted_tokens(usdc, weth);
        assert_eq!((t0b, t1b), (t0, t1));
    }

    #[test]
    fn virtual_address_is_deterministic() {
        let engine = FERMI_ENGINE_ADDRESS;
        let a = address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let b = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        assert_eq!(
            fermi_virtual_address(engine, a, b),
            fermi_virtual_address(engine, a, b)
        );
        assert_ne!(
            fermi_virtual_address(engine, a, b),
            fermi_virtual_address(engine, b, a)
        );
    }
}

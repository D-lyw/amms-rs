//! ElfomoFi propAMM 类型定义：合约 ABI 与 orderbook 档位结构。

use alloy::primitives::U256;
use alloy::sol;
use serde::{Deserialize, Serialize};

// ============================================================================
// Contract ABI（XLayer 实测：Router / Factory / Pool）
// ============================================================================

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IElfomoFiRouter {
        function getSupportedPairs() external view returns (address[2][] memory pairs);

        function getAmountOut(
            address fromToken,
            address toToken,
            uint256 fromAmount
        ) external view returns (uint256 toAmount);

        function getAmountIn(
            address fromToken,
            address toToken,
            uint256 toAmount
        ) external view returns (uint256 fromAmount);

        function swap(
            address fromToken,
            address toToken,
            int256 specifiedAmount,
            uint256 limitAmount,
            address receiver,
            uint256 partnerId
        ) external returns (uint256);
    }
}

sol! {
    #[allow(missing_docs)]
    #[derive(Debug, PartialEq, Eq)]
    struct ElfomoOrderbookLevel {
        uint256 size;
        uint256 price;
    }

    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IElfomoFiFactory {
        /// 返回两个 (size, price) 数组：
        /// - `fromToLevels`：fromToken→toToken 方向（size = 输入量）
        /// - `toFromLevels`：toToken→fromToken 方向（size = 输出量）
        function getOrderbook(
            address fromToken,
            address toToken
        )
            external
            view
            returns (
                ElfomoOrderbookLevel[] memory fromToLevels,
                ElfomoOrderbookLevel[] memory toFromLevels
            );
    }
}

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IElfomoFiPool {
        /// 池内 per-asset 做市参数（11 字段打包成一条 storage word，**每池每资产各一份**）。
        ///
        /// 本地 ladder **在 `init` 阶段逐池从这里读取**，不做任何 pair 特判：
        /// `unit`(U) / `band_count`(T) / `spread_level` / `spread_penalty` 唯一决定该池
        /// 两侧的全部档位宽度与斜率（生成规则见 [`ElfomoLadderConfig`]）。其余字段当前
        /// 不参与报价（保留以便审计与未来扩展）。
        ///
        /// 底层存储：`storage[keccak256(pad32(asset)‖pad32(0x04))]`。未配置的 asset
        /// 返回全零（USDT0 侧即如此，由 vault 余额背书）。
        function getMetadata(address asset)
            external
            view
            returns (
                uint8 field0,
                uint8 decimals,
                uint8 field2,
                uint8 field3,
                uint16 field4,
                uint128 unit,
                uint16 band_count,
                uint16 spread_level,
                uint8 spread_penalty,
                address field9,
                uint256 field10
            );

        /// 本池支持的 pair 列表（`address[2][]`，`[base, quote]` 顺序）。
        ///
        /// **profile 存储键就是从它派生的**：池内逐档 profile word 存在
        /// `mapping(uint256 => uint256)`（slot 0），key = 该 pair 在本列表中的下标。
        /// keeper 的 `0xd4ff31bd(uint256[] keys, uint256[] words)` 用的也是同一组下标，
        /// 因此代码里不需要任何"pair 0 = xETH/USDT0"之类的特判。
        function getSupportedPairs() external view returns (address[2][] memory pairs);
    }
}

// ============================================================================
// 数据结构
// ============================================================================

/// 单档 orderbook 档位（size = 档位容量，price = 1e24 定点价格）。
///
/// 语义（链上逐位对拍锁定，见 docs/2026-09-01_elfomo_prop_xlayer_research.md §3.1）：
/// - `from→to`（arr0）：size = 该档最大**输入**量，
///   输出 = `floor(take × price / 1e24)`，`take = min(剩余输入, size)`。
/// - `to→from`（arr1）：size = 该档最大**输出**量，
///   所需输入 = `ceil(size × price / 1e24)`。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrderbookLevel {
    pub size: U256,
    pub price: U256,
}

impl OrderbookLevel {
    pub const fn new(size: U256, price: U256) -> Self {
        Self { size, price }
    }
}

/// 订单簿快照：两侧档位 + 金库余额背书 + 价格种子 + 逐档 profile word。
///
/// `price_seed` 是 Pool slot1 高 32 位（`a`）、`profile_word` 是逐档
/// 宽度/偏离表 word（见 [`ElfomoBandProfile`]），orderbook 是
/// `(profile, price_seed, vault_usdt0, vault_xeth)` 的**读时纯函数**（见
/// `ElfomoFiPropPool::build_orderbook_with`）。档位字段仅为缓存/对拍，
/// 本地报价一律按这份状态实时重算，保证与链上一致。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrderbookSnapshot {
    /// fromToken→toToken 方向档位（size = 输入量）
    pub from_to_levels: Vec<OrderbookLevel>,
    /// toToken→fromToken 方向档位（size = 输出量）
    pub to_from_levels: Vec<OrderbookLevel>,
    /// 金库 USDT0 余额（正向输出封顶）
    pub vault_usdt0: U256,
    /// 金库 xETH 余额（反向输出封顶；s1+s2+s3 == 此值）
    pub vault_xeth: U256,
    /// 价格种子 `a`（Pool slot1 >> 32；updatePrices calldata 直接携带）
    #[serde(default)]
    pub price_seed: U256,
    /// 逐档宽度/偏离表 word（池存储 `keccak256(pad32(key)‖pad32(0))`；
    /// `0xd4ff31bd` calldata 直接携带；`0` = 尚未读取）
    #[serde(default)]
    pub profile_word: U256,
}

// ============================================================================
// 每 pool 的 ladder 参数（**init 时逐池从链上 `getMetadata` 读取**）
// ============================================================================

/// 池内逐档「宽度表 + 偏离表」——**池子存储里的动态数据，不是协议常量**。
///
/// ## 为什么不能写死
///
/// 2026-09-11 链上实证（XLayer 块 `70352699 → 70352700`）：keeper 发了一笔
/// `0xd4ff31bd(uint256[] keys, uint256[] words)` 改写这条 word，偏离表从
/// `[7,10,15,25,40,50]` 变成 `[15,20,25,35,45,60]`。**逆向阶段观测到的只是
/// 那一刻的快照**——写死常量会在 keeper 改表后静默算错价（本地报价偏离链上，
/// 产生幻影机会）。因此本表与 `unit/band_count/spread_level` 一样，必须在
/// `init` 时从链上读取，并由 raw-tx（`0xd4ff31bd` calldata）+ 周期快照持续同步。
///
/// ## 存储与打包布局（分块实测解码 + 逐字段置位验证）
///
/// 位置：`mapping(uint256 => uint256)` 在 slot 0，key = 本池基础资产档位号
/// （XLayer 单 pair = `0`）→ `slot = keccak256(pad32(key)‖pad32(0))`，
/// 即 `0xad3228b6…5fb5`。一条 word 打包 `n = byte31` 档：
///
/// ```text
/// byte31            = n（档数，本例 6）
/// byte30            = widths[0]
/// deviations[k]     = u16 little-endian @ byte (28 − 4k)   k = 0..n-1
/// widths[k+1]       = u16 little-endian @ byte (26 − 4k)   k = 0..n-2
/// ```
///
/// 实测样本（块 `70352699`）：`0x…00320064002800140019000a000f000a000a00050007000106`
/// → `n=6, widths=[1,5,10,10,20,100], deviations=[7,10,15,25,40,50]`；
/// 块 `70352700`（keeper 改写后）：`0x…003c0064002d00140023000a0019000a00140005000f000106`
/// → `deviations=[15,20,25,35,45,60]`，宽度表不变。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ElfomoBandProfile {
    /// 档数 `n`（word byte31；`0` = 未配置）。
    #[serde(default)]
    pub count: u8,
    /// 逐档宽度，单位 `U`（word byte30 + byte(26−4k) 的 u16 LE）。
    #[serde(default)]
    pub widths: [u64; ELFOMO_MAX_BANDS],
    /// 逐档偏离量（单位 1e-5；word byte(28−4k) 的 u16 LE）。
    #[serde(default)]
    pub deviations: [u64; ELFOMO_MAX_BANDS],
}

/// 单条 profile word 支持的最大档数（打包布局按 6 档逆向）。
pub const ELFOMO_MAX_BANDS: usize = 6;

impl ElfomoBandProfile {
    /// 从链上 profile word 解码（布局见类型文档）。
    ///
    /// `None` = 该 word 不是合法的 profile（档数为 0 / 超 6 档）。
    pub fn decode_word(word: U256) -> Option<Self> {
        let bytes = word.to_be_bytes::<32>();
        let count = bytes[31] as usize;
        if count == 0 || count > ELFOMO_MAX_BANDS {
            return None;
        }
        let u16_le = |off: usize| -> u64 { (bytes[off] as u64) | ((bytes[off + 1] as u64) << 8) };
        let mut widths = [0u64; ELFOMO_MAX_BANDS];
        let mut deviations = [0u64; ELFOMO_MAX_BANDS];
        for k in 0..count {
            deviations[k] = u16_le(28 - 4 * k);
            widths[k] = if k == 0 {
                bytes[30] as u64
            } else {
                u16_le(30 - 4 * k)
            };
        }
        Some(Self {
            count: count as u8,
            widths,
            deviations,
        })
    }

    /// 编码回 word（测试/诊断用，与 [`Self::decode_word`] 互逆）。
    pub fn encode_word(&self) -> U256 {
        let mut bytes = [0u8; 32];
        let n = (self.count as usize).min(ELFOMO_MAX_BANDS);
        bytes[31] = self.count;
        for k in 0..n {
            let dev = self.deviations[k];
            bytes[28 - 4 * k] = (dev & 0xff) as u8;
            bytes[29 - 4 * k] = ((dev >> 8) & 0xff) as u8;
            if k == 0 {
                bytes[30] = (self.widths[0] & 0xff) as u8;
            } else {
                let w = self.widths[k];
                bytes[30 - 4 * k] = (w & 0xff) as u8;
                bytes[31 - 4 * k] = ((w >> 8) & 0xff) as u8;
            }
        }
        U256::from_be_bytes(bytes)
    }

    /// 结构性校验：至少 1 档且所有宽度非零。
    pub fn is_valid(&self) -> bool {
        let n = self.count as usize;
        n > 0 && n <= ELFOMO_MAX_BANDS && self.widths[..n].iter().all(|w| *w > 0)
    }
}

/// 单个 pool（base asset）的做市 ladder 参数。
///
/// 链上每个 Pool 按 asset 各存一份 11 字段配置（[`IElfomoFiPool::getMetadata`]），
/// 另有一条**逐档 profile word**（宽度表 + 偏离表，见 [`ElfomoBandProfile`]）。
/// 两者都是 per-pool 数据：本地在 `init` 阶段逐池读取并落库，
/// **代码里没有任何 pair 特判或地址常量**——新增 pair 自动获得自己的参数。
///
/// ## 档位生成规则（2026-09-11 全网格逐位实证：30,240 组 bit-exact）
///
/// 设 `U = unit`、`T = band_count`、`F = spread_penalty`、`SL = spread_level`；
/// 宽度表 `PREFIX = profile.widths`（单位 `U`）与偏离表 `D = profile.deviations`
/// （单位 1e-5）**都来自链上 profile word**（见 [`ElfomoBandProfile`]）：
///
/// - **from→to（`token_x` 侧）**：maker 补库存方向，前缀容量 `C = (T−1)·U − vault_xeth`。
///   `C > 0` 时前缀按 `PREFIX` 逐档截断到 `C`，尾部再补一档 `5T·U`；逐档以剩余
///   `vault_usdt0` 预算封顶（`size = min(rem/price, width)`）。`C == 0`
///   （即 `vault_xeth ≥ (T−1)·U`）时链上只给**一档** `min(T·U − vault_xeth, 预算)`
///   残余档、价恒为尾档价；`vault_xeth ≥ T·U` 则不报价。第 `i` 档斜率
///   `= 100000 − dev_i`，尾档恒 `50000`。
/// - **to→from（`token_y` 侧）**：maker 减库存方向，前缀容量 `C = vault_xeth − U`，
///   尾部再补一档 `U`；逐档以剩余 `vault_xeth` 封顶。第 `i` 档斜率 `= 100000 + dev_i`，
///   尾档恒 `150000`。
/// - **`dev_i = profile.deviations[i]`**，加宽时 `i ≥ 1` 的所有档位再 `+F`；
///   前缀只有 1 档时该档（`i == 0`）同样 `+F`。
/// - **加宽开关由金库余额决定（不是静态配置）**：`from→to` 用容量 `C`、
///   `to→from` 用 `vault_xeth` 作判据，`SL · U ≥ 容量` 即加宽。真实池
///   `T=30, SL=5, U=0.6e18`：`vault_xeth ≈ 3.68U` 时 `to→from` 已加宽、`from→to` 未加宽。
///
/// 任一字段取错都会被 `verify_model_against_chain` 的逐位对拍 **fail-closed** 拦下
/// （拒绝报价而不是出错价）。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ElfomoLadderConfig {
    /// 档位宽度单位 `U`（`getMetadata.unit`，uint128）。
    #[serde(default)]
    pub unit: U256,
    /// 档位数量 `T`（`getMetadata.band_count`）——同时决定可用斜率档数与尾档宽度。
    #[serde(default)]
    pub band_count: u16,
    /// 价差档位（`getMetadata.spread_level`）：与 `U` 相乘后跟**本侧容量**比较决定是否加宽。
    #[serde(default)]
    pub spread_level: u16,
    /// 价差步长 `F`（`getMetadata.spread_penalty`）：加宽时 `i ≥ 1`（或唯一档）追加的偏离量。
    #[serde(default)]
    pub spread_penalty: u8,
    /// 逐档宽度表 + 偏离表（链上 profile word，**动态存储**，缺省 = 未读取 → 不报价）。
    #[serde(default)]
    pub profile: ElfomoBandProfile,
    /// profile word 的存储键 = 本 pair 在 `Pool.getSupportedPairs()` 中的下标。
    ///
    /// 槽位 `keccak256(pad32(profile_key)‖pad32(0))`。读取时由链上 pair 列表现算，
    /// 运行期由 `0xd4ff31bd` calldata 的 `keys[]` 对齐（同一组下标）。
    #[serde(default)]
    pub profile_key: u64,
}

// ---------------------------------------------------------------------------
// 多链 / 实现变体说明（2026-09-11 真链实测；**当前只支持 XLayer**）
//
// 协议在每个链上用同一个 Router/Factory 地址各自部署，pool 合约是**按链独立部署
// 的实现**，因此 ladder 的两张表**不是跨链常量**：
//
// | 项 | XLayer（已支持） | Base（已核查、未支持） |
// |---|---|---|
// | `getMetadata(address)` | 有（selector `0x2a50c146`） | 有，ABI/存储布局相同 |
// | `getMetadata(quote)` | 返回**全零** | 直接 **revert**（`0x672215de`）→ `fetch_ladder` 已逐 asset 容错 |
// | metadata word byte28（ABI `f3`） | `0` | `1`（XLayer 上把该字节改非 0 → `getOrderbook` revert / `not implemented`，说明 `f3` 是**实现变体开关**） |
// | `DEVIATIONS`（偏离表） | `[7,10,15,25,40,50]` | `[2,3,15,25,40,50]`（仅前两档不同） |
// | `PREFIX_WIDTHS` | `[1,5,10,10,20,100]` | 实测相同 |
// | 价格种子 | `slot1 >> 32` | **不是** `slot1>>32`（槽位打包不同，需另行逆向） |
// | vault 余额 | `token.balanceOf(vault)` | 未核查 |
//
// **结论/维护指引**：本模块只对 XLayer（`f3 = 0` 变体）做过全网格逐位对拍，因此
// 当前只在 XLayer 启用。接入 Base 等变体时的最小步骤：
//   ① 从 `getMetadata` 的原始 word 里读出 `f3`（本模块目前不解析该字段）；
//   ② 按变体给出对应的 `DEVIATIONS`（必要时连 `PREFIX_WIDTHS`/容量规则一起）；
//   ③ 逆向该链的种子读取方式（Base 不是 `slot1>>32`）与 vault 读法；
//   ④ 用与 XLayer 相同的方法（anvil fork + `eth_call` stateOverride 全网格）逐位对拍，
//      结果固化进 fixture 单测。**在对拍通过之前不要开启**——`verify_model_against_chain`
//      会把不匹配的池判为 `model_verified = false`（fail-closed，不报价，不会出错价）。
// ---------------------------------------------------------------------------

impl ElfomoLadderConfig {
    /// `from→to` 尾部档斜率。
    pub const FT_TAIL_SLOPE: u64 = 50_000;
    /// `to→from` 尾部档斜率。
    pub const TF_TAIL_SLOPE: u64 = 150_000;
    /// 斜率基准（偏离都相对它计算）。
    pub const SLOPE_BASE: u64 = 100_000;

    /// 从链上 `getMetadata(asset)` 的 11 字段解码出 ladder 参数。
    ///
    /// 字段顺序与 [`IElfomoFiPool::getMetadata`] 返回一致：
    /// `[_, decimals, _, _, _, unit, band_count, spread_level, spread_penalty, ..]`。
    pub fn from_metadata(fields: &[U256; 11]) -> Self {
        let as_u16 = |v: U256| -> u16 { v.try_into().unwrap_or(u16::MAX) };
        let as_u8 = |v: U256| -> u8 { v.try_into().unwrap_or(u8::MAX) };
        Self {
            unit: fields[5],
            band_count: as_u16(fields[6]),
            spread_level: as_u16(fields[7]),
            spread_penalty: as_u8(fields[8]),
            profile: ElfomoBandProfile::default(),
            profile_key: 0,
        }
    }

    /// base asset 的 `decimals`（`getMetadata.decimals`，字段 1）。
    pub fn metadata_decimals(fields: &[U256; 11]) -> u8 {
        fields[1].try_into().unwrap_or(0)
    }

    /// 结构性校验：`U > 0`、`T > 0` 且 profile 合法（至少 1 档、宽度非零）。
    /// 缺省（未 init / 旧序列化状态 / profile 未读取）即 invalid，
    /// 后续本地报价为空、模型自证判不可信 → **fail-closed**。
    pub fn is_valid(&self) -> bool {
        !self.unit.is_zero() && self.band_count > 0 && self.profile.is_valid()
    }
}

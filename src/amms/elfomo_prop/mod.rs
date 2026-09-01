//! # ElfomoFi propAMM (XLayer)
//!
//! 调研/逆向文档：`docs/2026-09-01_elfomo_prop_xlayer_research.md`（长期维护必读）。
//!
//! ElfomoFi 是 XLayer 上的 proprietary AMM（PropAMM）：链下做市引擎（MM）每块
//! 更新 Pool 合约内 per-asset 的 3 档 orderbook（价格随 oracle/链上信号漂移，
//! 实测存在），Router 负责报价与执行，金库（Gnosis Safe）持币背书整仓。
//! 公式全在 Pool（`0x561fa97d` 返回 packed 5-word，Factory/Router 仅透传）。
//!
//! ## 报价模型（固定块 `0x423c2b8` 逐位对拍锁定，双向 33 点精确命中；
//! 真实链 10 块 + anvil vault 全量扫描复验）
//!
//! `Factory.getOrderbook(xETH, USDT0)`（公开 `0x0a6e04cb`）返回两个
//! `(size, price)[]`，**但 orderbook 不是链上持久化状态，而是 Pool 在
//! 每次读取时按 `(price_seed, vault 余额)` 实时算出来的纯函数**
//! （`debug_traceCall` 实证：每次报价 Pool 都会实时 staticcall
//! `token.balanceOf(vault)`）。因此本地模拟必须同样"读时重算"，不能缓存档位递减：
//!
//! - **价格**：`a = slot1 >> 32`；`q = (a >> 22) & 0x3f`，
//!   `qs = q>=32 ? q-64 : q`；`low = a & 0x3fffff`；
//!   `base = (100000 + qs) × low`。每档 `price = slope × base`（定点 1e24）。
//! - **from→to 档位**（size=输入量）：深度 `DEPTH1=[0.6e18,3e18,6e18,
//!   4859537498999137814,9e19]`，斜率 `[99993,99990,99985,99975,50000]`；
//!   `rem=vault_usdt0×1e24`，逐档 `cap=rem//price`、`s=min(DEPTH1[i],cap)`，
//!   `rem -= ceil(s×price/1e24)×1e24`，`s<DEPTH1[i]` 即停（余量档）。
//! - **to→from 档位**（size=输出量）：深度 `DEPTH2=[0.6e18,3e18,6e18,6e18,
//!   12e18,60e18,0.6e18]`；`rem=vault_xeth`，逐档 `cap=rem-0.6e18`、
//!   `s=min(DEPTH2[i],cap)`，`s<DEPTH2[i]` 即停；**尾部 0.6e18 恒显示**。
//!   斜率按档位数：`n=1`→`[150000]`；`n=2`→`[100067,150000]`；
//!   `n=3`→`[100007, (s₂≤1.8e18?100070:100010), 150000]`；
//!   `n≥4`→`[100007,100010,100015,100025,100040,100050,...]` 依次 +5，尾部 150000。
//! - **撮合**：`from→to` 逐档 `out += floor(take×price/1e24)`，封顶
//!   `min(总输出, vault USDT0)`；`to→from` 输出量逐档
//!   `need=ceil(size×price/1e24)`，`剩余≥need` 取满档否则
//!   `out += floor(剩余×1e24/price)`，封顶 `min(out, vault xETH)`。
//! - exact-out（`getAmountIn`）：容量内逐档 `rem ≥ 档输出/档 size` 取满整档，
//!   否则 `in += ceil(rem×1e24/price)`（正向）或 `ceil(rem×price/1e24)`（反向）；
//!   超容量返回 0。
//!
//! 价格定点基 1e24；算术用全精度整数（链上 OZ mulDiv 512 位，本地 U256 等价）。
//!
//! ## 数据同步（参照 binaryfi_prop / caliber_prop：raw-tx 本地直算优先，零 RPC）
//!
//! 报价更新机制（2026-09-01 链上实证）：MM keeper 每块向 Pool 发一笔
//! `updatePrices(uint256)`（selector `0xae7e8d81`），Pool 同步 emit 一条空
//! data 事件（topic `0xc5d08cbe…`）并更新 slot1。**calldata 参数就是价格种子**：
//! 实测 `arg ≈ (a<<32) | (ts-1)`，`a = arg >> 32` 可直接从原始交易解析，
//! 无需任何 RPC 即可在本地重算整本 orderbook。
//!
//! 1. **L3 — flashblocks 原始交易流（主通道，零 RPC）**：
//!    `xlayer_flashblocks` 流按 selector `0xae7e8d81` 拦截发往 Pool 的
//!    `updatePrices` 交易，解析出种子 `a` → `apply_price_seed` 本地重算
//!    orderbook；同块该交易 emit 的空事件被过滤（避免重复 AsyncUpdate）。
//!    `ElfomoTrade`（Router emit，topic `0xbe65a3f1…e2528`，data =
//!    [executor, receiver, fromToken, toToken, fromAmount, toAmount]）
//!    驱动金库余额递减（orderbook 随余额自动重算）。事件里的
//!    fromAmount/toAmount 是**实际成交额**（与 router `swap` 的
//!    `int256 specifiedAmount` 符号无关：负值=exact-out，正值=exact-in，
//!    事件均携带实际 input/output），本地账本按事件实际金额处理即可。
//!    关键路径完全本地。
//! 2. **L1 — 事件通道（无 raw-tx 时的回退）**：Pool `updatePrices` 空事件
//!    本身不含种子 → `SyncAction::AsyncUpdate` 重拉 `getOrderbook` +
//!    slot1 + vault `balanceOf` 真值（仅 flashblocks 断流/未覆盖时触发）。
//! 3. **L2 — 周期快照（最后兜底）**：`start_elfomo_prop_sync_task` 低频重拉
//!    整档回正 + 种子 + vault `balanceOf`，覆盖断流/重连/漏块等极端场景。

use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{address, Address, B256, U256},
    providers::Provider,
    rpc::types::Log,
    sol,
};
use serde::{Deserialize, Serialize};
use tracing::{instrument, warn};

use crate::amms::{
    amm::{AutomatedMarketMaker, SyncAction, AMM},
    error::AMMError,
    Token,
};

use crate::amms::elfomo_prop::types::{LevelConsumed, OrderbookLevel, OrderbookSnapshot};

pub mod factory;
pub mod types;

// ============================================================================
// 常量（XLayer 实测地址，2026-09-01）
// ============================================================================

/// XLayer chain id
pub const ELFOMO_CHAIN_ID: u64 = 196;

/// Router（报价/swap 入口，ElfomoTrade 事件由它 emit）
pub const ELFOMO_ROUTER_ADDRESS: Address = address!("0xf0f0f0f0fb0d738452efd03a28e8be14c76d5f73");
/// Factory 代理（getOrderbook / pair→pool 映射）
pub const ELFOMO_FACTORY_ADDRESS: Address = address!("0xffffffbb2d432b8acb4c57d556c0c721a431d038");
/// Pool（orderbook 存储与计算所在，非代理）
pub const ELFOMO_POOL_ADDRESS: Address = address!("0x02dcdf4171939ac0fe28e48e8758649311e9459a");
/// Vault（Gnosis Safe，仅持币背书）
pub const ELFOMO_VAULT_ADDRESS: Address = address!("0xbb1b19f138db3925883a96ff7a304277460e0c99");
/// 资产：xETH（18 dp）
pub const ELFOMO_XETH_ADDRESS: Address = address!("0xe7b000003a45145decf8a28fc755ad5ec5ea025a");
/// 资产：USDT0（6 dp）
pub const ELFOMO_USDT0_ADDRESS: Address = address!("0x779ded0c9e1022225f8e0630b35a9b54be713736");

/// Router emit 的 `ElfomoTrade` 事件 topic0
pub const ELFOMO_TRADE_EVENT: B256 = B256::new([
    0xbe, 0x65, 0xa3, 0xf1, 0xf3, 0x81, 0xda, 0x16, 0x73, 0x2d, 0xf7, 0x86, 0xf5, 0x71, 0x60, 0x4a,
    0x72, 0xb7, 0xc1, 0x22, 0xcf, 0xf3, 0xae, 0x2b, 0x35, 0x55, 0x66, 0xdd, 0xf0, 0x1e, 0x25, 0x28,
]);

/// Pool emit 的 `updatePrices` 空事件 topic0（每块 1 笔，MM keeper 驱动）。
/// data 为空，仅作"价格已漂移"的实时触发信号；真值需重拉 `getOrderbook`。
pub const ELFOMO_UPDATE_EVENT: B256 = B256::new([
    0xc5, 0xd0, 0x8c, 0xbe, 0x6f, 0xd3, 0xeb, 0xc2, 0x4e, 0x5a, 0x48, 0x36, 0x16, 0xdd, 0xdb, 0xc6,
    0x3b, 0x2a, 0xff, 0x5c, 0x08, 0x2c, 0x7d, 0x69, 0x76, 0x03, 0xab, 0x52, 0x10, 0x79, 0xf8, 0x09,
]);

/// Pool `updatePrices(uint256)` selector（flashblocks raw-tx 主通道用）
pub const ELFOMO_UPDATE_SELECTOR: [u8; 4] = [0xae, 0x7e, 0x8d, 0x81];

/// 价格定点基（1e24 = 0xD3C21BCECCEDA1000000，64-bit limbs 小端）
const ONE_E24: U256 = U256::from_limbs([0x1bcecceda1000000, 0xd3c2, 0, 0]);

// ----------------------------------------------------------------------------
// orderbook 生成公式常量（真实链 10 块 + anvil vault 全量扫描复验，见模块文档）
// ----------------------------------------------------------------------------

/// from→to 每档深度（xETH 输入量上限）
const DEPTH1: [U256; 5] = [
    U256::from_limbs([0x853a0d2313c0000, 0, 0, 0]), // 0.6e18
    U256::from_limbs([0x29a2241af62c0000, 0, 0, 0]), // 3e18
    U256::from_limbs([0x53444835ec580000, 0, 0, 0]), // 6e18
    U256::from_limbs([0x43708b9bc088a616, 0, 0, 0]), // 4859537498999137814
    U256::from_limbs([0xe1003b28d9280000, 0x4, 0, 0]), // 9e19
];
/// from→to 每档价格斜率（price = slope × base）
const SLOPES_FT: [u64; 5] = [99_993, 99_990, 99_985, 99_975, 50_000];

/// to→from 每档深度（xETH 输出量上限；最后一档为恒显尾部 0.6e18）
const DEPTH2: [U256; 7] = [
    U256::from_limbs([0x853a0d2313c0000, 0, 0, 0]), // 0.6e18
    U256::from_limbs([0x29a2241af62c0000, 0, 0, 0]), // 3e18
    U256::from_limbs([0x53444835ec580000, 0, 0, 0]), // 6e18
    U256::from_limbs([0x53444835ec580000, 0, 0, 0]), // 6e18
    U256::from_limbs([0xa688906bd8b00000, 0, 0, 0]), // 12e18
    U256::from_limbs([0x40aad21b3b700000, 0x3, 0, 0]), // 60e18
    U256::from_limbs([0x853a0d2313c0000, 0, 0, 0]), // 0.6e18 尾部
];
/// to→from 非尾部档位斜率（n≥4 时依次 +5；n<4 另有规则见 `build_orderbook`）
const SLOPES_TF: [u64; 7] = [
    100_007, 100_010, 100_015, 100_025, 100_040, 100_050, 150_000,
];

/// to→from n=3 时第二档斜率切换阈值（size ≤ 此值用 100070，否则 100010）
const TF_N3_SLOPE_THRESHOLD: U256 = U256::from_limbs([0x18fae27693b40000, 0, 0, 0]); // 1.8e18

/// 价格种子位域掩码（与链上 `0x561fa97d` 内部一致）
const SEED_Q_MASK: u64 = 0x3f;
const SEED_LOW_MASK: u64 = 0x3f_ffff;

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IElfomoVault {
        function balanceOf(address account) external view returns (uint256);
    }
}

// ============================================================================
// ElfomoFiPropPool
// ============================================================================

/// ElfomoFi propAMM 池子（每 pair 一个独立 pool 合约，参照 caliber_prop
/// 拆独立池子管理：token_x/token_y 定义 pair，pool/vault 地址均随实例
/// 保存，由 Factory/部署配置在初始化阶段传入，不依赖全局常量）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElfomoFiPropPool {
    /// 池子合约地址（orderbook 存储与计算所在）
    pub pool_address: Address,
    /// pair 的 token 0（from→to 输入侧，对应 vault_xeth 余额）
    pub token_x: Address,
    /// pair 的 token 1（from→to 输出侧，对应 vault_usdt0 余额）
    pub token_y: Address,
    /// 价格种子 `a`（Pool slot1 >> 32，updatePrices calldata 直接携带）。
    /// orderbook 是 `(a, vault_usdt0, vault_xeth)` 的读时纯函数，
    /// 本地报价实时重算（见 `build_orderbook`）。
    pub price_seed: U256,
    /// Factory 代理地址（getOrderbook 快照来源）
    pub factory_address: Address,
    /// Router 地址（swap 事件来源）
    pub router_address: Address,
    /// 金库合约地址（持币背书，balanceOf 快照来源）
    pub vault_address: Address,
    /// 链 ID
    pub chain_id: u64,
    /// 创建区块号（StateSpace 扫描起点）
    pub created_block: u64,
    /// 最后同步区块号
    pub last_synced_block: u64,
    /// 资产列表（[xETH, USDT0]）
    pub tokens: Vec<Token>,
    /// 订单簿快照（两侧各 3 档 + 金库余额）
    pub levels: OrderbookSnapshot,
    /// 本地档位消耗状态（L1 事件驱动，L2 快照整档回正）
    pub consumed: LevelConsumed,
}

impl Default for ElfomoFiPropPool {
    fn default() -> Self {
        Self {
            pool_address: ELFOMO_POOL_ADDRESS,
            token_x: ELFOMO_XETH_ADDRESS,
            token_y: ELFOMO_USDT0_ADDRESS,
            factory_address: ELFOMO_FACTORY_ADDRESS,
            router_address: ELFOMO_ROUTER_ADDRESS,
            vault_address: ELFOMO_VAULT_ADDRESS,
            chain_id: ELFOMO_CHAIN_ID,
            created_block: 0,
            last_synced_block: 0,
            price_seed: U256::ZERO,
            tokens: Vec::new(),
            levels: OrderbookSnapshot::default(),
            consumed: LevelConsumed::new(0, 0),
        }
    }
}

impl ElfomoFiPropPool {
    /// 构建池子骨架（资产/档位在 init 时填充）
    fn skeleton(
        pool_address: Address,
        token_x: Address,
        token_y: Address,
        factory_address: Address,
        router_address: Address,
        vault_address: Address,
        chain_id: u64,
        created_block: u64,
    ) -> Self {
        Self {
            pool_address,
            token_x,
            token_y,
            factory_address,
            router_address,
            vault_address,
            chain_id,
            created_block,
            last_synced_block: 0,
            price_seed: U256::ZERO,
            tokens: Vec::new(),
            levels: OrderbookSnapshot::default(),
            consumed: LevelConsumed::new(0, 0),
        }
    }

    fn token_index(&self, token: Address) -> Option<usize> {
        self.tokens.iter().position(|t| t.address == token)
    }

    // ------------------------------------------------------------------------
    // Quote 公式（纯函数，链上逐位对拍；price 定点 1e24）
    // ------------------------------------------------------------------------

    /// ceil 除法
    fn ceil_div(a: U256, b: U256) -> U256 {
        if a.is_zero() {
            return U256::ZERO;
        }
        (a + b - U256::from(1)) / b
    }

    /// 从 `updatePrices(uint256)` calldata 解析价格种子 `a`。
    ///
    /// 链上实证（2026-09-01）：MM keeper 每块发的 `0xae7e8d81` 交易，
    /// calldata 参数 `arg ≈ (a<<32) | (ts-1)`，`arg >> 32` 即 Pool slot1
    /// 高 32 位价格种子，无需任何 RPC 即可本地重算 orderbook。
    pub fn parse_update_prices_calldata(input: &[u8]) -> Option<U256> {
        if input.len() < 36 || input[..4] != ELFOMO_UPDATE_SELECTOR {
            return None;
        }
        Some(U256::from_be_slice(&input[4..36]) >> 32)
    }

    /// 按种子 `a` + 金库余额生成完整 orderbook（纯函数，与链上逐位一致）。
    ///
    /// 公式已用真实链 10 个块（fromTo/toFrom 全对）+ anvil vault 余额全量
    /// 扫描复验；链上每次读取都实时用 `balanceOf(vault)` 重算，本地同构。
    pub fn build_orderbook(seed: U256, vault_usdt0: U256, vault_xeth: U256) -> OrderbookSnapshot {
        // q 取位 22..27、low 取位 0..21，只需种子低 27 位
        let low_bits = (seed & U256::from(0x7ff_ffffu64)).to::<u64>();
        let q = (low_bits >> 22) & SEED_Q_MASK;
        let qs = if q >= 32 { q as i64 - 64 } else { q as i64 };
        let low = low_bits & SEED_LOW_MASK;
        let base = U256::from((100_000i64 + qs) as u64) * U256::from(low);

        // from→to（xETH→USDT0，size = 输入量）
        let mut from_to_levels = Vec::with_capacity(SLOPES_FT.len());
        let mut rem = vault_usdt0 * ONE_E24;
        for (i, slope) in SLOPES_FT.iter().enumerate() {
            let price = U256::from(*slope) * base;
            let cap = rem / price;
            if cap.is_zero() {
                break;
            }
            let depth = DEPTH1[i];
            let size = cap.min(depth);
            from_to_levels.push(OrderbookLevel::new(size, price));
            rem -= Self::ceil_div(size * price, ONE_E24) * ONE_E24;
            if size < depth {
                break;
            }
        }

        // to→from（USDT0→xETH，size = 输出量）
        let mut sizes: Vec<U256> = Vec::with_capacity(DEPTH2.len());
        let mut rem = vault_xeth;
        let tail = DEPTH2[6];
        for (i, depth) in DEPTH2[..6].iter().enumerate() {
            let cap = rem.saturating_sub(tail);
            if cap.is_zero() {
                break;
            }
            let size = cap.min(*depth);
            sizes.push(size);
            rem -= size;
            if size < *depth {
                break;
            }
        }
        sizes.push(rem.min(tail));
        let n = sizes.len();
        let mut to_from_levels = Vec::with_capacity(n);
        for (i, size) in sizes.into_iter().enumerate() {
            let slope = if n == 1 {
                150_000
            } else if n == 2 {
                if i == 0 {
                    100_067
                } else {
                    150_000
                }
            } else if n == 3 {
                if i == 0 {
                    100_007
                } else if i == 1 {
                    if size <= TF_N3_SLOPE_THRESHOLD {
                        100_070
                    } else {
                        100_010
                    }
                } else {
                    150_000
                }
            } else if i == n - 1 {
                150_000
            } else {
                SLOPES_TF[i]
            };
            to_from_levels.push(OrderbookLevel::new(size, U256::from(slope) * base));
        }

        OrderbookSnapshot {
            from_to_levels,
            to_from_levels,
            vault_usdt0,
            vault_xeth,
            price_seed: seed,
        }
    }

    /// 用当前种子 + 金库余额重建 orderbook 缓存（读时重算模型下的缓存刷新）。
    fn refresh_levels(&mut self) {
        self.levels = Self::build_orderbook(
            self.price_seed,
            self.levels.vault_usdt0,
            self.levels.vault_xeth,
        );
        self.consumed = LevelConsumed::new(
            self.levels.from_to_levels.len(),
            self.levels.to_from_levels.len(),
        );
    }

    /// 针对给定 orderbook 快照报价（纯函数；链上对拍/复用用）。
    ///
    /// 语义与 `simulate_swap` 完全一致（零 consumed、金库封顶），但使用外部
    /// 传入的 orderbook —— 用于"父块金库余额 + raw-tx 种子"这类跨状态对拍。
    pub fn simulate_swap_for_orderbook(
        ob: &OrderbookSnapshot,
        token_x: Address,
        token_y: Address,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> U256 {
        if token_in == token_x && token_out == token_y {
            let zero = vec![U256::ZERO; ob.from_to_levels.len()];
            Self::quote_fwd_exact(&ob.from_to_levels, &zero, amount_in, ob.vault_usdt0)
        } else if token_in == token_y && token_out == token_x {
            let zero = vec![U256::ZERO; ob.to_from_levels.len()];
            Self::quote_rev_exact(&ob.to_from_levels, &zero, amount_in, ob.vault_xeth)
        } else {
            U256::ZERO
        }
    }

    /// 应用 `updatePrices` raw-tx 解析出的价格种子（本地直算，零 RPC）。
    ///
    /// - 设置 `price_seed`，按**当前本地金库余额**重算整本 orderbook；
    /// - 档位随余额自动缩放（链上读时动态计算，同构）；
    /// - 单调推进 `last_synced_block`（与 `set_last_synced_block` 同语义）。
    pub fn apply_price_seed(&mut self, seed: U256, block_number: u64) {
        self.price_seed = seed;
        self.refresh_levels();
        self.last_synced_block = self.last_synced_block.max(block_number);
    }

    /// 正向 exact-in：from→to（size = 输入量），`out += floor(take×price/1e24)`，
    /// 封顶 `min(总输出, vault_usdt0)`。
    fn quote_fwd_exact(
        levels: &[OrderbookLevel],
        consumed: &[U256],
        amount_in: U256,
        vault_usdt0: U256,
    ) -> U256 {
        let mut out = U256::ZERO;
        let mut rem = amount_in;
        for (lv, c) in levels.iter().zip(consumed.iter()) {
            if rem.is_zero() {
                break;
            }
            let remaining = lv.size.saturating_sub(*c);
            if remaining.is_zero() {
                continue;
            }
            let take = rem.min(remaining);
            out += take * lv.price / ONE_E24;
            rem -= take;
        }
        out.min(vault_usdt0)
    }

    /// 反向 exact-in：to→from（size = 输出量），
    /// `need = ceil(size×price/1e24)`；`剩余≥need` 时 `out+=size`，
    /// 否则 `out += floor(剩余×1e24/price)`；封顶 `min(out, vault_xeth)`。
    fn quote_rev_exact(
        levels: &[OrderbookLevel],
        consumed: &[U256],
        amount_in: U256,
        vault_xeth: U256,
    ) -> U256 {
        let mut out = U256::ZERO;
        let mut rem = amount_in;
        for (lv, c) in levels.iter().zip(consumed.iter()) {
            if rem.is_zero() {
                break;
            }
            let remaining = lv.size.saturating_sub(*c);
            if remaining.is_zero() {
                continue;
            }
            let need = Self::ceil_div(remaining * lv.price, ONE_E24);
            if rem >= need {
                out += remaining;
                rem -= need;
            } else {
                out += rem * ONE_E24 / lv.price;
                break;
            }
        }
        out.min(vault_xeth)
    }

    /// 正向 exact-out：容量 = `min(Σ floor(remaining×price/1e24), vault_usdt0)`；
    /// `to > 容量 → 0`；逐档 `rem ≥ 档输出` 取满整档，否则
    /// `in += ceil(rem×1e24/price)`。
    fn quote_fwd_exact_out(
        levels: &[OrderbookLevel],
        consumed: &[U256],
        amount_out: U256,
        vault_usdt0: U256,
    ) -> U256 {
        let mut level_out = Vec::with_capacity(levels.len());
        let mut cap = U256::ZERO;
        for (lv, c) in levels.iter().zip(consumed.iter()) {
            let remaining = lv.size.saturating_sub(*c);
            let o = remaining * lv.price / ONE_E24;
            level_out.push(o);
            cap += o;
        }
        let cap = cap.min(vault_usdt0);
        if amount_out > cap {
            return U256::ZERO;
        }
        let mut amount_in = U256::ZERO;
        let mut rem = amount_out;
        for ((lv, c), o) in levels.iter().zip(consumed.iter()).zip(level_out.iter()) {
            if rem.is_zero() {
                break;
            }
            let remaining = lv.size.saturating_sub(*c);
            if remaining.is_zero() {
                continue;
            }
            if rem >= *o {
                amount_in += remaining;
                rem -= *o;
            } else {
                amount_in += Self::ceil_div(rem * ONE_E24, lv.price);
                break;
            }
        }
        amount_in
    }

    /// 反向 exact-out：`to > vault_xeth → 0`；逐档 `rem ≥ 档 size` 取满
    /// （`in += ceil(size×price/1e24)`），否则 `in += ceil(rem×price/1e24)`。
    fn quote_rev_exact_out(
        levels: &[OrderbookLevel],
        consumed: &[U256],
        amount_out: U256,
        vault_xeth: U256,
    ) -> U256 {
        if amount_out > vault_xeth {
            return U256::ZERO;
        }
        let mut amount_in = U256::ZERO;
        let mut rem = amount_out;
        for (lv, c) in levels.iter().zip(consumed.iter()) {
            if rem.is_zero() {
                break;
            }
            let remaining = lv.size.saturating_sub(*c);
            if remaining.is_zero() {
                continue;
            }
            if rem >= remaining {
                amount_in += Self::ceil_div(remaining * lv.price, ONE_E24);
                rem -= remaining;
            } else {
                amount_in += Self::ceil_div(rem * lv.price, ONE_E24);
                break;
            }
        }
        amount_in
    }

    // ------------------------------------------------------------------------
    // L1：档位消耗（ElfomoTrade 事件驱动）
    // ------------------------------------------------------------------------

    /// 按输入量消耗 from→to 档位
    fn consume_from_to(&mut self, amount_in: U256) {
        let mut rem = amount_in;
        for (lv, c) in self
            .levels
            .from_to_levels
            .iter()
            .zip(self.consumed.from_to.iter_mut())
        {
            if rem.is_zero() {
                break;
            }
            let remaining = lv.size.saturating_sub(*c);
            let take = rem.min(remaining);
            *c += take;
            rem -= take;
        }
    }

    /// 按输出量消耗 to→from 档位
    fn consume_to_from(&mut self, amount_out: U256) {
        let mut rem = amount_out;
        for (lv, c) in self
            .levels
            .to_from_levels
            .iter()
            .zip(self.consumed.to_from.iter_mut())
        {
            if rem.is_zero() {
                break;
            }
            let remaining = lv.size.saturating_sub(*c);
            let take = rem.min(remaining);
            *c += take;
            rem -= take;
        }
    }

    /// 应用订单簿快照（L2/init 整档回正 + 金库余额 + 价格种子）
    pub fn apply_orderbook_snapshot(
        &mut self,
        from_to_levels: Vec<OrderbookLevel>,
        to_from_levels: Vec<OrderbookLevel>,
        vault_usdt0: U256,
        vault_xeth: U256,
        price_seed: U256,
        block_number: u64,
    ) {
        self.levels = OrderbookSnapshot {
            from_to_levels,
            to_from_levels,
            vault_usdt0,
            vault_xeth,
            price_seed,
        };
        self.price_seed = price_seed;
        self.consumed = LevelConsumed::new(
            self.levels.from_to_levels.len(),
            self.levels.to_from_levels.len(),
        );
        self.last_synced_block = self.last_synced_block.max(block_number);
    }

    /// 拉取 orderbook + vault 余额 + 价格种子快照（L2/init 兜底通道）。
    ///
    /// 注意（链上实证）：vault 是 Gnosis Safe，**不能**在 vault 合约上调用
    /// `balanceOf`；余额必须读 `token.balanceOf(vault)`。价格种子读
    /// Pool slot1（`a = slot1 >> 32`），供本地 `build_orderbook` 使用。
    pub async fn fetch_orderbook_snapshot<N, P>(
        &self,
        provider: P,
        block: BlockId,
    ) -> Result<OrderbookSnapshot, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        use crate::amms::elfomo_prop::types::IElfomoFiFactory;

        let factory = IElfomoFiFactory::new(self.factory_address, provider.clone());
        let orderbook = factory
            .getOrderbook(self.token_x, self.token_y)
            .block(block)
            .call()
            .await?;
        let from_to = orderbook.fromToLevels;
        let to_from = orderbook.toFromLevels;

        // 余额在 token 合约上按 vault 地址读（vault 本身无 balanceOf）
        let usdt0 = IElfomoVault::new(self.token_y, provider.clone());
        let vault_usdt0 = usdt0
            .balanceOf(self.vault_address)
            .block(block)
            .call()
            .await?;
        let xeth = IElfomoVault::new(self.token_x, provider.clone());
        let vault_xeth = xeth
            .balanceOf(self.vault_address)
            .block(block)
            .call()
            .await?;

        // 价格种子：Pool slot1 高 32 位
        let slot1: U256 = provider
            .get_storage_at(self.pool_address, U256::from(1u64))
            .block_id(block)
            .await?;
        let price_seed = slot1 >> 32;

        Ok(OrderbookSnapshot {
            from_to_levels: from_to
                .into_iter()
                .map(|lv| OrderbookLevel::new(lv.size, lv.price))
                .collect(),
            to_from_levels: to_from
                .into_iter()
                .map(|lv| OrderbookLevel::new(lv.size, lv.price))
                .collect(),
            vault_usdt0,
            vault_xeth,
            price_seed,
        })
    }
}

// ============================================================================
// init_batch
// ============================================================================

impl ElfomoFiPropPool {
    /// 批量初始化：逐个 init（单池部署，无虚拟子池去重逻辑）
    pub async fn init_batch<N, P>(
        amms: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let mut initialized = Vec::with_capacity(amms.len());
        for amm in amms {
            let address = amm.address();
            match amm.init::<N, P>(block_number, provider.clone()).await {
                Ok(pool) => initialized.push(pool),
                Err(e) => {
                    warn!(
                        target: "amms::elfomo_prop",
                        pool = %address,
                        error = %e,
                        "elfomofi: failed to init pool"
                    );
                }
            }
        }
        Ok(initialized)
    }
}

// ============================================================================
// AutomatedMarketMaker impl
// ============================================================================

impl AutomatedMarketMaker for ElfomoFiPropPool {
    fn address(&self) -> Address {
        self.pool_address
    }

    fn supported_chains(&self) -> Option<Vec<u64>> {
        Some(vec![ELFOMO_CHAIN_ID])
    }

    fn last_synced_block(&self) -> u64 {
        self.last_synced_block
    }

    fn set_last_synced_block(&mut self, block_number: u64) {
        // 单调不回退（与 BinaryFi/UniswapV3 一致）：防止周期任务/AsyncUpdate
        // 用更旧块号覆盖本地已推进的日志状态
        self.last_synced_block = self.last_synced_block.max(block_number);
    }

    fn sync_events(&self) -> Vec<B256> {
        vec![ELFOMO_TRADE_EVENT, ELFOMO_UPDATE_EVENT]
    }

    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError> {
        let topics = log.topics();

        // L1a: Pool 每块 updatePrices 空事件（MM keeper 驱动）。
        // calldata 只含时间戳、价格由 Pool 内部按 slot1 种子动态计算，
        // 无法从事件/交易解析 → 返回 AsyncUpdate 重拉 getOrderbook 真值。
        // 每块 1 次，块级实时（非定时轮询）。
        if log.address() == self.pool_address
            && topics.len() == 1
            && topics[0] == ELFOMO_UPDATE_EVENT
        {
            return Ok(SyncAction::AsyncUpdate);
        }

        // L1b: Router ElfomoTrade 事件 → 本地档位消耗 + 金库余额递减
        if log.address() == self.router_address
            && topics.len() >= 1
            && topics[0] == ELFOMO_TRADE_EVENT
        {
            let data = log.data().data.as_ref();
            // data = [executor, receiver, fromToken, toToken, fromAmount, toAmount]
            if data.len() < 6 * 32 {
                return Ok(SyncAction::Resync);
            }
            let from_token = Address::from_word(B256::from_slice(&data[64..96]));
            let to_token = Address::from_word(B256::from_slice(&data[96..128]));
            let amount_in = U256::from_be_slice(&data[128..160]);
            let amount_out = U256::from_be_slice(&data[160..192]);
            if from_token == self.token_x && to_token == self.token_y {
                self.consume_from_to(amount_in);
                self.levels.vault_usdt0 = self.levels.vault_usdt0.saturating_sub(amount_out);
                // orderbook 是 (seed, vault) 的读时函数：金库递减后档位自动缩放
                self.refresh_levels();
                return Ok(SyncAction::None);
            }
            if from_token == self.token_y && to_token == self.token_x {
                self.consume_to_from(amount_out);
                self.levels.vault_xeth = self.levels.vault_xeth.saturating_sub(amount_out);
                self.refresh_levels();
                return Ok(SyncAction::None);
            }
            return Ok(SyncAction::None);
        }
        Ok(SyncAction::None)
    }

    fn tokens(&self) -> Vec<Address> {
        self.tokens.iter().map(|t| t.address).collect()
    }

    fn calculate_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
        if self.token_index(base_token).is_none() {
            return Err(AMMError::TokenNotFound(base_token));
        }
        if self.token_index(quote_token).is_none() {
            return Err(AMMError::TokenNotFound(quote_token));
        }
        if base_token == quote_token {
            return Ok(0.0);
        }
        // 用首档边际价：xETH→USDT0 边际 = price/1e12（USDT0/xETH）；
        // USDT0→xETH 边际 = price/1e12（USDT0/xETH），取倒数得 xETH/USDT0。
        let (level, is_fwd) = if base_token == self.token_x {
            (self.levels.from_to_levels.first(), true)
        } else {
            (self.levels.to_from_levels.first(), false)
        };
        let Some(lv) = level else {
            return Ok(0.0);
        };
        if lv.price.is_zero() {
            return Ok(0.0);
        }
        // price/1e24 × 1e6（USDT0 6dp）后，1 个 xETH（1e18 raw）的输出 =
        // price × 1e6 / 1e24 = price / 1e18（USDT0 raw）→ /1e6 = price/1e24 USDT0。
        // 即边际价（USDT0/xETH）= price / 1e12（price 定点含 1e12 缩放）。
        let per_xeth_usdt0 = u256_to_f64(&lv.price) / 1e12;
        Ok(if is_fwd {
            per_xeth_usdt0
        } else {
            1.0 / per_xeth_usdt0
        })
    }

    fn has_sufficient_liquidity(&self) -> bool {
        !self.levels.from_to_levels.is_empty()
            && !self.levels.to_from_levels.is_empty()
            && (!self.levels.vault_usdt0.is_zero() || !self.levels.vault_xeth.is_zero())
    }

    fn decimals(&self, token: Address) -> u8 {
        self.token_index(token)
            .and_then(|i| self.tokens.get(i))
            .map(|t| t.decimals)
            .unwrap_or(0)
    }

    fn simulate_swap(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        if self.token_index(token_in).is_none() {
            return Err(AMMError::TokenNotFound(token_in));
        }
        if self.token_index(token_out).is_none() {
            return Err(AMMError::TokenNotFound(token_out));
        }
        if token_in == token_out || amount_in.is_zero() {
            return Ok(U256::ZERO);
        }
        // orderbook 是 (seed, vault) 的读时纯函数：与链上每次读取实时
        // `balanceOf(vault)` 重算同构，本地不缓存档位递减。
        let ob = Self::build_orderbook(
            self.price_seed,
            self.levels.vault_usdt0,
            self.levels.vault_xeth,
        );
        let zero = vec![U256::ZERO; ob.from_to_levels.len()];
        if token_in == self.token_x && token_out == self.token_y {
            return Ok(Self::quote_fwd_exact(
                &ob.from_to_levels,
                &zero,
                amount_in,
                ob.vault_usdt0,
            ));
        }
        let zero = vec![U256::ZERO; ob.to_from_levels.len()];
        if token_in == self.token_y && token_out == self.token_x {
            return Ok(Self::quote_rev_exact(
                &ob.to_from_levels,
                &zero,
                amount_in,
                ob.vault_xeth,
            ));
        }
        Ok(U256::ZERO)
    }

    fn simulate_swap_mut(
        &mut self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let out = self.simulate_swap(token_in, token_out, amount_in)?;
        if out.is_zero() {
            return Ok(out);
        }
        if token_in == self.token_x && token_out == self.token_y {
            self.consume_from_to(amount_in);
            self.levels.vault_usdt0 = self.levels.vault_usdt0.saturating_sub(out);
        } else if token_in == self.token_y && token_out == self.token_x {
            self.consume_to_from(out);
            self.levels.vault_xeth = self.levels.vault_xeth.saturating_sub(out);
        }
        self.refresh_levels();
        Ok(out)
    }

    fn simulate_swap_exact_out(
        &self,
        token_in: Address,
        token_out: Address,
        amount_out: U256,
    ) -> Result<U256, AMMError> {
        if self.token_index(token_in).is_none() {
            return Err(AMMError::TokenNotFound(token_in));
        }
        if self.token_index(token_out).is_none() {
            return Err(AMMError::TokenNotFound(token_out));
        }
        if token_in == token_out || amount_out.is_zero() {
            return Err(AMMError::Msg("elfomofi: invalid exact out".to_string()));
        }
        let ob = Self::build_orderbook(
            self.price_seed,
            self.levels.vault_usdt0,
            self.levels.vault_xeth,
        );
        let zero = vec![U256::ZERO; ob.from_to_levels.len()];
        if token_in == self.token_x && token_out == self.token_y {
            return Ok(Self::quote_fwd_exact_out(
                &ob.from_to_levels,
                &zero,
                amount_out,
                ob.vault_usdt0,
            ));
        }
        let zero = vec![U256::ZERO; ob.to_from_levels.len()];
        if token_in == self.token_y && token_out == self.token_x {
            return Ok(Self::quote_rev_exact_out(
                &ob.to_from_levels,
                &zero,
                amount_out,
                ob.vault_xeth,
            ));
        }
        Ok(U256::ZERO)
    }

    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        // 固定块号，保证同一锚点内各字段一致
        let snap_block = match block_number {
            BlockId::Number(alloy::eips::BlockNumberOrTag::Number(num)) => num,
            _ => provider.get_block_number().await?,
        };
        let block = BlockId::Number(alloy::eips::BlockNumberOrTag::Number(snap_block));

        // 资产：pair 由实例字段 token_x/token_y 定义（Factory/部署配置传入）。
        // decimals 对已知默认 pair（xETH=18/USDT0=6）取常量，其余 token 兜底 18。
        self.tokens = vec![
            Token {
                address: self.token_x,
                decimals: 18,
                symbol: if self.token_x == ELFOMO_XETH_ADDRESS {
                    "xETH".to_string()
                } else {
                    "TOKEN0".to_string()
                },
                chain_id: self.chain_id,
                fot_tax: None,
            },
            Token {
                address: self.token_y,
                decimals: if self.token_y == ELFOMO_USDT0_ADDRESS {
                    6
                } else {
                    18
                },
                symbol: if self.token_y == ELFOMO_USDT0_ADDRESS {
                    "USDT0".to_string()
                } else {
                    "TOKEN1".to_string()
                },
                chain_id: self.chain_id,
                fot_tax: None,
            },
        ];

        let snap = self
            .fetch_orderbook_snapshot::<N, _>(provider, block)
            .await?;
        if snap.from_to_levels.is_empty() || snap.to_from_levels.is_empty() {
            warn!(
                target: "amms::elfomo_prop",
                pool = %self.pool_address,
                "elfomofi: init orderbook empty, pair may be offline"
            );
        }
        self.apply_orderbook_snapshot(
            snap.from_to_levels,
            snap.to_from_levels,
            snap.vault_usdt0,
            snap.vault_xeth,
            snap.price_seed,
            snap_block,
        );
        Ok(self)
    }

    #[instrument(skip_all, fields(pool = %self.pool_address))]
    async fn update<N, P>(&mut self, provider: P) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        self.update_at(provider, BlockId::latest()).await
    }
}

impl ElfomoFiPropPool {
    /// 在指定区块拉取 orderbook + vault 快照（StateSpace update 与周期任务共用）。
    ///
    /// 固定到具体块号：快照读取与块号一致，避免 `BlockId::latest()` 的隐式
    /// 取数块与本地日志推进产生竞态（参照 BinaryFi `update_at`）。
    pub async fn update_at<N, P>(&mut self, provider: P, block: BlockId) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let snap_block: u64 = match block {
            BlockId::Number(alloy::eips::BlockNumberOrTag::Number(num)) => num,
            _ => provider.get_block_number().await?,
        };
        let block = BlockId::Number(alloy::eips::BlockNumberOrTag::Number(snap_block));
        let snap = self.fetch_orderbook_snapshot(provider, block).await?;
        self.apply_orderbook_snapshot(
            snap.from_to_levels,
            snap.to_from_levels,
            snap.vault_usdt0,
            snap.vault_xeth,
            snap.price_seed,
            snap_block,
        );
        Ok(())
    }
}

/// f64 转换辅助（用于 spot price）
fn u256_to_f64(v: &U256) -> f64 {
    // U256 → f64：拆高 128 位
    let (hi, lo): (U256, U256) = (*v >> 128, v & U256::from(u128::MAX));
    (hi.to::<u128>() as f64) * 2f64.powi(128) + lo.to::<u128>() as f64
}

// ============================================================================
// 测试：固定块 `0x423c2b8` 链上逐位对拍矩阵
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn lev(size: u128, price: u128) -> OrderbookLevel {
        OrderbookLevel::new(U256::from(size), U256::from(price))
    }

    /// 本块 getOrderbook 实测档位
    fn snapshot() -> OrderbookSnapshot {
        OrderbookSnapshot {
            from_to_levels: vec![
                lev(600_000_000_000_000_000, 2_473_060_529_144_115),
                lev(3_000_000_000_000_000_000, 2_472_986_332_134_450),
                lev(4_161_015_515_317_950_639, 2_472_862_670_451_675),
            ],
            to_from_levels: vec![
                lev(600_000_000_000_000_000, 2_473_406_781_855_885),
                lev(1_740_462_501_000_862_186, 2_474_964_919_058_850),
                lev(600_000_000_000_000_000, 3_709_850_483_250_000),
            ],
            // 本块 vault 余额
            vault_usdt0: U256::from(19_192_415_254u64),
            vault_xeth: U256::from(2_940_462_501_000_862_186u128),
            // 本块价格种子（slot1 >> 32）
            price_seed: U256::from(0x143c60fu64),
        }
    }

    #[test]
    fn test_quote_fwd_exact() {
        let s = snapshot();
        let consumed = vec![U256::ZERO; 3];
        // 链上对拍点（块 0x423c2b8，xETH→USDT0）
        let cases: &[(u128, u128)] = &[
            (1, 0),
            (1_000, 0),
            (1_000_000, 0),
            (1_000_000_000_000, 2_473),
            (600_000_000_000_000_000, 1_483_836_317),
            (600_000_000_000_000_001, 1_483_836_317),
            (3_600_000_000_000_000_000, 8_902_795_313),
            (3_600_000_000_000_000_001, 8_902_795_313),
            (7_761_015_515_317_950_639, 19_192_415_251),
            (10_000_000_000_000_000_000, 19_192_415_251),
        ];
        for (inp, want) in cases {
            let got = ElfomoFiPropPool::quote_fwd_exact(
                &s.from_to_levels,
                &consumed,
                U256::from(*inp),
                s.vault_usdt0,
            );
            assert_eq!(got, U256::from(*want), "fwd exact-in in={inp}");
        }
    }

    #[test]
    fn test_quote_rev_exact() {
        let s = snapshot();
        let consumed = vec![U256::ZERO; 3];
        // 链上对拍点（块 0x423c2b8，USDT0→xETH）
        let cases: &[(u128, u128)] = &[
            (1, 404_300_662),
            (1_000_000, 404_300_662_283_162),
            (1_484_044_069, 599_999_999_954_099_341),
            (1_484_044_070, 600_000_000_000_000_000),
            (1_484_044_071, 600_000_000_404_046_131),
            (5_791_627_702, 2_340_462_500_631_336_739),
            (5_791_627_703, 2_340_462_501_000_862_186),
            (5_791_627_704, 2_340_462_501_270_414_828),
            (8_017_537_993, 2_940_462_501_000_862_186),
            (9_000_000_000, 2_940_462_501_000_862_186),
        ];
        for (inp, want) in cases {
            let got = ElfomoFiPropPool::quote_rev_exact(
                &s.to_from_levels,
                &consumed,
                U256::from(*inp),
                s.vault_xeth,
            );
            assert_eq!(got, U256::from(*want), "rev exact-in in={inp}");
        }
    }

    #[test]
    fn test_quote_fwd_exact_out() {
        let s = snapshot();
        let consumed = vec![U256::ZERO; 3];
        let cases: &[(u128, u128)] = &[
            (1, 404_357_269),
            (1_483_836_317, 600_000_000_000_000_000),
            (1_483_836_318, 600_000_000_404_369_401),
            (8_902_795_313, 3_600_000_000_000_000_000),
            (8_902_795_314, 3_600_000_000_404_389_622),
            (19_192_415_248, 7_761_015_513_700_392_153),
            (19_192_415_249, 7_761_015_514_104_781_775),
            (19_192_415_250, 7_761_015_514_509_171_396),
            (19_192_415_251, 7_761_015_515_317_950_639),
            (19_192_415_252, 0),
        ];
        for (to, want) in cases {
            let got = ElfomoFiPropPool::quote_fwd_exact_out(
                &s.from_to_levels,
                &consumed,
                U256::from(*to),
                s.vault_usdt0,
            );
            assert_eq!(got, U256::from(*want), "fwd exact-out to={to}");
        }
    }

    #[test]
    fn test_quote_rev_exact_out() {
        let s = snapshot();
        let consumed = vec![U256::ZERO; 3];
        let cases: &[(u128, u128)] = &[
            (1, 1),
            (100_000_000_000_000_000, 247_340_679),
            (600_000_000_000_000_000, 1_484_044_070),
            (600_000_000_000_000_001, 1_484_044_071),
            (2_340_462_501_000_862_185, 5_791_627_703),
            (2_340_462_501_000_862_186, 5_791_627_703),
            (2_940_462_501_000_862_186, 8_017_537_993),
            (2_940_462_501_000_862_187, 0),
        ];
        for (to, want) in cases {
            let got = ElfomoFiPropPool::quote_rev_exact_out(
                &s.to_from_levels,
                &consumed,
                U256::from(*to),
                s.vault_xeth,
            );
            assert_eq!(got, U256::from(*want), "rev exact-out to={to}");
        }
    }

    #[test]
    fn test_consume_then_quote() {
        let s = snapshot();
        let consumed = LevelConsumed::new(3, 3);
        let tokens = vec![
            Token {
                address: ELFOMO_XETH_ADDRESS,
                decimals: 18,
                symbol: "xETH".to_string(),
                chain_id: ELFOMO_CHAIN_ID,
                fot_tax: None,
            },
            Token {
                address: ELFOMO_USDT0_ADDRESS,
                decimals: 6,
                symbol: "USDT0".to_string(),
                chain_id: ELFOMO_CHAIN_ID,
                fot_tax: None,
            },
        ];
        let mut pool = ElfomoFiPropPool {
            tokens,
            levels: s.clone(),
            consumed: consumed.clone(),
            price_seed: U256::from(0x143c60fu64),
            ..ElfomoFiPropPool::default()
        };
        // 模拟一笔第一档内的小额 swap（输入 0.1215e18 < 档 1 容量 0.6e18）
        let amount_in = U256::from(121_513_229_231_558_820u128);
        let out = pool
            .simulate_swap(ELFOMO_XETH_ADDRESS, ELFOMO_USDT0_ADDRESS, amount_in)
            .unwrap();
        assert!(out > U256::ZERO);
        let vault_before = pool.levels.vault_usdt0;
        // 事件同步（ElfomoTrade）：消耗档位 + 金库递减
        pool.consume_from_to(amount_in);
        pool.levels.vault_usdt0 = pool.levels.vault_usdt0 - out;
        // 同档内价格线性，同输入输出不变；金库余额已扣减
        let out2 = pool
            .simulate_swap(ELFOMO_XETH_ADDRESS, ELFOMO_USDT0_ADDRESS, amount_in)
            .unwrap();
        assert_eq!(out2, out);
        assert_eq!(pool.levels.vault_usdt0, vault_before - out);
        // 反向 quote 用金库 xETH 封顶
        let rev = pool
            .simulate_swap(
                ELFOMO_USDT0_ADDRESS,
                ELFOMO_XETH_ADDRESS,
                U256::from(8_017_537_993u64),
            )
            .unwrap();
        assert_eq!(rev, pool.levels.vault_xeth);
    }

    #[test]
    fn test_sync_update_event_returns_async_update() {
        let mut pool = ElfomoFiPropPool::default();
        pool.tokens = vec![
            Token {
                address: ELFOMO_XETH_ADDRESS,
                decimals: 18,
                symbol: "xETH".to_string(),
                chain_id: ELFOMO_CHAIN_ID,
                fot_tax: None,
            },
            Token {
                address: ELFOMO_USDT0_ADDRESS,
                decimals: 6,
                symbol: "USDT0".to_string(),
                chain_id: ELFOMO_CHAIN_ID,
                fot_tax: None,
            },
        ];
        pool.levels = snapshot();

        // Pool updatePrices 空事件（每块 1 笔）→ AsyncUpdate（重拉 getOrderbook 真值）
        let update_log: Log = serde_json::from_value(serde_json::json!({
            "address": format!("{:#x}", ELFOMO_POOL_ADDRESS),
            "topics": [format!("{:#x}", ELFOMO_UPDATE_EVENT)],
            "data": "0x",
            "blockNumber": "0x423c2b8",
            "transactionIndex": "0x0",
            "logIndex": "0x0",
        }))
        .unwrap();
        assert!(matches!(
            pool.sync(&update_log).unwrap(),
            SyncAction::AsyncUpdate
        ));

        // 无关事件不触发
        let unrelated: Log = serde_json::from_value(serde_json::json!({
            "address": format!("{:#x}", ELFOMO_POOL_ADDRESS),
            "topics": [format!("{:#x}", B256::repeat_byte(0xab))],
            "data": "0x",
            "blockNumber": "0x423c2b8",
            "transactionIndex": "0x1",
            "logIndex": "0x1",
        }))
        .unwrap();
        assert!(matches!(pool.sync(&unrelated).unwrap(), SyncAction::None));
    }

    #[test]
    fn test_sync_trade_event_consumes_levels() {
        let s = snapshot();
        let tokens = vec![
            Token {
                address: ELFOMO_XETH_ADDRESS,
                decimals: 18,
                symbol: "xETH".to_string(),
                chain_id: ELFOMO_CHAIN_ID,
                fot_tax: None,
            },
            Token {
                address: ELFOMO_USDT0_ADDRESS,
                decimals: 6,
                symbol: "USDT0".to_string(),
                chain_id: ELFOMO_CHAIN_ID,
                fot_tax: None,
            },
        ];
        let mut pool = ElfomoFiPropPool {
            tokens,
            levels: s.clone(),
            consumed: LevelConsumed::new(3, 3),
            price_seed: U256::from(0x143c60fu64),
            ..ElfomoFiPropPool::default()
        };
        let amount_in = U256::from(121_513_229_231_558_820u128);
        let out = ElfomoFiPropPool::quote_fwd_exact(
            &s.from_to_levels,
            &pool.consumed.from_to,
            amount_in,
            s.vault_usdt0,
        );
        assert!(out > U256::ZERO);

        // Router emit 的 ElfomoTrade：data = [executor, receiver, from, to, in, out]
        let mut data = Vec::new();
        for w in [
            U256::from(0x1234u64), // executor
            U256::from(0x5678u64), // receiver
            U256::ZERO,            // fromToken（占位，用 topics 对齐）
            U256::ZERO,            // toToken（占位）
            amount_in,
            out,
        ] {
            data.extend_from_slice(&w.to_be_bytes::<32>());
        }
        let from_token = ELFOMO_XETH_ADDRESS.into_word();
        let to_token = ELFOMO_USDT0_ADDRESS.into_word();
        // data 中 from/to token 用地址（左对齐 word）表达
        data[64..96].copy_from_slice(from_token.as_slice());
        data[96..128].copy_from_slice(to_token.as_slice());

        let trade_log: Log = serde_json::from_value(serde_json::json!({
            "address": format!("{:#x}", ELFOMO_ROUTER_ADDRESS),
            "topics": [
                format!("{:#x}", ELFOMO_TRADE_EVENT),
                format!("0x{:064x}", 1u64), // quoteId
                format!("0x{:064x}", 0u64), // partnerId
            ],
            "data": format!("0x{}", alloy::hex::encode(&data)),
            "blockNumber": "0x423b0c9",
            "transactionIndex": "0x0",
            "logIndex": "0x0",
        }))
        .unwrap();

        assert!(matches!(pool.sync(&trade_log).unwrap(), SyncAction::None));
        // 金库 USDT0 按成交额扣减；orderbook 是 (seed, vault) 读时函数，
        // 事件后缓存已重建（本笔成交量小，首档仍满 0.6e18，报价不变）
        assert_eq!(pool.levels.vault_usdt0, s.vault_usdt0 - out);
        assert_eq!(pool.levels.from_to_levels[0].size, s.from_to_levels[0].size);
        let out_again = pool
            .simulate_swap(ELFOMO_XETH_ADDRESS, ELFOMO_USDT0_ADDRESS, amount_in)
            .unwrap();
        assert_eq!(out_again, out);
    }

    #[test]
    fn test_build_orderbook_matches_chain_block() {
        // 块 0x423c2b8（seed=0x143c60f，vault 实测）逐位对拍
        let ob = ElfomoFiPropPool::build_orderbook(
            U256::from(0x143c60fu64),
            U256::from(19_192_415_254u64),
            U256::from(2_940_462_501_000_862_186u128),
        );
        assert_eq!(ob, snapshot());
        assert_eq!(ob.price_seed, U256::from(0x143c60fu64));
    }

    #[test]
    fn test_build_orderbook_reproduces_small_vault_levels() {
        // 金库 USDT0 低于首档容量阈值时，首档 size 随余额收缩（读时重算语义）
        let ob = ElfomoFiPropPool::build_orderbook(
            U256::from(0x143c60fu64),
            U256::from(1_000_000_000u64),
            U256::from(2_940_462_501_000_862_186u128),
        );
        assert_eq!(ob.from_to_levels.len(), 1);
        assert_eq!(
            ob.from_to_levels[0].size,
            U256::from(404_357_268_338_306_026u128)
        );
        assert_eq!(
            ob.from_to_levels[0].price,
            U256::from(2_473_060_529_144_115u128)
        );
    }

    #[test]
    fn test_parse_update_prices_calldata() {
        // 真实形态：arg = (a << 32) | (ts-1)，a 直接取高 32 位
        let a = U256::from(0x143c60fu64);
        let arg: U256 = (a << 32) | U256::from(0x6a96bd30u64);
        let mut input = Vec::new();
        input.extend_from_slice(&ELFOMO_UPDATE_SELECTOR);
        input.extend_from_slice(&arg.to_be_bytes::<32>());
        assert_eq!(
            ElfomoFiPropPool::parse_update_prices_calldata(&input),
            Some(a)
        );
        // 其它 selector / 截断输入 → None
        let bad = vec![0xde, 0xad, 0xbe, 0xef, 0x00];
        assert_eq!(ElfomoFiPropPool::parse_update_prices_calldata(&bad), None);
        assert_eq!(
            ElfomoFiPropPool::parse_update_prices_calldata(&input[..35]),
            None
        );
    }

    #[test]
    fn test_apply_price_seed_recomputes_orderbook() {
        let tokens = vec![
            Token {
                address: ELFOMO_XETH_ADDRESS,
                decimals: 18,
                symbol: "xETH".to_string(),
                chain_id: ELFOMO_CHAIN_ID,
                fot_tax: None,
            },
            Token {
                address: ELFOMO_USDT0_ADDRESS,
                decimals: 6,
                symbol: "USDT0".to_string(),
                chain_id: ELFOMO_CHAIN_ID,
                fot_tax: None,
            },
        ];
        let mut pool = ElfomoFiPropPool {
            tokens,
            levels: snapshot(),
            consumed: LevelConsumed::new(3, 3),
            price_seed: U256::from(0x143c60fu64),
            last_synced_block: 1,
            ..ElfomoFiPropPool::default()
        };
        let vault_usdt0 = pool.levels.vault_usdt0;
        // 换一个种子 → 价格全变、金库余额不变、块号单调推进
        let new_seed = U256::from(0x143c1dau64);
        pool.apply_price_seed(new_seed, 100);
        assert_eq!(pool.price_seed, new_seed);
        assert_eq!(pool.levels.vault_usdt0, vault_usdt0);
        assert_eq!(pool.levels.price_seed, new_seed);
        assert_eq!(pool.last_synced_block, 100);
        assert_ne!(
            pool.levels.from_to_levels[0].price,
            snapshot().from_to_levels[0].price
        );
        // 旧块号不回退
        pool.apply_price_seed(new_seed, 50);
        assert_eq!(pool.last_synced_block, 100);
    }

    #[test]
    fn test_real_arb_tx_ledger_replay() {
        // 真实套利交易 0x3a608dfefedf19731f01ba93945df8475fa9559eb40f5bae07334f991369e6f0
        // （块 69447881，status=0x1，ElfomoFi 段 xETH→USDT0）：
        // 同块 updatePrices calldata 解出种子 0x143c4e5 + 父块金库余额 →
        // 本地重算 orderbook → 报价精确等于事件 toAmount=300147468。
        // 这是「raw-tx 种子 + 本地金库 → 读时重算」模型的链上端到端回归锚点。
        let seed = U256::from(0x143c4e5u64);
        let ob = ElfomoFiPropPool::build_orderbook(
            seed,
            U256::from(19_492_562_722u64),
            U256::from(2_818_949_271_769_303_366u128),
        );
        let amount_in = U256::from(121_513_229_231_558_820u128);
        let got = ElfomoFiPropPool::quote_fwd_exact(
            &ob.from_to_levels,
            &vec![U256::ZERO; ob.from_to_levels.len()],
            amount_in,
            ob.vault_usdt0,
        );
        assert_eq!(got, U256::from(300_147_468u64));
    }

    #[test]
    fn test_sync_trade_event_reverse_decrements_vault_xeth() {
        let s = snapshot();
        let tokens = vec![
            Token {
                address: ELFOMO_XETH_ADDRESS,
                decimals: 18,
                symbol: "xETH".to_string(),
                chain_id: ELFOMO_CHAIN_ID,
                fot_tax: None,
            },
            Token {
                address: ELFOMO_USDT0_ADDRESS,
                decimals: 6,
                symbol: "USDT0".to_string(),
                chain_id: ELFOMO_CHAIN_ID,
                fot_tax: None,
            },
        ];
        let mut pool = ElfomoFiPropPool {
            tokens,
            levels: s.clone(),
            consumed: LevelConsumed::new(3, 3),
            price_seed: U256::from(0x143c60fu64),
            ..ElfomoFiPropPool::default()
        };
        // 反向成交：USDT0→xETH，事件 toAmount 即 xETH 实际输出
        let amount_out = U256::from(100_000_000_000_000_000u128); // 0.1 xETH
        let mut data = Vec::new();
        for w in [
            U256::from(0x1234u64), // executor
            U256::from(0x5678u64), // receiver
            U256::ZERO,
            U256::ZERO,
            U256::from(300_000_000u64), // 实际输入 USDT0
            amount_out,
        ] {
            data.extend_from_slice(&w.to_be_bytes::<32>());
        }
        data[64..96].copy_from_slice(ELFOMO_USDT0_ADDRESS.into_word().as_slice());
        data[96..128].copy_from_slice(ELFOMO_XETH_ADDRESS.into_word().as_slice());
        let trade_log: Log = serde_json::from_value(serde_json::json!({
            "address": format!("{:#x}", ELFOMO_ROUTER_ADDRESS),
            "topics": [
                format!("{:#x}", ELFOMO_TRADE_EVENT),
                format!("0x{:064x}", 1u64),
                format!("0x{:064x}", 0u64),
            ],
            "data": format!("0x{}", alloy::hex::encode(&data)),
            "blockNumber": "0x423b0c9",
            "transactionIndex": "0x0",
            "logIndex": "0x0",
        }))
        .unwrap();
        assert!(matches!(pool.sync(&trade_log).unwrap(), SyncAction::None));
        // 金库 xETH 按实际输出扣减；toFrom 档位随余额收缩后 s1+s2+s3 == vault
        assert_eq!(pool.levels.vault_xeth, s.vault_xeth - amount_out);
        let sum: U256 = pool.levels.to_from_levels.iter().map(|lv| lv.size).sum();
        assert_eq!(sum, pool.levels.vault_xeth);
        // 反向报价封顶仍等于金库 xETH
        let rev = pool
            .simulate_swap(
                ELFOMO_USDT0_ADDRESS,
                ELFOMO_XETH_ADDRESS,
                U256::from(8_017_537_993u64),
            )
            .unwrap();
        assert_eq!(rev, pool.levels.vault_xeth);
    }

    #[test]
    fn test_simulate_swap_read_time_recompute() {
        // 金库余额变化后，本地报价立即反映收缩后的 orderbook（不缓存档位递减）
        let mut pool = ElfomoFiPropPool {
            tokens: vec![
                Token {
                    address: ELFOMO_XETH_ADDRESS,
                    decimals: 18,
                    symbol: "xETH".to_string(),
                    chain_id: ELFOMO_CHAIN_ID,
                    fot_tax: None,
                },
                Token {
                    address: ELFOMO_USDT0_ADDRESS,
                    decimals: 6,
                    symbol: "USDT0".to_string(),
                    chain_id: ELFOMO_CHAIN_ID,
                    fot_tax: None,
                },
            ],
            levels: snapshot(),
            consumed: LevelConsumed::new(3, 3),
            price_seed: U256::from(0x143c60fu64),
            ..ElfomoFiPropPool::default()
        };
        let ob_small = ElfomoFiPropPool::build_orderbook(
            U256::from(0x143c60fu64),
            U256::from(1_000_000_000u64),
            U256::from(2_940_462_501_000_862_186u128),
        );
        // 直接把金库 USDT0 打到 1e9，报价必须按收缩档位走
        pool.levels.vault_usdt0 = U256::from(1_000_000_000u64);
        let out = pool
            .simulate_swap(
                ELFOMO_XETH_ADDRESS,
                ELFOMO_USDT0_ADDRESS,
                U256::from(600_000_000_000_000_000u128),
            )
            .unwrap();
        // 首档容量只剩 404357268338306026，多出的输入无档可吃
        assert_eq!(
            out,
            ElfomoFiPropPool::quote_fwd_exact(
                &ob_small.from_to_levels,
                &vec![U256::ZERO; 1],
                U256::from(600_000_000_000_000_000u128),
                ob_small.vault_usdt0,
            )
        );
        // exact-out 同样按收缩后的容量封顶
        let amount_in = pool
            .simulate_swap_exact_out(
                ELFOMO_XETH_ADDRESS,
                ELFOMO_USDT0_ADDRESS,
                U256::from(999_000_000u64),
            )
            .unwrap();
        assert!(amount_in > U256::ZERO);
    }
}

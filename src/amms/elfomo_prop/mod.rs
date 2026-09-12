//! # ElfomoFi propAMM (XLayer)
//!
//! 调研/逆向文档：`docs/2026-09-01_elfomo_prop_xlayer_research.md`（长期维护必读）。
//!
//! ElfomoFi 是 XLayer 上的 proprietary AMM（PropAMM）：链下做市引擎（MM）每块
//! 更新 Pool 合约内 per-asset 的 ladder orderbook（档数与宽度由该池 ladder 参数
//! 决定，价格随 oracle/链上信号漂移），Router 负责报价与执行，金库（Gnosis Safe）
//! 持币背书整仓。
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
//! - **档位生成**：宽度/偏离由**本 pool 的 ladder 参数**（[`ElfomoLadderConfig`]，
//!   `init` 时逐池从链上读取）现算——静态参数 `U/T/spread_*` 来自
//!   `Pool.getMetadata(asset)`；宽度表/偏离表来自 slot0 `mapping(pairIdx => word)`
//!   的 **profile word**（**动态存储，不是常量**，keeper 会用 `0xd4ff31bd` 改写，
//!   布局见 [`ElfomoBandProfile`]）。再加容量截断 + 尾档，加宽开关
//!   `spread_level·U ≥ 本侧容量`（由金库余额决定）。
//!   完整规则与实证见 [`ElfomoLadderConfig`] 与
//!   `build_orderbook_with`（唯一实现，逐位对拍链上）。
//! - **from→to 档位**（size=输入量）：容量 `(T−1)U − vault_xeth`；
//!   `rem=vault_usdt0×1e24`，逐档 `s=min(width,rem//price)`，
//!   `rem -= ceil(s×price/1e24)×1e24`，`s<width` 即停（余量档）。
//! - **to→from 档位**（size=输出量）：容量 `vault_xeth`，前缀容量 `vault_xeth−U`，
//!   尾部一档 `U`；`rem=vault_xeth`，逐档 `s=min(width,rem)`，`s<width` 即停；
//!   **尾档恒显示**。
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
//! data 事件（topic `0xc5d08cbe…`）并更新 slot1。**空事件零信息量、不是状态源**
//! （仅用于提取侧的 raw-tx 去重，见下）；**calldata 参数才是价格种子**：
//! 实测 `arg ≈ (a<<32) | (ts-1)`，`a = arg >> 32` 可直接从原始交易解析，
//! 无需任何 RPC 即可在本地重算整本 orderbook。
//!
//! 1. **L3 — flashblocks 原始交易流（主通道，零 RPC）**：
//!    `xlayer_flashblocks` 流拦截发往 Pool 的两类 calldata（信息都不在事件里）：
//!    `updatePrices(uint256)`（`0xae7e8d81`）→ 种子 `a` → `apply_price_seed`；
//!    keeper 逐档 profile 改写 `0xd4ff31bd(uint256[] keys,uint256[] words)` →
//!    `apply_band_profile`（按 pair 下标过滤）。两者都本地重算 orderbook；
//!    同块这些交易 emit 的空事件被过滤（避免重复 AsyncUpdate）。
//!    `ElfomoTrade`（Router emit，topic `0xbe65a3f1…e2528`，data =
//!    [executor, receiver, fromToken, toToken, fromAmount, toAmount]）
//!    驱动金库余额递减（orderbook 随余额自动重算）。事件里的
//!    fromAmount/toAmount 是**实际成交额**（与 router `swap` 的
//!    `int256 specifiedAmount` 符号无关：负值=exact-out，正值=exact-in，
//!    事件均携带实际 input/output），本地账本按事件实际金额处理即可。
//!    关键路径完全本地。
//! 2. **L1 — 覆盖率自证（零 RPC）**：Pool `updatePrices` 空事件已**不再订阅**
//!    （零信息量、且与本笔 raw-tx 种子同源，见 `sync_events`）。提取通道是否
//!    静默失效由 `observe_block` 按块边界自证：连续缺种子超阈值发一次 `error`，
//!    不做逐块 RPC 重拉。
//! 3. **L2 — 周期快照（唯一兜底）**：`start_elfomo_prop_sync_task`（45s）低频
//!    重拉整档回正 + 种子 + vault `balanceOf`，覆盖断流/重连/漏块等极端场景；
//!    AsyncUpdate/Resync 走 `execute_elfomo_snapshot_reconcile` 同形状三段式合并。
//!
//! ### 两条通道的合并语义（改动前先读 `docs/dynamic_state_sync_principles.md` §3）
//!
//! - **vault 余额（累积量）**：由 `ledger::VaultDeltaLedger` 做
//!   `current = 快照(S) + Σ_{块>S} Δ` 的 **rebase-merge**。快照永远能落地
//!   （不再"本地水位更高就跳过"——那会把唯一纠错通道饿死），只有早于账本
//!   **锚点块** 才丢弃。余额由账本派生，负值只告警+截断、账本保留真值。
//! - **`price_seed`（最新值）**：字段级水位 `price_seed_block` 保鲜。
//!   raw-tx 种子来自 flashblock 乐观头，快照读规范头，故只在
//!   `snap_block >= price_seed_block` 时用快照种子覆盖；否则保留本地新种子
//!   并按它重建档位（链上档位是与快照种子绑定的，不能混用）。
//! - **profile word（最新值）**：同形态的字段级水位 `profile_block`（keeper 的
//!   `0xd4ff31bd` 也会改它）；模型自证**用快照自带的 profile** 重建对拍，
//!   避免"本地 profile 已更新、快照落后"时的假阴性误停报价。

use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{address, Address, B256, U256},
    providers::Provider,
    rpc::types::Log,
    sol,
};
use serde::{Deserialize, Serialize};
use tracing::{error, info, instrument, warn};

use crate::amms::{
    amm::{AutomatedMarketMaker, SyncAction, AMM},
    error::AMMError,
    Token,
};

use crate::amms::elfomo_prop::types::{
    ElfomoBandProfile, ElfomoLadderConfig, OrderbookLevel, OrderbookSnapshot,
};

pub mod factory;
pub mod ledger;
pub mod types;

use self::ledger::{VaultDeltaLedger, VaultLedgerApply};

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
/// **不是状态源**：仅 topic0、data 0 字节；价格种子在同笔 raw-tx calldata 里。
/// 只保留给 flashblocks 提取侧做去重（有 raw-tx 时剔除同块该空日志）。
pub const ELFOMO_UPDATE_EVENT: B256 = B256::new([
    0xc5, 0xd0, 0x8c, 0xbe, 0x6f, 0xd3, 0xeb, 0xc2, 0x4e, 0x5a, 0x48, 0x36, 0x16, 0xdd, 0xdb, 0xc6,
    0x3b, 0x2a, 0xff, 0x5c, 0x08, 0x2c, 0x7d, 0x69, 0x76, 0x03, 0xab, 0x52, 0x10, 0x79, 0xf8, 0x09,
]);

/// Pool `updatePrices(uint256)` selector（flashblocks raw-tx 主通道用）
pub const ELFOMO_UPDATE_SELECTOR: [u8; 4] = [0xae, 0x7e, 0x8d, 0x81];

/// keeper 逐档 profile 改写 selector：`0xd4ff31bd(uint256[] keys, uint256[] words)`。
///
/// 链上实证（XLayer 块 `70352700`）：该调用改写 slot0 `mapping(pairIdx => profileWord)`，
/// 同样只 emit 空事件 `ELFOMO_UPDATE_EVENT`，信息全在 calldata。
pub const ELFOMO_BAND_PROFILE_SELECTOR: [u8; 4] = [0xd4, 0xff, 0x31, 0xbd];

/// 单笔 `0xd4ff31bd` 允许携带的 profile 改写条数上限（防御异常 calldata 造成巨量分配）。
pub const ELFOMO_MAX_BAND_UPDATES: usize = 64;

/// `getSupportedPairs()` 找不到本 pair 时，兜底扫描的 profile 存储键范围。
/// 正常路径不会用到（key = pair 下标）；只在链上 pair 列表异常时保证仍能自愈。
pub const ELFOMO_PROFILE_KEY_SCAN: u64 = 8;

// ----------------------------------------------------------------------------
// storage 读取专用 HTTP RPC（处理方式与 caliber_prop 一致）
// ----------------------------------------------------------------------------

/// 存储读取块高校验（与 `caliber_prop::ensure_storage_block_available` 同一逻辑）。
///
/// elfomo 的 storage 读取（Pool slot1 价格种子）经 `eth_call` bulk-SLOAD 走
/// 调用方注入的 provider（见 [`crate::amms::evm_storage`]）；该节点头部可能落后于
/// 调用方传入的块高（如 maintenance Resync / coverage 传入的 canonical 头），
/// 直接查询会触发 `-32019 block is out of range`。超前块**不降级读取**，返回
/// `BlockNotAvailable`，由 maintenance 层把任务留在队列重试，直到节点收录该块后
/// 再按原块读取。历史块（≤ 头部）原样保留；头部查询失败时按原块继续（存储读取
/// 失败走既有错误重试路径）。
async fn ensure_storage_block_available<N, P>(
    provider: &P,
    block: BlockId,
) -> Result<BlockId, AMMError>
where
    N: Network,
    P: Provider<N>,
{
    let BlockId::Number(alloy::eips::BlockNumberOrTag::Number(num)) = block else {
        return Ok(block);
    };
    match provider.get_block_number().await {
        Ok(head) if head < num => Err(AMMError::BlockNotAvailable {
            requested_block: num,
            storage_head: head,
        }),
        _ => Ok(block),
    }
}

/// 价格定点基（1e24 = 0xD3C21BCECCEDA1000000，64-bit limbs 小端）
const ONE_E24: U256 = U256::from_limbs([0x1bcecceda1000000, 0xd3c2, 0, 0]);

// ----------------------------------------------------------------------------
// orderbook 生成公式常量（真实链 10 块 + anvil vault 全量扫描复验，见模块文档）
// ----------------------------------------------------------------------------

// 说明：orderbook 的档位宽度/偏离不是全局常量——`PREFIX`/`D` 是协议级常量，
// 而 `unit/band_count/spread_level/spread_penalty` 是**每 pool 的链上配置**，
// 由 `init` 逐池从 `Pool.getMetadata(asset)` 读取（见 `types::ElfomoLadderConfig`），
// 代码里没有任何 pair 特判。生成规则见 `build_orderbook_with`。
// ----------------------------------------------------------------------------

/// 覆盖率自证阈值：连续这么多块没有拿到 raw-tx 种子 → 说明主通道可能失效，
/// 发一次 error 告警（零 RPC；收敛由周期对账负责，不再自动重拉）。
pub const ELFOMO_SEED_COVERAGE_ALERT_BLOCKS: u64 = 5;

/// 价格种子位域掩码（与链上 `0x561fa97d` 内部一致）
const SEED_Q_MASK: u64 = 0x3f;
const SEED_LOW_MASK: u64 = 0x3f_ffff;

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IElfomoVault {
        function balanceOf(address account) external view returns (uint256);
    }

    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IERC20Metadata {
        function decimals() external view returns (uint8);
        function symbol() external view returns (string);
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
    /// 价格种子的**字段级水位**：种子上次被写入的块号。
    ///
    /// raw-tx 通道的种子来自 flashblock 乐观头（领先规范头 1~3 块），而快照读的是
    /// 规范头 → 快照落地时若种子的水位更新，必须保留本地种子（否则会把已应用的
    /// 新种子回退成旧种子，产生旧价报价窗口）。见 principles §3 规则 1。
    #[serde(default)]
    pub price_seed_block: u64,
    /// 逐档 profile word（宽度/偏离表）的**字段级水位**：上次被写入的块号。
    ///
    /// 与 `price_seed_block` 完全同形态——raw-tx（`0xd4ff31bd` calldata）来自
    /// flashblock 乐观头，快照读规范头；两者各自按字段水位保鲜，互不覆盖。
    #[serde(default)]
    pub profile_block: u64,
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
    /// 订单簿快照（两侧档位 + 金库余额 + 价格种子；档数由 ladder 决定）
    pub levels: OrderbookSnapshot,
    /// **本 pair 的 ladder 配置**（深度/斜率表；链上是 per-asset 存储，
    /// 本地按 pair 持有）。缺省即已知 pair 的 ladder，见 [`ElfomoLadderConfig`]。
    #[serde(default)]
    pub ladder: ElfomoLadderConfig,
    /// **模型自证**：最近一次"链上档位 vs 本地 `build_orderbook` 重算"对拍是否一致。
    ///
    /// 链上 orderbook 是 `(seed, vault)` 的纯函数，本地同构重算；一旦对拍不一致
    /// （协议改了参数、接入的新 pair 深度表不同），说明本地模型不再可信 → 置 false，
    /// 报价路径直接拒绝该池，绝不输出错价；下一次对拍一致即自动恢复。
    /// `serde(default)` 对旧状态取 true（默认信任已逐位对拍过的模型）。
    #[serde(default = "default_model_verified")]
    pub model_verified: bool,
    /// 覆盖率自证：最后一次**成功应用 raw-tx 种子**的块号。
    ///
    /// 与 `price_seed_block` 分开：后者会被快照对账（45s）推进，会掩盖"raw-tx
    /// 提取通道失效"这一真正要观测的事实。
    #[serde(default)]
    pub raw_seed_block: u64,
    /// 覆盖率自证：块边界观测到的"已收口"块号（flashblocks `index==0` 推进）。
    #[serde(default)]
    pub coverage_open_block: u64,
    /// 覆盖率自证：已收口的、无 raw-tx 种子的连续块数。
    #[serde(default)]
    pub blocks_since_seed: u64,
    /// 覆盖率告警是否已发出（避免刷屏；下一个健康块收口时清零）。
    #[serde(default)]
    pub seed_coverage_alerted: bool,
    /// vault 余额增量账本（checkpoint + redo log）。
    ///
    /// **活池的 `levels.vault_xeth/vault_usdt0` 由此账本派生**：事件记账、
    /// 快照落地走 `rebase`（`快照(S) + Σ_{块 > S}`），因此"快照读数落后于
    /// 本地事件块"不再需要跳过快照（那是会把唯一纠错通道饿死的反模式）。
    /// `simulate_swap_mut` 只作用于深拷贝工作副本，不进账本。
    #[serde(default)]
    pub vault_ledger: VaultDeltaLedger,
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
            price_seed_block: 0,
            profile_block: 0,
            tokens: Vec::new(),
            levels: OrderbookSnapshot::default(),
            ladder: ElfomoLadderConfig::default(),
            model_verified: true,
            raw_seed_block: 0,
            coverage_open_block: 0,
            blocks_since_seed: 0,
            seed_coverage_alerted: false,
            vault_ledger: VaultDeltaLedger::new(),
        }
    }
}

/// `model_verified` 的 serde 默认值（旧序列化状态无该字段时视为已验证）。
fn default_model_verified() -> bool {
    true
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
            price_seed_block: 0,
            profile_block: 0,
            tokens: Vec::new(),
            levels: OrderbookSnapshot::default(),
            ladder: ElfomoLadderConfig::default(),
            model_verified: true,
            raw_seed_block: 0,
            coverage_open_block: 0,
            blocks_since_seed: 0,
            seed_coverage_alerted: false,
            vault_ledger: VaultDeltaLedger::new(),
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

    /// 从 keeper `0xd4ff31bd(uint256[] keys, uint256[] words)` calldata
    /// 解析逐档 profile 改写（`keys[i]` = pair 下标，`words[i]` = 新 profile word）。
    ///
    /// 该交易**只 emit 那条空事件**（`ELFOMO_UPDATE_EVENT`），信息全在 calldata 里，
    /// 所以这是 profile 实时更新的唯一零 RPC 通道（与价格种子同构）。
    ///
    /// 解析失败（selector 不符 / 长度/偏移越界 / 两数组长度不等）→ `None`。
    pub fn parse_band_profile_calldata(input: &[u8]) -> Option<Vec<(U256, U256)>> {
        if input.len() < 4 + 64 || input[..4] != ELFOMO_BAND_PROFILE_SELECTOR {
            return None;
        }
        let body = &input[4..];
        let word_at = |off: usize| -> Option<U256> {
            let end = off.checked_add(32)?;
            if end > body.len() {
                return None;
            }
            Some(U256::from_be_slice(&body[off..end]))
        };
        let as_usize = |v: U256| -> Option<usize> { usize::try_from(v).ok() };
        // header: keys_off, words_off（相对 body 起点，ABI 标准）
        let keys_off = as_usize(word_at(0)?)?;
        let words_off = as_usize(word_at(32)?)?;
        let keys_len = as_usize(word_at(keys_off)?)?;
        let words_len = as_usize(word_at(words_off)?)?;
        if keys_len != words_len || keys_len > ELFOMO_MAX_BAND_UPDATES {
            return None;
        }
        let mut out = Vec::with_capacity(keys_len);
        for i in 0..keys_len {
            let key = word_at(keys_off + 32 * (i + 1))?;
            let word = word_at(words_off + 32 * (i + 1))?;
            out.push((key, word));
        }
        Some(out)
    }

    /// profile word 的存储槽：`keccak256(pad32(key)‖pad32(0))`
    /// （`mapping(uint256=>uint256)` 在 slot 0）。
    pub fn band_profile_slot(key: u64) -> B256 {
        let mut buf = [0u8; 64];
        buf[24..32].copy_from_slice(&key.to_be_bytes());
        alloy::primitives::keccak256(buf)
    }

    /// 按指定 pool ladder + 种子 + 金库余额生成 orderbook（纯函数，与链上逐位一致）。
    ///
    /// **这是 ladder 生成规则在本地唯一的实现**：`(U, T, F, spread_level, vault) → 档位表`
    /// 的读时纯函数。规则由 2026-09-11 fork 全网格（30,240 组）逐位实证得出，见
    /// [`ElfomoLadderConfig`]；本地一切报价路径都必须走 [`Self::local_orderbook`]。
    ///
    /// **注意：这套规则（含 `PREFIX_WIDTHS` / `DEVIATIONS`）是 XLayer 部署的实证**。
    /// Base 等链是同一协议的**另一套部署变体**（`getMetadata` word 的 `f3` = 1、偏离表
    /// 前两档不同、种子槽位不同），当前**未支持**——在那些链上模型自证会判不可信并
    /// fail-closed（不报价）。扩展步骤见 [`ElfomoLadderConfig`] 顶部的多链说明。
    pub fn build_orderbook_with(
        ladder: &ElfomoLadderConfig,
        seed: U256,
        vault_usdt0: U256,
        vault_xeth: U256,
    ) -> OrderbookSnapshot {
        let profile_word = ladder.profile.encode_word();
        let empty = || OrderbookSnapshot {
            from_to_levels: Vec::new(),
            to_from_levels: Vec::new(),
            vault_usdt0,
            vault_xeth,
            price_seed: seed,
            profile_word,
        };

        // q 取位 22..27、low 取位 0..21，只需种子低 27 位
        let low_bits = (seed & U256::from(0x7ff_ffffu64)).to::<u64>();
        let q = (low_bits >> 22) & SEED_Q_MASK;
        let qs = if q >= 32 { q as i64 - 64 } else { q as i64 };
        let low = low_bits & SEED_LOW_MASK;
        let base = U256::from((100_000i64 + qs) as u64) * U256::from(low);
        if !ladder.is_valid() || base.is_zero() {
            // ladder 缺失（未 init / 配置错误）→ 不给任何档位，模型自证判不可信
            return empty();
        }

        let from_to_levels = Self::build_from_to_levels(ladder, base, vault_usdt0, vault_xeth);
        let to_from_levels = Self::build_to_from_levels(ladder, base, vault_xeth);

        OrderbookSnapshot {
            from_to_levels,
            to_from_levels,
            vault_usdt0,
            vault_xeth,
            price_seed: seed,
            profile_word,
        }
    }

    /// 前缀宽度表逐档截断到容量 `cap`（不足一整档时给残余档）。
    ///
    /// 宽度表 `widths` 来自链上 profile word（**动态存储**，见 [`ElfomoBandProfile`]），
    /// 只取前 `count` 档；不再有写死的常量表。
    fn prefix_widths(cap: U256, unit: U256, profile: &ElfomoBandProfile) -> Vec<U256> {
        let n = (profile.count as usize).min(profile.widths.len());
        let mut out = Vec::with_capacity(n);
        let mut cumulative = U256::ZERO;
        for w in profile.widths[..n].iter().copied() {
            let width = U256::from(w) * unit;
            if cumulative.saturating_add(width) <= cap {
                out.push(width);
                cumulative += width;
            } else {
                if cap > cumulative {
                    out.push(cap - cumulative);
                }
                break;
            }
        }
        out
    }

    /// 第 `i` 档偏离量：`profile.deviations[i]`（链上动态表），整梯加宽时再加 `F`。
    ///
    /// 实测规则（2026-09-11 全网格实证）：加宽时 **`i >= 1` 的所有档位 +`F`**；
    /// 前缀只有 1 档时该档（`i == 0`）同样 +`F`（唯一档位等价于末档）。
    fn prefix_deviation(
        i: usize,
        prefix_len: usize,
        penalty: u64,
        widened: bool,
        profile: &ElfomoBandProfile,
    ) -> u64 {
        let mut dev = profile.deviations.get(i).copied().unwrap_or(0);
        if widened && (i >= 1 || prefix_len == 1) {
            dev += penalty;
        }
        dev
    }

    /// 单侧是否进入"加宽"档：**由金库余额决定，不是静态配置**。
    ///
    /// 实测（全网格 bit-exact）：`from→to` 侧容量 `cap = (T−1)·U − vault_xeth`、
    /// `to→from` 侧容量即 `vault_xeth`；当 `spread_level · U >= 容量` 时该侧所有档位加宽
    /// `F`。真实池 `T=30, spread_level=5, U=0.6e18`：`vault_xeth ≈ 3.68U` 时 `to→from`
    /// 已加宽、`from→to` 未加宽——静态判据（旧 `spread_level >= T−1`）在此会算错价格。
    fn side_widened(spread_level: u16, unit: U256, capacity: U256) -> bool {
        U256::from(spread_level) * unit >= capacity
    }

    /// from→to 侧：maker 补库存方向，容量受 `vault_xeth` 约束、输出受 `vault_usdt0` 约束。
    ///
    /// 前缀容量 `cap = (T−1)·U − vault_xeth`：
    /// - `cap > 0`：按 `PREFIX` 逐档截断到 `cap`，尾部再补一档 `5T·U`；逐档以剩余
    ///   `vault_usdt0` 预算封顶（`size = min(rem/price, width)`），消耗按
    ///   `ceil(size·price / 1e24)` 扣减；
    /// - `cap == 0`（含 `vault_xeth >= (T−1)·U`）：链上只给**一档**残余档
    ///   `min(T·U − vault_xeth, 预算)`，价恒为尾档价（`5T·U` 那一档的斜率），超出
    ///   `T·U` 则完全不给档位。
    fn build_from_to_levels(
        ladder: &ElfomoLadderConfig,
        base: U256,
        vault_usdt0: U256,
        vault_xeth: U256,
    ) -> Vec<OrderbookLevel> {
        let unit = ladder.unit;
        let band_count = U256::from(ladder.band_count);
        let penalty = u64::from(ladder.spread_penalty);
        let total = band_count * unit; // T·U
        let full = total - unit; // (T−1)·U
        let tail_price = U256::from(ElfomoLadderConfig::FT_TAIL_SLOPE) * base;
        let mut rem = vault_usdt0.saturating_mul(ONE_E24);
        let mut levels = Vec::new();

        let cap = full.saturating_sub(vault_xeth);
        if cap.is_zero() {
            // 库存已达/超过目标上限：只有一档 `T·U − vault_xeth` 的尾档价残余档。
            let width = total.saturating_sub(vault_xeth);
            if width.is_zero() || tail_price.is_zero() {
                return levels;
            }
            let size = (rem / tail_price).min(width);
            if !size.is_zero() {
                levels.push(OrderbookLevel::new(size, tail_price));
            }
            return levels;
        }

        let widened = Self::side_widened(ladder.spread_level, unit, cap);
        let prefix = Self::prefix_widths(cap, unit, &ladder.profile);
        let mut widths = prefix.clone();
        widths.push(U256::from(5u64) * band_count * unit);

        for (i, width) in widths.iter().enumerate() {
            let slope = if i < prefix.len() {
                ElfomoLadderConfig::SLOPE_BASE
                    - Self::prefix_deviation(i, prefix.len(), penalty, widened, &ladder.profile)
            } else {
                ElfomoLadderConfig::FT_TAIL_SLOPE
            };
            let price = U256::from(slope) * base;
            if price.is_zero() {
                break;
            }
            let budget_cap = rem / price;
            let size = budget_cap.min(*width);
            if size.is_zero() {
                break;
            }
            levels.push(OrderbookLevel::new(size, price));
            rem = rem.saturating_sub(Self::ceil_div(size * price, ONE_E24).saturating_mul(ONE_E24));
            if size < *width {
                break;
            }
        }
        levels
    }

    /// to→from 侧：maker 减库存方向，容量受 `vault_xeth` 约束。
    ///
    /// 前缀容量 `vault_xeth − U`（不足一档时给残余档，`vault_xeth < U` 时前缀为空），
    /// 尾部再补一档 `U`；逐档以剩余 `vault_xeth` 封顶。`vault_xeth` 为 0 时不给档位。
    fn build_to_from_levels(
        ladder: &ElfomoLadderConfig,
        base: U256,
        vault_xeth: U256,
    ) -> Vec<OrderbookLevel> {
        let unit = ladder.unit;
        let penalty = u64::from(ladder.spread_penalty);
        let mut rem = vault_xeth;
        let mut levels = Vec::new();
        if rem.is_zero() {
            return levels;
        }

        let widened = Self::side_widened(ladder.spread_level, unit, vault_xeth);
        let prefix = Self::prefix_widths(vault_xeth.saturating_sub(unit), unit, &ladder.profile);
        let mut widths = prefix.clone();
        widths.push(unit);

        for (i, width) in widths.iter().enumerate() {
            let slope = if i < prefix.len() {
                ElfomoLadderConfig::SLOPE_BASE
                    + Self::prefix_deviation(i, prefix.len(), penalty, widened, &ladder.profile)
            } else {
                ElfomoLadderConfig::TF_TAIL_SLOPE
            };
            let price = U256::from(slope) * base;
            if price.is_zero() {
                break;
            }
            let size = rem.min(*width);
            if size.is_zero() {
                break;
            }
            levels.push(OrderbookLevel::new(size, price));
            rem -= size;
            if size < *width {
                break;
            }
        }
        levels
    }

    /// 用**本池 ladder** + 当前种子 + 金库余额重算 orderbook（读时重算模型）。
    fn local_orderbook(
        &self,
        seed: U256,
        vault_usdt0: U256,
        vault_xeth: U256,
    ) -> OrderbookSnapshot {
        Self::build_orderbook_with(&self.ladder, seed, vault_usdt0, vault_xeth)
    }

    /// 用当前种子 + 金库余额重建 orderbook 缓存（读时重算模型下的缓存刷新）。
    fn refresh_levels(&mut self) {
        self.levels = self.local_orderbook(
            self.price_seed,
            self.levels.vault_usdt0,
            self.levels.vault_xeth,
        );
    }

    /// 确保 vault 账本已锚定（正常路径由 `init`/快照落地完成）。
    ///
    /// 防御分支：若事件早于任何 abs 快照到达（如从序列化状态恢复、账本字段
    /// 为新增 default），则以**当前本地余额**为基底锚定。
    ///
    /// 基底块号取 `last_synced_block`——它正是"本地余额成立到哪一块"的语义，
    /// 也是后续 `rebase` 陈旧判据的正确标尺（取 `block - 1` 会把基底虚标得更新，
    /// 误杀本可用的中间块快照）。块号 0（无块级语义）时才退回 `block - 1`。
    ///
    /// 若该基底已覆盖本笔（`block <= base_block`），`record_trade` 会拒收并在
    /// `sync()` 留 debug 痕；漏账/多计都会在下一次快照 rebase 时对齐链上真值。
    fn ensure_vault_ledger_anchored(&mut self, block: u64) {
        if self.vault_ledger.is_anchored() {
            return;
        }
        let base_block = if self.last_synced_block != 0 {
            self.last_synced_block
        } else {
            block.saturating_sub(1)
        };
        self.vault_ledger
            .anchor(base_block, self.levels.vault_xeth, self.levels.vault_usdt0);
    }

    /// 由账本派生 vault 余额（活池余额的唯一写入点）。
    ///
    /// 有符号视图为负 = 本地账本与链上已不一致（漏帧/重放），**必须告警**
    /// 而不是静默截断；截断只作用于物化的 `levels.vault_*`，账本保留真值，
    /// 下轮快照 rebase 即可回正。
    fn sync_vault_balances_from_ledger(&mut self) {
        let (signed_xeth, signed_usdt0) = self.vault_ledger.current_signed();
        if signed_xeth < 0 || signed_usdt0 < 0 {
            warn!(
                target: "amms::elfomo_prop",
                pool = %self.pool_address,
                vault_xeth_signed = signed_xeth,
                vault_usdt0_signed = signed_usdt0,
                anchor_block = self.vault_ledger.anchor_block(),
                "elfomofi: vault ledger went negative (local drift); clamping to 0, awaiting snapshot rebase"
            );
        }
        let (vault_xeth, vault_usdt0) = self.vault_ledger.current();
        self.levels.vault_xeth = vault_xeth;
        self.levels.vault_usdt0 = vault_usdt0;
    }

    /// 模型未通过链上对拍时拒绝报价（宁可不出价，也不出错价）。
    fn ensure_model_verified(&self) -> Result<(), AMMError> {
        if self.model_verified {
            return Ok(());
        }
        Err(AMMError::Msg(format!(
            "elfomofi: local orderbook model not verified against chain for pool {}; quotes disabled",
            self.pool_address
        )))
    }

    /// 模型自证：把链上返回的档位与本地 `build_orderbook` 的**逐位**重算结果对拍。
    ///
    /// 两者不一致 = 本地模型已与链上脱节（协议改参数 / 新 pair 深度表不同 /
    /// 公式逆向有误），此时任何本地报价都不可信 → `model_verified=false`，
    /// 报价路径拒绝该池；下一次对拍一致会自动恢复（周期对账 45s 一次）。
    #[allow(clippy::too_many_arguments)]
    fn verify_model_against_chain(
        &mut self,
        chain_from_to: &[OrderbookLevel],
        chain_to_from: &[OrderbookLevel],
        seed: U256,
        vault_usdt0: U256,
        vault_xeth: U256,
        profile: &ElfomoBandProfile,
        source: &str,
        block: u64,
    ) -> bool {
        // **必须用快照自带的 profile** 重建（而不是本地的 `self.ladder.profile`）：
        // 链上档位是"那一刻的 profile + seed + 余额"的纯函数；快照读规范头、
        // raw-tx 读乐观头，本地 profile 可能已经更新。用本地 profile 对拍会在
        // 快照落后时误判模型不可信（假阴性）并错误地停掉报价。
        let mut snapshot_ladder = self.ladder;
        snapshot_ladder.profile = *profile;
        let local = Self::build_orderbook_with(&snapshot_ladder, seed, vault_usdt0, vault_xeth);
        let matched =
            local.from_to_levels == chain_from_to && local.to_from_levels == chain_to_from;
        if matched {
            if !self.model_verified {
                info!(
                    target: "amms::elfomo_prop",
                    pool = %self.pool_address, source, block,
                    "elfomofi: local orderbook model re-verified against chain; quotes re-enabled"
                );
                self.model_verified = true;
            }
        } else {
            if self.model_verified {
                error!(
                    target: "amms::elfomo_prop",
                    pool = %self.pool_address, source, block,
                    chain_from_to = chain_from_to.len(),
                    chain_to_from = chain_to_from.len(),
                    local_from_to = local.from_to_levels.len(),
                    local_to_from = local.to_from_levels.len(),
                    "elfomofi: local orderbook model MISMATCH vs chain — quotes disabled \
                     until re-verified (new pair with an un-reversed depth table?)"
                );
            }
            self.model_verified = false;
        }
        matched
    }

    /// 覆盖率自证：**块边界**观测（flashblocks `index == 0` 时由应用层驱动）。
    ///
    /// 为什么按块收口而不是按 slice：一个 XLayer 块会被拆成多个 flashblock
    /// payload 先后到达，携带种子的 `updatePrices` 交易可能落在任意 slice 上，
    /// 所以在 slice 粒度判定"本块没种子"会误报。这里在观察到块 B 时结算
    /// `coverage_open_block`（即上一块）的覆盖情况：
    /// - 上一块有 raw-tx 种子（`raw_seed_block >= open`）→ 计数器清零、告警位复位；
    /// - 没有 → 计 1 块缺失；中间被整段跳过的块（流重连）按缺失计（保守）。
    ///
    /// 零 RPC：只依赖本地块号与 [`Self::raw_seed_block`]。
    pub fn observe_block(&mut self, block: u64) {
        if block <= self.coverage_open_block {
            return;
        }
        let open = self.coverage_open_block;
        if open != 0 {
            let skipped_between = block.saturating_sub(open + 1);
            let had_seed = self.raw_seed_block >= open;
            let mut missing = skipped_between;
            if !had_seed {
                missing += 1;
            }
            if missing > 0 {
                self.blocks_since_seed = self.blocks_since_seed.saturating_add(missing);
                if self.blocks_since_seed >= ELFOMO_SEED_COVERAGE_ALERT_BLOCKS
                    && !self.seed_coverage_alerted
                {
                    self.seed_coverage_alerted = true;
                    error!(
                        target: "amms::elfomo_prop",
                        pool = %self.pool_address,
                        block,
                        blocks_since_seed = self.blocks_since_seed,
                        last_raw_seed_block = self.raw_seed_block,
                        "elfomofi: raw-tx price seed missing for {} consecutive blocks — \
                         flashblocks extraction likely broken (quotes drift with the stale seed; \
                         the 45s reconcile only repairs vault balances, not the price)",
                        self.blocks_since_seed
                    );
                }
            } else {
                self.blocks_since_seed = 0;
                self.seed_coverage_alerted = false;
            }
        }
        self.coverage_open_block = block;
    }

    /// 本池的种子覆盖状态（观测用）：`(最后一次 raw-tx 种子的块号, 连续缺失块数)`。
    pub fn seed_coverage(&self) -> (u64, u64) {
        (self.raw_seed_block, self.blocks_since_seed)
    }

    /// 针对给定 orderbook 快照报价（纯函数；链上对拍/复用用）。
    ///
    /// 语义与 `simulate_swap` 完全一致（金库封顶），但使用外部传入的
    /// orderbook —— 用于"父块金库余额 + raw-tx 种子"这类跨状态对拍。
    pub fn simulate_swap_for_orderbook(
        ob: &OrderbookSnapshot,
        token_x: Address,
        token_y: Address,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> U256 {
        if token_in == token_x && token_out == token_y {
            Self::quote_fwd_exact(&ob.from_to_levels, amount_in, ob.vault_usdt0)
        } else if token_in == token_y && token_out == token_x {
            Self::quote_rev_exact(&ob.to_from_levels, amount_in, ob.vault_xeth)
        } else {
            U256::ZERO
        }
    }

    /// 应用 `updatePrices` raw-tx 解析出的价格种子（本地直算，零 RPC）。
    ///
    /// - 设置 `price_seed`，按**当前本地金库余额**重算整本 orderbook；
    /// - 档位随余额自动缩放（链上读时动态计算，同构）；
    /// - **水位保鲜**：仅当 `block_number >= price_seed_block` 时才写入种子并把
    ///   水位推进到该块（规则 1：水位必须被"读"，只写不读会让旧块/重放的
    ///   raw-tx 把新种子回退、水位却反被 stamp 到更高块号，自相矛盾并骗过
    ///   `merge_snapshot` 的 `seed_fresh` 闸门）。同块多笔由调用方按 `tx_index`
    ///   排序，最后一笔赢。
    /// - 单调推进 `last_synced_block`。
    pub fn apply_price_seed(&mut self, seed: U256, block_number: u64) {
        if block_number >= self.price_seed_block {
            self.price_seed = seed;
            self.price_seed_block = block_number;
            // 覆盖率自证：记录"raw-tx 通道本块有货"（收口判定在块边界做，
            // 因为同块多个 flashblock slice 都可能携带种子）。
            self.raw_seed_block = self.raw_seed_block.max(block_number);
        } else {
            tracing::debug!(
                target: "amms::elfomo_prop",
                pool = %self.pool_address,
                block_number,
                price_seed_block = self.price_seed_block,
                "elfomofi: stale price seed ignored"
            );
        }
        self.refresh_levels();
        self.last_synced_block = self.last_synced_block.max(block_number);
    }

    /// 应用 keeper `0xd4ff31bd` raw-tx 解析出的逐档 profile word（本地直算，零 RPC）。
    ///
    /// profile（宽度表 + 偏离表）与价格种子一样是**动态链上状态**：keeper 会改
    /// （2026-09-11 块 `70352700` 实证偏离表 `[7,10,15,25,40,50]→[15,20,25,35,45,60]`）。
    /// 这里与 [`Self::apply_price_seed`] 完全同形态：
    /// - 解码 word → 更新 `ladder.profile`，按当前 (seed, 余额) 重算 orderbook；
    /// - **字段级水位** `profile_block` 保鲜（旧块/重放不得回退，同块多笔由调用方按
    ///   `tx_index` 排序，最后一笔赢）；
    /// - word 解码非法（档数 0/超上限）→ 告警并忽略，保持原 profile。
    pub fn apply_band_profile(&mut self, word: U256, block_number: u64) {
        if block_number < self.profile_block {
            tracing::debug!(
                target: "amms::elfomo_prop",
                pool = %self.pool_address,
                block_number,
                profile_block = self.profile_block,
                "elfomofi: stale band profile ignored"
            );
            return;
        }
        match ElfomoBandProfile::decode_word(word) {
            Some(profile) => {
                self.ladder.profile = profile;
                self.profile_block = block_number;
                self.refresh_levels();
                self.last_synced_block = self.last_synced_block.max(block_number);
            }
            None => warn!(
                target: "amms::elfomo_prop",
                pool = %self.pool_address,
                block_number,
                %word,
                "elfomofi: invalid band profile word from raw-tx; keeping previous profile"
            ),
        }
    }

    /// 正向 exact-in：from→to（size = 输入量），`out += floor(take×price/1e24)`，
    /// 封顶 `min(总输出, vault_usdt0)`。
    fn quote_fwd_exact(levels: &[OrderbookLevel], amount_in: U256, vault_usdt0: U256) -> U256 {
        let mut out = U256::ZERO;
        let mut rem = amount_in;
        for lv in levels.iter() {
            if rem.is_zero() {
                break;
            }
            let take = rem.min(lv.size);
            out += take * lv.price / ONE_E24;
            rem -= take;
        }
        out.min(vault_usdt0)
    }

    /// 反向 exact-in：to→from（size = 输出量），
    /// `need = ceil(size×price/1e24)`；`剩余≥need` 时 `out+=size`，
    /// 否则 `out += floor(剩余×1e24/price)`；封顶 `min(out, vault_xeth)`。
    fn quote_rev_exact(levels: &[OrderbookLevel], amount_in: U256, vault_xeth: U256) -> U256 {
        let mut out = U256::ZERO;
        let mut rem = amount_in;
        for lv in levels.iter() {
            if rem.is_zero() {
                break;
            }
            if lv.size.is_zero() {
                continue;
            }
            let need = Self::ceil_div(lv.size * lv.price, ONE_E24);
            if rem >= need {
                out += lv.size;
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
    fn quote_fwd_exact_out(levels: &[OrderbookLevel], amount_out: U256, vault_usdt0: U256) -> U256 {
        let mut level_out = Vec::with_capacity(levels.len());
        let mut cap = U256::ZERO;
        for lv in levels.iter() {
            let o = lv.size * lv.price / ONE_E24;
            level_out.push(o);
            cap += o;
        }
        let cap = cap.min(vault_usdt0);
        if amount_out > cap {
            return U256::ZERO;
        }
        let mut amount_in = U256::ZERO;
        let mut rem = amount_out;
        for (lv, o) in levels.iter().zip(level_out.iter()) {
            if rem.is_zero() {
                break;
            }
            if lv.size.is_zero() {
                continue;
            }
            if rem >= *o {
                amount_in += lv.size;
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
    fn quote_rev_exact_out(levels: &[OrderbookLevel], amount_out: U256, vault_xeth: U256) -> U256 {
        if amount_out > vault_xeth {
            return U256::ZERO;
        }
        let mut amount_in = U256::ZERO;
        let mut rem = amount_out;
        for lv in levels.iter() {
            if rem.is_zero() {
                break;
            }
            if lv.size.is_zero() {
                continue;
            }
            if rem >= lv.size {
                amount_in += Self::ceil_div(lv.size * lv.price, ONE_E24);
                rem -= lv.size;
            } else {
                amount_in += Self::ceil_div(rem * lv.price, ONE_E24);
                break;
            }
        }
        amount_in
    }

    // ------------------------------------------------------------------------
    // L2：整档回正（快照/init 通道；无"本地档位消耗"这一层状态）
    // ------------------------------------------------------------------------

    /// 应用订单簿快照（init / 快照通道：整档回正 + 金库余额 + 价格种子 + profile）。
    #[allow(clippy::too_many_arguments)]
    pub fn apply_orderbook_snapshot(
        &mut self,
        from_to_levels: Vec<OrderbookLevel>,
        to_from_levels: Vec<OrderbookLevel>,
        vault_usdt0: U256,
        vault_xeth: U256,
        price_seed: U256,
        profile_word: U256,
        block_number: u64,
    ) {
        // 仅限 init / 空账本：更旧的块号会让账本锚点与水位回退，静默抹掉 P2 收益。
        if block_number != 0 && block_number < self.vault_ledger.anchor_block() {
            warn!(
                target: "amms::elfomo_prop",
                pool = %self.pool_address,
                block_number,
                anchor_block = self.vault_ledger.anchor_block(),
                "elfomofi: stale orderbook snapshot ignored (older than ledger anchor)"
            );
            return;
        }
        // 种子 / profile 同样按字段级水位保鲜（与 `apply_price_seed` /
        // `apply_band_profile` / `merge_snapshot` 同形态）：本地已有的更新值不得回退。
        // 链上档位数组是绑定快照 (profile, seed, 余额) 的，任一被本地更新取代，
        // 都必须按本地值重建，不能直接采用。
        let seed_fresh = block_number >= self.price_seed_block;
        // 快照自带的 profile：解码后既用于落地，也作为模型自证的 profile 基准
        // （链上档位是那一刻 profile 的纯函数，不能用可能更新的本地 profile 对拍）。
        let snapshot_profile_opt = if profile_word.is_zero() {
            None
        } else {
            ElfomoBandProfile::decode_word(profile_word)
        };
        if !profile_word.is_zero() && snapshot_profile_opt.is_none() {
            warn!(
                target: "amms::elfomo_prop",
                pool = %self.pool_address,
                block_number,
                %profile_word,
                "elfomofi: invalid band profile word in orderbook snapshot; keeping local profile"
            );
        }
        let snapshot_profile = snapshot_profile_opt.unwrap_or(self.ladder.profile);
        let profile_fresh = snapshot_profile_opt.is_some() && block_number >= self.profile_block;
        // 模型自证：链上档位 vs 本地 `build_orderbook` 逐位对拍（不一致则拒绝报价）。
        // 放在赋值之前，借用的是入参而不是 `self.levels`（无需克隆）。
        self.verify_model_against_chain(
            &from_to_levels,
            &to_from_levels,
            price_seed,
            vault_usdt0,
            vault_xeth,
            &snapshot_profile,
            "init/orderbook-snapshot",
            block_number,
        );
        let covered_all = seed_fresh && profile_fresh;
        self.levels = OrderbookSnapshot {
            from_to_levels,
            to_from_levels,
            vault_usdt0,
            vault_xeth,
            price_seed,
            profile_word: snapshot_profile.encode_word(),
        };
        if seed_fresh {
            self.price_seed = price_seed;
            self.price_seed_block = block_number;
        } else {
            warn!(
                target: "amms::elfomo_prop",
                pool = %self.pool_address,
                block_number,
                price_seed_block = self.price_seed_block,
                "elfomofi: orderbook snapshot kept newer realtime price seed"
            );
        }
        if profile_fresh {
            self.ladder.profile = snapshot_profile;
            self.profile_block = block_number;
        }
        if !covered_all {
            self.refresh_levels();
        }
        self.last_synced_block = self.last_synced_block.max(block_number);
        // 链上绝对值 = 账本锚点（块末状态已含该块及更早的全部成交）。
        self.vault_ledger
            .anchor(block_number, vault_xeth, vault_usdt0);
    }

    /// 拉取 orderbook + vault 余额 + 价格种子快照（L2/init 兜底通道）。
    ///
    /// 注意（链上实证）：vault 是 Gnosis Safe，**不能**在 vault 合约上调用
    /// `balanceOf`；余额必须读 `token.balanceOf(vault)`。价格种子读
    /// Pool slot1（`a = slot1 >> 32`，经 `eth_call` bulk-SLOAD 走调用方注入的
    /// provider，官方 WS 网关可用），供本地 `build_orderbook` 使用。
    pub async fn fetch_orderbook_snapshot<N, P>(
        &self,
        provider: P,
        block: BlockId,
    ) -> Result<OrderbookSnapshot, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        // 存储读取块高校验（同 caliber_prop）：超前于 HTTP 节点头部时返回
        // `BlockNotAvailable`，由 maintenance 留队重试，不降级读取。
        let block = ensure_storage_block_available::<N, P>(&provider, block).await?;

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

        // 价格种子（Pool slot1 高 32 位）+ 逐档 profile word（slot0 mapping，
        // key = 本 pair 在 `getSupportedPairs()` 中的下标，见 `fetch_ladder`）。
        // 两条槽一次 bulk-SLOAD 读完，经 `eth_call` 走调用方注入的 provider
        // （官方 WS 网关不开放 eth_getStorageAt，见 `amms::evm_storage`）。
        let slots = [
            B256::from(U256::from(1u64).to_be_bytes::<32>()),
            Self::band_profile_slot(self.ladder.profile_key),
        ];
        let words = crate::amms::evm_storage::storage_slots_at::<N, P>(
            &provider,
            self.pool_address,
            &slots,
            block,
        )
        .await?;
        let price_seed = words[0] >> 32;
        let profile_word = words[1];

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
            profile_word,
        })
    }

    /// 逐池从链上读取该 pair 的 ladder：`Pool.getMetadata(asset)` 的静态参数
    /// （U/T/spread）+ 逐档 profile word（宽度/偏离表，slot0 mapping）。
    ///
    /// 这是"per-pool 自动获取"的实装：`init` 时读 11 字段配置并解码成
    /// [`ElfomoLadderConfig`]——**没有任何 pair 特判/地址常量**，接入新 pair
    /// 自动获得自己的参数。
    ///
    /// profile 的存储键由链上 `Pool.getSupportedPairs()` 现算（key = 本 pair 在下标），
    /// 与 keeper `0xd4ff31bd(uint256[] keys, …)` 用的是同一组下标；列表异常时才退回
    /// 扫描 `0..ELFOMO_PROFILE_KEY_SCAN`（取第一条合法 profile）。**绝不写死 pair 0**。
    ///
    /// 两链实测的差异：XLayer 对非 base asset 返回**全零**，Base 的 pool 对非 base asset
    /// 直接 **revert**。因此这里对每个 asset 单独容错（失败/非法就试下一个），不能把
    /// 第一个 asset 的错误当成整体失败——否则 `token_x` 恰是 quote 的 pair 会整个 init 失败。
    ///
    /// 都读不到 → `Ok(None)`，后续本地报价为空、模型自证判不可信（fail-closed），不 panic。
    pub async fn fetch_ladder<N, P>(
        &self,
        provider: P,
        block: BlockId,
    ) -> Result<Option<ElfomoLadderConfig>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        use crate::amms::elfomo_prop::types::IElfomoFiPool;

        let pool = IElfomoFiPool::new(self.pool_address, provider.clone());

        // 1) profile 存储键：本 pair 在 `getSupportedPairs()` 中的下标（pair 顺序无关）。
        let pair_index = match pool.getSupportedPairs().block(block).call().await {
            Ok(pairs) => pairs
                .iter()
                .position(|p| {
                    let (a, b) = (p[0], p[1]);
                    (a == self.token_x && b == self.token_y)
                        || (a == self.token_y && b == self.token_x)
                })
                .map(|i| i as u64),
            Err(e) => {
                tracing::debug!(
                    target: "amms::elfomo_prop",
                    pool = %self.pool_address,
                    error = %e,
                    "elfomofi: getSupportedPairs() unavailable, falling back to profile-key scan"
                );
                None
            }
        };
        let band = self
            .fetch_band_profile::<N, _>(&provider, pair_index, block)
            .await;

        for asset in [self.token_x, self.token_y] {
            let md = match pool.getMetadata(asset).block(block).call().await {
                Ok(md) => md,
                Err(e) => {
                    // 该 asset 无配置（部分部署对 quote asset 直接 revert）：换下一个
                    tracing::debug!(
                        target: "amms::elfomo_prop",
                        pool = %self.pool_address,
                        %asset,
                        error = %e,
                        "elfomofi: getMetadata(asset) unavailable, trying next asset"
                    );
                    continue;
                }
            };
            let fields = [
                U256::from(md.field0),
                U256::from(md.decimals),
                U256::from(md.field2),
                U256::from(md.field3),
                U256::from(md.field4),
                U256::from(md.unit),
                U256::from(md.band_count),
                U256::from(md.spread_level),
                U256::from(md.spread_penalty),
                U256::from_be_slice(md.field9.as_slice()),
                md.field10,
            ];
            let mut ladder = ElfomoLadderConfig::from_metadata(&fields);
            if let Some((key, profile)) = band {
                ladder.profile = profile;
                ladder.profile_key = key;
            }
            if ladder.is_valid() {
                return Ok(Some(ladder));
            }
        }
        Ok(None)
    }

    /// 读本池的逐档 profile word：优先用 `pair_index`（= `getSupportedPairs()` 下标），
    /// 取不到/非法时扫描 `0..ELFOMO_PROFILE_KEY_SCAN` 兜底。
    ///
    /// 一次 bulk `eth_call` 读完所有候选槽（零额外往返），返回 `(key, 解码后的 profile)`。
    async fn fetch_band_profile<N, P>(
        &self,
        provider: &P,
        pair_index: Option<u64>,
        block: BlockId,
    ) -> Option<(u64, ElfomoBandProfile)>
    where
        N: Network,
        P: Provider<N>,
    {
        let mut keys: Vec<u64> = Vec::with_capacity(ELFOMO_PROFILE_KEY_SCAN as usize + 1);
        if let Some(i) = pair_index {
            keys.push(i);
        }
        for k in 0..ELFOMO_PROFILE_KEY_SCAN {
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
        let slots: Vec<B256> = keys.iter().map(|k| Self::band_profile_slot(*k)).collect();
        let words = crate::amms::evm_storage::storage_slots_at::<N, P>(
            provider,
            self.pool_address,
            &slots,
            block,
        )
        .await
        .ok()?;
        for (key, word) in keys.iter().zip(words) {
            if let Some(profile) = ElfomoBandProfile::decode_word(word) {
                if profile.is_valid() {
                    return Some((*key, profile));
                }
            }
        }
        None
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

    /// 事件通道**只注册 `ElfomoTrade`**（真状态源：金库余额增量）。
    ///
    /// Pool `updatePrices` 的空事件（`ELFOMO_UPDATE_EVENT`：仅 topic0、data 0 字节）
    /// 不作为状态源——它零信息量，且与价格种子出自**同一笔交易**（种子在 calldata
    /// 里），既不可能独立兜底，又会把每块一次的全量 RPC 重拉常态化。价格更新的
    /// 实时唯一来源是 flashblocks 原始交易 calldata（零 RPC）；主通道是否失效由
    /// `observe_block` 的覆盖率自证暴露，收敛交给周期对账（45s）。
    fn sync_events(&self) -> Vec<B256> {
        vec![ELFOMO_TRADE_EVENT]
    }

    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError> {
        let topics = log.topics();

        // Router ElfomoTrade 事件 → 金库余额增量（orderbook 随余额自动重算）。
        //
        // 注意：Pool 的 updatePrices 空事件**不再订阅**（`sync_events` 已剔除），
        // 它零信息量且与种子同源，价格由 raw-tx calldata 直算。
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
            // 金库是成交的双向对手方（链上 10 块逐笔实证）：账户给出什么金库就
            // 收进什么，账户收到什么金库就付出什么。
            //   x→y：Δxeth = +amount_in, Δusdt0 = −amount_out
            //   y→x：Δusdt0 = +amount_in, Δxeth = −amount_out
            // 增量先入 `vault_ledger`（有符号，不静默截断），余额再由账本派生；
            // orderbook 是 (seed, vault) 读时纯函数，余额更新后整本自动重算。
            // `record_trade` 返回 false = 重复回放/已含在快照锚点内 → 不得重复入账。
            let x_to_y = from_token == self.token_x && to_token == self.token_y;
            let y_to_x = from_token == self.token_y && to_token == self.token_x;
            if !x_to_y && !y_to_x {
                // 非本池 pair 的成交（同一 Router 下其它 pair）：静默忽略。
                return Ok(SyncAction::None);
            }
            // 缺块号时绝不能按 0 记账：账本锚点会拒收 `block <= anchor` 的增量 →
            // 金库余额被静默漏记（幻影报价）。走 Resync 用链上绝对值兜住。
            let Some(block) = log.block_number else {
                warn!(
                    target: "amms::elfomo_prop",
                    pool = %self.pool_address,
                    "elfomofi: ElfomoTrade without block number; requesting resync"
                );
                return Ok(SyncAction::Resync);
            };
            self.ensure_vault_ledger_anchored(block);
            if !self
                .vault_ledger
                .record_trade(block, x_to_y, amount_in, amount_out)
            {
                // 两种被拒情形都留痕（不再静默）：快照锚点已含该块 / 同块同日志重放。
                tracing::debug!(
                    target: "amms::elfomo_prop",
                    pool = %self.pool_address,
                    block,
                    anchor_block = self.vault_ledger.anchor_block(),
                    "elfomofi: trade already covered by block-end snapshot anchor; skipped"
                );
                return Ok(SyncAction::None);
            }
            self.sync_vault_balances_from_ledger();
            self.refresh_levels();
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
        self.ensure_model_verified()?;
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
        // 链上 `out_raw = size_raw × price / 1e24`，所以
        //   rate = out_human / in_human
        //        = (out_raw/10^dec_y) / (size_raw/10^dec_x)
        //        = price / 10^(24 + dec_y − dec_x)
        // （xETH 18dp / USDT0 6dp → 10^12，与链上实证一致）。
        let per_xeth_usdt0 = u256_to_f64(&lv.price) / 10f64.powi(self.price_scale_exp());
        Ok(if is_fwd {
            per_xeth_usdt0
        } else {
            1.0 / per_xeth_usdt0
        })
    }

    fn has_sufficient_liquidity(&self) -> bool {
        if !self.model_verified {
            return false;
        }
        !self.levels.from_to_levels.is_empty()
            && !self.levels.to_from_levels.is_empty()
            && (!self.levels.vault_usdt0.is_zero() || !self.levels.vault_xeth.is_zero())
    }

    fn decimals(&self, token: Address) -> u8 {
        self.token_index(token)
            .and_then(|i| self.tokens.get(i))
            .map(|t| t.decimals)
            .unwrap_or_else(|| default_decimals_for(token))
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
        self.ensure_model_verified()?;
        // orderbook 是 (seed, vault) 的读时纯函数：与链上每次读取实时
        // `balanceOf(vault)` 重算同构，本地不缓存档位递减。
        let ob = self.local_orderbook(
            self.price_seed,
            self.levels.vault_usdt0,
            self.levels.vault_xeth,
        );
        if token_in == self.token_x && token_out == self.token_y {
            return Ok(Self::quote_fwd_exact(
                &ob.from_to_levels,
                amount_in,
                ob.vault_usdt0,
            ));
        }
        if token_in == self.token_y && token_out == self.token_x {
            return Ok(Self::quote_rev_exact(
                &ob.to_from_levels,
                amount_in,
                ob.vault_xeth,
            ));
        }
        Ok(U256::ZERO)
    }

    /// 可变模拟：**只允许作用在深拷贝工作副本上**（引擎 pending 路径的约定，
    /// 与 BinaryFi 一致），不可用于 `StateSpace` 里的活池。
    ///
    /// 原因：活池的 `levels.vault_*` 由 `vault_ledger` 派生（真实事件唯一写入点），
    /// 这里直接改派生值不会进账本——下一次 `sync_vault_balances_from_ledger()`
    /// 会把它静默回退。模拟结果只用于路径报价，不需要进真实账本。
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
        // 与 sync() 的金库方向契约一致（金库是对手方，双向记账）。
        if token_in == self.token_x && token_out == self.token_y {
            self.levels.vault_xeth = self.levels.vault_xeth.saturating_add(amount_in);
            self.levels.vault_usdt0 = self.levels.vault_usdt0.saturating_sub(out);
        } else if token_in == self.token_y && token_out == self.token_x {
            self.levels.vault_usdt0 = self.levels.vault_usdt0.saturating_add(amount_in);
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
        self.ensure_model_verified()?;
        let ob = self.local_orderbook(
            self.price_seed,
            self.levels.vault_usdt0,
            self.levels.vault_xeth,
        );
        if token_in == self.token_x && token_out == self.token_y {
            return Ok(Self::quote_fwd_exact_out(
                &ob.from_to_levels,
                amount_out,
                ob.vault_usdt0,
            ));
        }
        if token_in == self.token_y && token_out == self.token_x {
            return Ok(Self::quote_rev_exact_out(
                &ob.to_from_levels,
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
        // 资产：pair 由实例字段 token_x/token_y 定义（Factory/部署配置传入）。
        // decimals **走链上 `IERC20Metadata::decimals()`**（不再硬编码已知 pair）：
        // price 定点换算与 `calculate_price` 的缩放都依赖真实精度，硬编码在接入新的
        // pair（不同 decimals）时会静默算错。RPC 失败时回退已知 pair 常量，不让一次
        // 瞬时失败把 init 整个打挂。
        let dec_x = Self::fetch_token_decimals::<N, _>(&provider, self.token_x)
            .await
            .unwrap_or_else(|| default_decimals_for(self.token_x));
        let dec_y = Self::fetch_token_decimals::<N, _>(&provider, self.token_y)
            .await
            .unwrap_or_else(|| default_decimals_for(self.token_y));
        let sym_x = Self::fetch_token_symbol::<N, _>(&provider, self.token_x)
            .await
            .unwrap_or_else(|| default_symbol_for(self.token_x));
        let sym_y = Self::fetch_token_symbol::<N, _>(&provider, self.token_y)
            .await
            .unwrap_or_else(|| default_symbol_for(self.token_y));
        self.tokens = vec![
            Token {
                address: self.token_x,
                decimals: dec_x,
                symbol: sym_x,
                chain_id: self.chain_id,
                fot_tax: None,
            },
            Token {
                address: self.token_y,
                decimals: dec_y,
                symbol: sym_y,
                chain_id: self.chain_id,
                fot_tax: None,
            },
        ];

        // **逐池从链上取 ladder**：读 `Pool.getMetadata(token_x)`，解码出该 pair 的
        // `(U, T, spread_level, spread_penalty)`。不做任何 pair 特判——新增 pair 自动
        // 获得自己的参数。读取失败 → 保持缺省（invalid ladder）；本地报价为空，
        // 模型自证随后判不可信（fail-closed），并留一条可检索的告警。
        match self
            .fetch_ladder::<N, _>(provider.clone(), block_number)
            .await
        {
            Ok(Some(ladder)) => self.ladder = ladder,
            Ok(None) => warn!(
                target: "amms::elfomo_prop",
                pool = %self.pool_address,
                token_x = %self.token_x,
                token_y = %self.token_y,
                "elfomofi: pool getMetadata returned no ladder for this pair; \
                 quotes will be disabled until a valid ladder is available"
            ),
            Err(e) => warn!(
                target: "amms::elfomo_prop",
                pool = %self.pool_address,
                error = %e,
                "elfomofi: failed to fetch ladder metadata from pool; quotes may be disabled"
            ),
        }

        // ladder 自检：拿到参数后必须 `U > 0 && T > 0`。不满足则模型自证判不可信。
        if !self.ladder.is_valid() {
            warn!(
                target: "amms::elfomo_prop",
                pool = %self.pool_address,
                token_x = %self.token_x,
                token_y = %self.token_y,
                "elfomofi: invalid ladder config for this pair; quotes will be disabled \
                 until a valid ladder + chain model match is provided"
            );
        }

        // 读块钉死后再读：content 与水位同源（见 `fetch_snapshot_at`）
        let (snap, snap_block) = self
            .fetch_snapshot_at::<N, _>(provider, block_number)
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
            snap.profile_word,
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
    /// price 定点 → 人类可读边际价的缩放指数：`24 + dec_y − dec_x`。
    fn price_scale_exp(&self) -> i32 {
        24 + self.decimals(self.token_y) as i32 - self.decimals(self.token_x) as i32
    }

    /// 链上读 `decimals()`；失败返回 None（调用方回退已知常量）。
    async fn fetch_token_decimals<N, P>(provider: &P, token: Address) -> Option<u8>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        IERC20Metadata::new(token, provider.clone())
            .decimals()
            .call()
            .await
            .ok()
    }

    /// 链上读 `symbol()`（展示用元数据，不参与报价）；失败返回 None。
    async fn fetch_token_symbol<N, P>(provider: &P, token: Address) -> Option<String>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        IERC20Metadata::new(token, provider.clone())
            .symbol()
            .call()
            .await
            .ok()
            .filter(|s| !s.is_empty())
    }

    /// 拉取链上精确快照：**先把读块钉死、再读**（content 与水位同源）。
    ///
    /// `BlockId::Number(n)` 直接使用 `n`；其余（`latest` 等）先解析为 provider
    /// 当前头部块号，再以 `BlockId::Number(head)` 读取。杜绝"读的是 latest、
    /// 水位记的是另一次 `get_block_number()`"的错位：content 覆盖块 ≠ 水位块时，
    /// 落在该区间的成交会被水位守卫跳过 → 永久漏账。
    /// 读块可用性由 `ensure_storage_block_available` 守卫（超前于节点头部返回
    /// `BlockNotAvailable`，由调用方留队重试，不降级读取）。
    pub async fn fetch_snapshot_at<N, P>(
        &self,
        provider: P,
        block: BlockId,
    ) -> Result<(OrderbookSnapshot, u64), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let snap_block: u64 = match block {
            BlockId::Number(alloy::eips::BlockNumberOrTag::Number(num)) => num,
            _ => provider.get_block_number().await?,
        };
        let snap = self
            .fetch_orderbook_snapshot::<N, _>(provider, BlockId::Number(snap_block.into()))
            .await?;
        Ok((snap, snap_block))
    }

    /// 把链上精确快照**合并**进本地池（`快照(S) + Σ_{块 > S} 事件增量`）。
    ///
    /// 修复的反模式（v1.21.3 之前）：先前用**块级水位**判断"快照是否落后"
    /// （`last_synced_block > snap_block → 丢弃`）。Elfomo 的块级水位被 flashblock
    /// raw-tx（每块一笔 `updatePrices`）顶到乐观头，而快照读的是存储 RPC 的
    /// 规范头——于是健康期快照几乎总被跳过；一旦事件流**部分丢帧**（fire-once、
    /// 无缺口检测，水位仍在头部），快照永远修不进来 → vault 漂移无界累积。
    ///
    /// 现在交给 [`VaultDeltaLedger::rebase`]：快照永远能落地（增量 rebase 而非
    /// 覆盖），只有"快照早于当前**锚点**块"这一真正无法重建的情形才丢弃。
    ///
    /// 返回是否落地。
    pub fn merge_snapshot(&mut self, snap: OrderbookSnapshot, snap_block: u64) -> bool {
        let apply = self
            .vault_ledger
            .rebase(snap_block, snap.vault_xeth, snap.vault_usdt0);
        match apply {
            VaultLedgerApply::SkippedStale => {
                tracing::debug!(
                    target: "amms::elfomo_prop",
                    pool = %self.pool_address,
                    snap_block,
                    anchor_block = self.vault_ledger.anchor_block(),
                    "elfomofi: stale snapshot skipped (older than ledger anchor)"
                );
                return false;
            }
            VaultLedgerApply::SkippedNoBlock => {
                warn!(
                    target: "amms::elfomo_prop",
                    pool = %self.pool_address,
                    "elfomofi: snapshot without block number, keeping event ledger"
                );
                return false;
            }
            VaultLedgerApply::Anchored => tracing::debug!(
                target: "amms::elfomo_prop",
                pool = %self.pool_address,
                snap_block,
                "elfomofi: snapshot anchored (covers every recorded trade)"
            ),
            VaultLedgerApply::Merged => tracing::debug!(
                target: "amms::elfomo_prop",
                pool = %self.pool_address,
                snap_block,
                last_event_block = self.vault_ledger.last_event_block(),
                "elfomofi: snapshot rebase-merged (kept newer realtime trades)"
            ),
        }

        // 种子：**字段级水位**保鲜（规则 1）。raw-tx 种子来自 flashblock 乐观头，
        // 领先快照读的规范头；快照块早于本地种子水位时必须保留本地种子，
        // 否则会把已应用的新种子回退成旧种子 → 旧价报价窗口（幻影机会）。
        let seed_fresh = snap_block >= self.price_seed_block;
        if seed_fresh {
            self.price_seed = snap.price_seed;
            // 快照(S) 已含 S 及更早的全部种子更新 → 水位 stamp 到 S
            self.price_seed_block = snap_block;
        } else {
            tracing::debug!(
                target: "amms::elfomo_prop",
                pool = %self.pool_address,
                snap_block,
                price_seed_block = self.price_seed_block,
                "elfomofi: snapshot kept newer realtime price seed"
            );
        }

        // profile：同样的**字段级水位**保鲜（2026-09-11 起 profile 是动态存储：
        // keeper 的 `0xd4ff31bd` 会改）。快照的 word 是"那一刻链上的值"，
        // 既用于落地，也作为模型自证的 profile 基准（链上档位是那一刻 profile 的
        // 纯函数，用本地更新过的 profile 对拍会假阴性）。
        let snapshot_profile_opt = if snap.profile_word.is_zero() {
            None
        } else {
            ElfomoBandProfile::decode_word(snap.profile_word)
        };
        if !snap.profile_word.is_zero() && snapshot_profile_opt.is_none() {
            warn!(
                target: "amms::elfomo_prop",
                pool = %self.pool_address,
                snap_block,
                profile_word = %snap.profile_word,
                "elfomofi: invalid band profile word in snapshot; keeping local profile"
            );
        }
        let snapshot_profile = snapshot_profile_opt.unwrap_or(self.ladder.profile);
        let profile_fresh = snapshot_profile_opt.is_some() && snap_block >= self.profile_block;
        if profile_fresh {
            self.ladder.profile = snapshot_profile;
            self.profile_block = snap_block;
        } else if !snap.profile_word.is_zero() {
            tracing::debug!(
                target: "amms::elfomo_prop",
                pool = %self.pool_address,
                snap_block,
                profile_block = self.profile_block,
                "elfomofi: snapshot kept newer realtime band profile"
            );
        }

        // 余额 = 账本派生（快照 + Σ_{>S}）。只有"快照覆盖全部已记录成交"
        // （`Anchored`）**且**种子/profile 也都采用快照值时才可直接采用链上档位——
        // 链上档位是用快照的 (profile, seed, 余额) 算出来的，任一被本地更新取代，
        // 都必须按本地值重建，否则档位与三者互相矛盾。
        let covered_all = apply == VaultLedgerApply::Anchored;
        // 模型自证：链上档位是快照 (profile, seed, 余额) 的纯函数，与本地重算逐位对拍。
        // 不一致 → `model_verified=false`，报价路径拒绝该池（不给错价）。
        self.verify_model_against_chain(
            &snap.from_to_levels,
            &snap.to_from_levels,
            snap.price_seed,
            snap.vault_usdt0,
            snap.vault_xeth,
            &snapshot_profile,
            "snapshot-reconcile",
            snap_block,
        );
        self.sync_vault_balances_from_ledger();
        if covered_all
            && seed_fresh
            && profile_fresh
            && !snap.from_to_levels.is_empty()
            && !snap.to_from_levels.is_empty()
        {
            self.levels = OrderbookSnapshot {
                from_to_levels: snap.from_to_levels,
                to_from_levels: snap.to_from_levels,
                vault_usdt0: snap.vault_usdt0,
                vault_xeth: snap.vault_xeth,
                price_seed: snap.price_seed,
                profile_word: snapshot_profile.encode_word(),
            };
        } else {
            self.refresh_levels();
        }
        self.last_synced_block = self.last_synced_block.max(snap_block);
        true
    }

    /// 在指定区块拉取并合并 orderbook + vault 快照（StateSpace update 与周期
    /// 任务共用）。读块钉定见 [`Self::fetch_snapshot_at`]，合并语义见
    /// [`Self::merge_snapshot`]。
    pub async fn update_at<N, P>(&mut self, provider: P, block: BlockId) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let (snap, snap_block) = self.fetch_snapshot_at::<N, _>(provider, block).await?;
        self.merge_snapshot(snap, snap_block);
        Ok(())
    }
}

/// 已知 token 的 decimals 兜底（链上读取失败时用；未知 token 按 18 处理）。
fn default_decimals_for(token: Address) -> u8 {
    if token == ELFOMO_USDT0_ADDRESS {
        6
    } else {
        18
    }
}

/// 已知 token 的 symbol 兜底（链上 `symbol()` 读取失败时用）。
fn default_symbol_for(token: Address) -> String {
    if token == ELFOMO_XETH_ADDRESS {
        "xETH".to_string()
    } else if token == ELFOMO_USDT0_ADDRESS {
        "USDT0".to_string()
    } else {
        "TOKEN".to_string()
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

    /// XLayer xETH/USDT0 的链上 ladder 参数（`getMetadata(0xe7b0…025a)` 实测 fixture）。
    ///
    /// 仅测试用：生产代码在 `init` 阶段**逐池从链上读取**，不依赖此常量。
    /// 旧的（块 `70352699` 及更早）链上 profile：宽度表 + 偏离表。
    /// keeper 于块 `70352700` 起改为 `[15,20,25,35,45,60]`；本仓库的历史 fixture
    /// （69M 块 / 全网格）都对应这张旧表，故测试用它。
    fn legacy_profile() -> ElfomoBandProfile {
        ElfomoBandProfile {
            count: 6,
            widths: [1, 5, 10, 10, 20, 100],
            deviations: [7, 10, 15, 25, 40, 50],
        }
    }

    fn xlayer_ladder() -> ElfomoLadderConfig {
        ElfomoLadderConfig {
            profile: legacy_profile(),
            ..ElfomoLadderConfig::from_metadata(&[
                U256::ZERO,
                U256::from(18u64),
                U256::from(2u64),
                U256::ZERO,
                U256::ZERO,
                U256::from(600_000_000_000_000_000u128),
                U256::from(30u64),
                U256::from(5u64),
                U256::from(60u64),
                U256::ZERO,
                U256::ZERO,
            ])
        }
    }

    /// fork 对拍 helper：用 xLayer ladder 走本地读时重算。
    fn build_orderbook(seed: U256, vault_usdt0: U256, vault_xeth: U256) -> OrderbookSnapshot {
        ElfomoFiPropPool::build_orderbook_with(&xlayer_ladder(), seed, vault_usdt0, vault_xeth)
    }

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
            profile_word: legacy_profile().encode_word(),
        }
    }

    #[test]
    fn test_quote_fwd_exact() {
        let s = snapshot();
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
                U256::from(*inp),
                s.vault_usdt0,
            );
            assert_eq!(got, U256::from(*want), "fwd exact-in in={inp}");
        }
    }

    #[test]
    fn test_quote_rev_exact() {
        let s = snapshot();
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
                U256::from(*inp),
                s.vault_xeth,
            );
            assert_eq!(got, U256::from(*want), "rev exact-in in={inp}");
        }
    }

    #[test]
    fn test_quote_fwd_exact_out() {
        let s = snapshot();
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
                U256::from(*to),
                s.vault_usdt0,
            );
            assert_eq!(got, U256::from(*want), "fwd exact-out to={to}");
        }
    }

    #[test]
    fn test_quote_rev_exact_out() {
        let s = snapshot();
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
                U256::from(*to),
                s.vault_xeth,
            );
            assert_eq!(got, U256::from(*want), "rev exact-out to={to}");
        }
    }

    #[test]
    fn test_swap_then_quote_derived_from_vault() {
        // orderbook 是 (seed, vault) 的读时纯函数：swap 只改金库余额，
        // 下一次 quote 自动按新余额重建，不存在"本地档位消耗"这一层状态。
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
            price_seed: U256::from(0x143c60fu64),
            ladder: xlayer_ladder(),
            ..ElfomoFiPropPool::default()
        };
        // 模拟一笔第一档内的小额 swap（输入 0.1215e18 < 档 1 容量 0.6e18）
        let amount_in = U256::from(121_513_229_231_558_820u128);
        let out = pool
            .simulate_swap(ELFOMO_XETH_ADDRESS, ELFOMO_USDT0_ADDRESS, amount_in)
            .unwrap();
        assert!(out > U256::ZERO);
        let vault_before = pool.levels.vault_usdt0;
        // 事件同步（ElfomoTrade）只改金库余额（双向记账）
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
    fn test_pool_update_event_is_ignored() {
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

        // Pool updatePrices 空事件（仅 topic0、data 0 字节、零信息量）：
        // 不再作为状态源（sync_events 也不再订阅），必须被忽略——它的价格信息
        // 与 raw-tx calldata 同源，重拉没有任何独立价值，只会把全量 RPC 常态化。
        let update_log: Log = serde_json::from_value(serde_json::json!({
            "address": format!("{:#x}", ELFOMO_POOL_ADDRESS),
            "topics": [format!("{:#x}", ELFOMO_UPDATE_EVENT)],
            "data": "0x",
            "blockNumber": "0x423c2b8",
            "transactionIndex": "0x0",
            "logIndex": "0x0",
        }))
        .unwrap();
        assert!(matches!(pool.sync(&update_log).unwrap(), SyncAction::None));
        // 订阅表里只剩 ElfomoTrade
        assert_eq!(pool.sync_events(), vec![ELFOMO_TRADE_EVENT]);

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
    fn test_sync_trade_event_updates_vault_and_recomputes_orderbook() {
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
            price_seed: U256::from(0x143c60fu64),
            ladder: xlayer_ladder(),
            ..ElfomoFiPropPool::default()
        };
        let amount_in = U256::from(121_513_229_231_558_820u128);
        let out = ElfomoFiPropPool::quote_fwd_exact(&s.from_to_levels, amount_in, s.vault_usdt0);
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
        // 金库双向记账：xETH 收进 amount_in、USDT0 付出 out；orderbook 是
        // (seed, vault) 读时函数，事件后缓存已重建
        //（本笔成交量小，首档仍满 0.6e18，报价不变）
        assert_eq!(pool.levels.vault_usdt0, s.vault_usdt0 - out);
        assert_eq!(pool.levels.vault_xeth, s.vault_xeth + amount_in);
        assert_eq!(pool.levels.from_to_levels[0].size, s.from_to_levels[0].size);
        let out_again = pool
            .simulate_swap(ELFOMO_XETH_ADDRESS, ELFOMO_USDT0_ADDRESS, amount_in)
            .unwrap();
        assert_eq!(out_again, out);
    }

    #[test]
    fn test_build_orderbook_matches_chain_block() {
        // 块 0x423c2b8（seed=0x143c60f，vault 实测）逐位对拍
        let ob = build_orderbook(
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
        let ob = build_orderbook(
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
            price_seed: U256::from(0x143c60fu64),
            last_synced_block: 1,
            ladder: xlayer_ladder(),
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
        // 旧块号不回退：种子与字段级水位都不得被旧块回退
        pool.apply_price_seed(U256::from(0xdeadbeefu64), 50);
        assert_eq!(pool.last_synced_block, 100);
        assert_eq!(pool.price_seed, new_seed);
        assert_eq!(pool.price_seed_block, 100);
        assert_eq!(pool.raw_seed_block, 100);
    }

    #[test]
    fn test_real_arb_tx_ledger_replay() {
        // 真实套利交易 0x3a608dfefedf19731f01ba93945df8475fa9559eb40f5bae07334f991369e6f0
        // （块 69447881，status=0x1，ElfomoFi 段 xETH→USDT0）：
        // 同块 updatePrices calldata 解出种子 0x143c4e5 + 父块金库余额 →
        // 本地重算 orderbook → 报价精确等于事件 toAmount=300147468。
        // 这是「raw-tx 种子 + 本地金库 → 读时重算」模型的链上端到端回归锚点。
        let seed = U256::from(0x143c4e5u64);
        let ob = build_orderbook(
            seed,
            U256::from(19_492_562_722u64),
            U256::from(2_818_949_271_769_303_366u128),
        );
        let amount_in = U256::from(121_513_229_231_558_820u128);
        let got = ElfomoFiPropPool::quote_fwd_exact(&ob.from_to_levels, amount_in, ob.vault_usdt0);
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
            price_seed: U256::from(0x143c60fu64),
            ladder: xlayer_ladder(),
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
        // 金库双向记账：USDT0 收进实际输入、xETH 按实际输出扣减；
        // toFrom 档位随余额收缩后 s1+s2+s3 == vault
        assert_eq!(
            pool.levels.vault_usdt0,
            s.vault_usdt0 + U256::from(300_000_000u64)
        );
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
    fn test_merge_snapshot_rebases_newer_trades_instead_of_skipping() {
        // 修复的反模式：先前用块级水位判断"快照是否落后"（本地水位更高就丢弃），
        // 会把唯一的纠错通道饿死。正确语义是 `快照(S) + Σ_{块 > S} 事件增量`。
        let s = snapshot();
        let mut pool = ElfomoFiPropPool {
            levels: s.clone(),
            price_seed: s.price_seed,
            last_synced_block: 1_000,
            ladder: xlayer_ladder(),
            ..ElfomoFiPropPool::default()
        };

        // 先以块 1_000 的链上真值锚定
        assert!(pool.merge_snapshot(s.clone(), 1_000));
        assert_eq!(pool.last_synced_block, 1_000);

        // 块 1_005 的一笔 ElfomoTrade（xETH→USDT0）：本地水位推进到 1_005
        let amount_in = U256::from(1_000_000_000_000_000_000u128); // 1 xETH
        let amount_out = U256::from(3_000_000_000u64); // 3000 USDT0
        let mut data = Vec::new();
        for w in [
            U256::from(0x1234u64),
            U256::from(0x5678u64),
            U256::ZERO,
            U256::ZERO,
            amount_in,
            amount_out,
        ] {
            data.extend_from_slice(&w.to_be_bytes::<32>());
        }
        data[64..96].copy_from_slice(ELFOMO_XETH_ADDRESS.into_word().as_slice());
        data[96..128].copy_from_slice(ELFOMO_USDT0_ADDRESS.into_word().as_slice());
        let trade_log: Log = serde_json::from_value(serde_json::json!({
            "address": format!("{:#x}", ELFOMO_ROUTER_ADDRESS),
            "topics": [
                format!("{:#x}", ELFOMO_TRADE_EVENT),
                format!("0x{:064x}", 1u64),
                format!("0x{:064x}", 0u64),
            ],
            "data": format!("0x{}", alloy::hex::encode(&data)),
            "blockNumber": format!("0x{:x}", 1_005u64),
            "transactionIndex": "0x0",
            "logIndex": "0x0",
        }))
        .unwrap();
        assert!(matches!(pool.sync(&trade_log).unwrap(), SyncAction::None));
        assert_eq!(pool.levels.vault_xeth, s.vault_xeth + amount_in);
        assert_eq!(pool.levels.vault_usdt0, s.vault_usdt0 - amount_out);
        // StateSpace 在 sync() 返回后推进块级水位（幂等守卫用）
        pool.set_last_synced_block(1_005);
        assert_eq!(pool.last_synced_block, 1_005);

        // 快照读块 1_003 **落后于**本地水位 1_005：必须落地而不是跳过
        // （链上真值 1_003 = s + 块内 (1_000,1_003] 的净变化，这里记 (+100,-50)）
        let snap_xeth = s.vault_xeth + U256::from(100u64);
        let snap_usdt0 = s.vault_usdt0 - U256::from(50u64);
        let ob = build_orderbook(s.price_seed, snap_usdt0, snap_xeth);
        let older = OrderbookSnapshot {
            from_to_levels: ob.from_to_levels,
            to_from_levels: ob.to_from_levels,
            vault_usdt0: snap_usdt0,
            vault_xeth: snap_xeth,
            price_seed: s.price_seed,
            profile_word: legacy_profile().encode_word(),
        };
        assert!(
            pool.merge_snapshot(older, 1_003),
            "snapshot older than local watermark must still land via rebase"
        );
        // current = 快照(1_003) + Σ_{>1_003}（块 1_005 那笔）
        assert_eq!(pool.levels.vault_xeth, snap_xeth + amount_in);
        assert_eq!(pool.levels.vault_usdt0, snap_usdt0 - amount_out);
        // 水位保持单调（不回退）
        assert_eq!(pool.last_synced_block, 1_005);

        // 更新的快照覆盖全部已记录事件 → 直接锚定，账本清空
        let vault_usdt0 = U256::from(7_000_000_000u64);
        let vault_xeth = U256::from(1_500_000_000_000_000_000u128);
        let ob = build_orderbook(s.price_seed, vault_usdt0, vault_xeth);
        let fresh = OrderbookSnapshot {
            from_to_levels: ob.from_to_levels,
            to_from_levels: ob.to_from_levels,
            vault_usdt0,
            vault_xeth,
            price_seed: s.price_seed,
            profile_word: legacy_profile().encode_word(),
        };
        assert!(pool.merge_snapshot(fresh, 1_006));
        assert_eq!(pool.levels.vault_usdt0, vault_usdt0);
        assert_eq!(pool.levels.vault_xeth, vault_xeth);
        assert!(pool.vault_ledger.is_empty());
        assert_eq!(pool.last_synced_block, 1_006);
        // 落地后报价与 (seed, vault) 读时函数一致
        let rebuilt = build_orderbook(
            pool.price_seed,
            pool.levels.vault_usdt0,
            pool.levels.vault_xeth,
        );
        assert_eq!(rebuilt.from_to_levels, pool.levels.from_to_levels);
        assert_eq!(rebuilt.to_from_levels, pool.levels.to_from_levels);
    }

    #[test]
    fn test_apply_price_seed_ignores_stale_block() {
        // 规则 1：水位必须被读。旧块/重放的 raw-tx 不得回退种子，也不得把水位
        // stamp 到自相矛盾的更高块号（否则会骗过 merge_snapshot 的 seed_fresh）。
        let s = snapshot();
        let mut pool = ElfomoFiPropPool {
            levels: s.clone(),
            price_seed: s.price_seed,
            ladder: xlayer_ladder(),
            ..ElfomoFiPropPool::default()
        };
        pool.apply_price_seed(U256::from(111u64), 1_010);
        assert_eq!(pool.price_seed, U256::from(111u64));
        assert_eq!(pool.price_seed_block, 1_010);

        pool.apply_price_seed(U256::from(999u64), 1_005);
        assert_eq!(pool.price_seed, U256::from(111u64));
        assert_eq!(pool.price_seed_block, 1_010);

        // 同块重放仍可推进（调用方按 tx_index 排序，最后一笔赢）
        pool.apply_price_seed(U256::from(222u64), 1_010);
        assert_eq!(pool.price_seed, U256::from(222u64));
        assert_eq!(pool.price_seed_block, 1_010);

        // 更新的块正常推进
        pool.apply_price_seed(U256::from(333u64), 1_011);
        assert_eq!(pool.price_seed, U256::from(333u64));
        assert_eq!(pool.price_seed_block, 1_011);
    }

    #[test]
    fn test_merge_snapshot_keeps_newer_realtime_price_seed() {
        // 规则 1：种子是"最新值"字段，必须字段级水位保鲜。
        // raw-tx 种子来自 flashblock 乐观头，快照读规范头 → 快照落地时
        // 若本地种子更新（price_seed_block 更高），不得回退成快照的旧种子。
        let s = snapshot();
        let mut pool = ElfomoFiPropPool {
            levels: s.clone(),
            price_seed: s.price_seed,
            ladder: xlayer_ladder(),
            ..ElfomoFiPropPool::default()
        };
        assert!(pool.merge_snapshot(s.clone(), 1_000));
        assert_eq!(pool.price_seed_block, 1_000);

        // 块 1_005：raw-tx 应用了新种子（乐观头，领先规范头）
        let new_seed = s.price_seed + U256::from(12_345u64);
        pool.apply_price_seed(new_seed, 1_005);
        assert_eq!(pool.price_seed, new_seed);
        assert_eq!(pool.price_seed_block, 1_005);

        // 规范头快照（块 1_003）带的是**旧种子** → 必须保留本地新种子，
        // 且档位要按本地 (新种子, 余额) 重建，不能采用快照的旧种子档位。
        let ob = build_orderbook(s.price_seed, s.vault_usdt0, s.vault_xeth);
        let older = OrderbookSnapshot {
            from_to_levels: ob.from_to_levels,
            to_from_levels: ob.to_from_levels,
            vault_usdt0: s.vault_usdt0,
            vault_xeth: s.vault_xeth,
            price_seed: s.price_seed,
            profile_word: legacy_profile().encode_word(),
        };
        assert!(pool.merge_snapshot(older, 1_003));
        assert_eq!(
            pool.price_seed, new_seed,
            "newer realtime seed must not be rolled back by a snapshot"
        );
        assert_eq!(pool.price_seed_block, 1_005);
        let rebuilt = build_orderbook(new_seed, pool.levels.vault_usdt0, pool.levels.vault_xeth);
        assert_eq!(rebuilt.from_to_levels, pool.levels.from_to_levels);
        assert_eq!(rebuilt.to_from_levels, pool.levels.to_from_levels);

        // 快照块追上种子水位（1_006 > 1_005）→ 采用快照种子并抢占水位
        let ob2 = build_orderbook(s.price_seed, s.vault_usdt0, s.vault_xeth);
        let fresh = OrderbookSnapshot {
            from_to_levels: ob2.from_to_levels,
            to_from_levels: ob2.to_from_levels,
            vault_usdt0: s.vault_usdt0,
            vault_xeth: s.vault_xeth,
            price_seed: s.price_seed,
            profile_word: legacy_profile().encode_word(),
        };
        assert!(pool.merge_snapshot(fresh, 1_006));
        assert_eq!(pool.price_seed, s.price_seed);
        assert_eq!(pool.price_seed_block, 1_006);
    }

    #[test]
    fn test_merge_snapshot_skips_only_when_older_than_anchor() {
        // 只有"快照早于当前账本锚点块"（无法重建 (S, base] 增量）才丢弃。
        let s = snapshot();
        let mut pool = ElfomoFiPropPool {
            levels: s.clone(),
            price_seed: s.price_seed,
            ladder: xlayer_ladder(),
            ..ElfomoFiPropPool::default()
        };
        assert!(pool.merge_snapshot(s.clone(), 2_000));
        assert_eq!(pool.vault_ledger.anchor_block(), 2_000);

        let vault_usdt0 = U256::from(7_000_000_000u64);
        let vault_xeth = U256::from(1_500_000_000_000_000_000u128);
        let ob = build_orderbook(s.price_seed, vault_usdt0, vault_xeth);
        let stale = OrderbookSnapshot {
            from_to_levels: ob.from_to_levels,
            to_from_levels: ob.to_from_levels,
            vault_usdt0,
            vault_xeth,
            price_seed: s.price_seed,
            profile_word: legacy_profile().encode_word(),
        };
        assert!(!pool.merge_snapshot(stale, 1_999));
        assert_eq!(pool.levels.vault_xeth, s.vault_xeth);
        assert_eq!(pool.levels.vault_usdt0, s.vault_usdt0);
        assert_eq!(pool.last_synced_block, 2_000);
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
            price_seed: U256::from(0x143c60fu64),
            ladder: xlayer_ladder(),
            ..ElfomoFiPropPool::default()
        };
        let ob_small = build_orderbook(
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

    fn xeth_usdt0_tokens() -> Vec<Token> {
        vec![
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
        ]
    }

    fn pool_with_snapshot() -> ElfomoFiPropPool {
        ElfomoFiPropPool {
            token_x: ELFOMO_XETH_ADDRESS,
            token_y: ELFOMO_USDT0_ADDRESS,
            tokens: xeth_usdt0_tokens(),
            levels: snapshot(),
            price_seed: U256::from(0x143c60fu64),
            ladder: xlayer_ladder(),
            ..ElfomoFiPropPool::default()
        }
    }

    #[test]
    fn test_chain_fixture_is_bit_identical_to_local_model() {
        // 关键前提：`verify_model_against_chain` 用逐位相等判定模型是否可信。
        // 若链上 fixture 与本地重算有一 wei 偏差，生产环境 init/对账会把模型
        // 误判为失效、报价被整体禁用。这里把该前提钉死。
        let s = snapshot();
        let rebuilt = build_orderbook(s.price_seed, s.vault_usdt0, s.vault_xeth);
        assert_eq!(rebuilt.from_to_levels, s.from_to_levels);
        assert_eq!(rebuilt.to_from_levels, s.to_from_levels);
    }

    /// 全网格 bit-exact 回归：链上 `getOrderbook` vs 本地模型（不依赖 RPC）。
    ///
    /// fixture 由 anvil fork XLayer + `eth_call` (`stateDiff` override) 生成，覆盖
    /// `band_count ∈ {6,10,30,200} × spread_penalty ∈ {0,60} × spread_level ∈ {0,1,5,29,30}
    /// × vault_xeth ∈ {0.5U, 2.94U, 5U, 5.1U, 10.1U, 29.5U, 30.1U}` 及若干真实池状态，
    /// 每行都记录链上原始档位（size/price 十进制、全精度）。任何 ladder 规则改动都必须
    /// 让本用例保持全绿——它是"本地模拟 == 链上结果"最直接的证据。
    #[test]
    fn test_chain_ladder_grid_bit_exact() {
        #[derive(serde::Deserialize)]
        struct Row {
            t: u16,
            f: u8,
            sl: u16,
            vx: String,
            vu: String,
            seed: String,
            ft: Vec<(String, String)>,
            tf: Vec<(String, String)>,
        }
        let rows: Vec<Row> = serde_json::from_str(include_str!("fixtures/ladder_grid.json"))
            .expect("fixture JSON 解析失败");
        assert!(rows.len() >= 250, "fixture 被裁剪？只有 {} 行", rows.len());

        let parse = |s: &str| U256::from_str_radix(s, 10).expect("U256 解析失败");
        let mut bad = Vec::new();
        for (idx, r) in rows.iter().enumerate() {
            let ladder = ElfomoLadderConfig {
                unit: U256::from(600_000_000_000_000_000u128),
                band_count: r.t,
                spread_level: r.sl,
                spread_penalty: r.f,
                profile: legacy_profile(),
                profile_key: 0,
            };
            let ob = ElfomoFiPropPool::build_orderbook_with(
                &ladder,
                parse(&r.seed),
                parse(&r.vu),
                parse(&r.vx),
            );
            let levels = |v: &[OrderbookLevel]| -> Vec<(String, String)> {
                v.iter()
                    .map(|l| (l.size.to_string(), l.price.to_string()))
                    .collect()
            };
            if levels(&ob.from_to_levels) != r.ft || levels(&ob.to_from_levels) != r.tf {
                bad.push(format!(
                    "#{idx} T={} F={} SL={} vx={} vu={} seed={}\n  chain ft={:?}\n  local ft={:?}\n                       chain tf={:?}\n  local tf={:?}",
                    r.t,
                    r.f,
                    r.sl,
                    r.vx,
                    r.vu,
                    r.seed,
                    r.ft,
                    levels(&ob.from_to_levels),
                    r.tf,
                    levels(&ob.to_from_levels),
                ));
            }
        }
        assert!(
            bad.is_empty(),
            "{} / {} 组 orderbook 与链上不一致：\n{}",
            bad.len(),
            rows.len(),
            bad.join("\n")
        );
    }

    #[test]
    fn test_model_mismatch_disables_quotes_until_reverified() {
        let s = snapshot();
        let mut pool = pool_with_snapshot();
        let ok = build_orderbook(s.price_seed, s.vault_usdt0, s.vault_xeth);
        let amount_in = U256::from(1_000_000_000_000u64);

        // 链上档位 == 本地重算 → 模型可信，报价可用
        assert!(pool.verify_model_against_chain(
            &ok.from_to_levels,
            &ok.to_from_levels,
            s.price_seed,
            s.vault_usdt0,
            s.vault_xeth,
            &legacy_profile(),
            "test",
            100,
        ));
        assert!(pool.model_verified);
        assert!(pool.has_sufficient_liquidity());
        assert!(pool
            .simulate_swap(ELFOMO_XETH_ADDRESS, ELFOMO_USDT0_ADDRESS, amount_in)
            .is_ok());
        assert!(pool
            .calculate_price(ELFOMO_XETH_ADDRESS, ELFOMO_USDT0_ADDRESS)
            .is_ok());

        // 链上档位与本地重算不一致（协议改参数 / 公式逆向有误）→ 拒绝报价
        let bogus = vec![lev(1, 1)];
        assert!(!pool.verify_model_against_chain(
            &bogus,
            &bogus,
            s.price_seed,
            s.vault_usdt0,
            s.vault_xeth,
            &legacy_profile(),
            "test",
            100,
        ));
        assert!(!pool.model_verified);
        assert!(!pool.has_sufficient_liquidity());
        assert!(pool
            .simulate_swap(ELFOMO_XETH_ADDRESS, ELFOMO_USDT0_ADDRESS, amount_in)
            .is_err());
        assert!(pool
            .simulate_swap_exact_out(ELFOMO_XETH_ADDRESS, ELFOMO_USDT0_ADDRESS, U256::from(1u64))
            .is_err());
        assert!(pool
            .calculate_price(ELFOMO_XETH_ADDRESS, ELFOMO_USDT0_ADDRESS)
            .is_err());

        // 下一次对拍一致 → 自动恢复（45s 对账会做这件事）
        assert!(pool.verify_model_against_chain(
            &ok.from_to_levels,
            &ok.to_from_levels,
            s.price_seed,
            s.vault_usdt0,
            s.vault_xeth,
            &legacy_profile(),
            "test",
            101,
        ));
        assert!(pool.model_verified);
        assert!(pool
            .simulate_swap(ELFOMO_XETH_ADDRESS, ELFOMO_USDT0_ADDRESS, amount_in)
            .is_ok());
    }

    #[test]
    fn test_ladder_is_per_pool_and_fail_closed() {
        let s = snapshot();
        // 缺省 ladder 与静态已知 pair 版逐位一致（回归保护）
        let mut pool = pool_with_snapshot();
        assert_eq!(
            pool.local_orderbook(s.price_seed, s.vault_usdt0, s.vault_xeth),
            build_orderbook(s.price_seed, s.vault_usdt0, s.vault_xeth)
        );

        // 换一个 pool 的 ladder（U 不同）→ 本地 orderbook 必须随之改变，
        // 且与"本池链上档位"对拍必然不一致 → 拒绝报价（fail-closed）
        let mut other = xlayer_ladder();
        other.unit = U256::from(1_200_000_000_000_000_000u128);
        pool.ladder = other;
        let ob = pool.local_orderbook(s.price_seed, s.vault_usdt0, s.vault_xeth);
        assert_ne!(ob.from_to_levels[0].size, s.from_to_levels[0].size);
        assert!(!pool.verify_model_against_chain(
            &s.from_to_levels,
            &s.to_from_levels,
            s.price_seed,
            s.vault_usdt0,
            s.vault_xeth,
            &legacy_profile(),
            "test",
            100,
        ));
        assert!(!pool.model_verified);
        assert!(pool
            .simulate_swap(
                ELFOMO_XETH_ADDRESS,
                ELFOMO_USDT0_ADDRESS,
                U256::from(1_000_000_000_000u64)
            )
            .is_err());

        // 配置缺失（缺省/未 init 的 ladder）不 panic，只是判不可信
        pool.ladder = ElfomoLadderConfig::default();
        assert!(!pool.ladder.is_valid());
        let empty = pool.local_orderbook(s.price_seed, s.vault_usdt0, s.vault_xeth);
        assert!(empty.from_to_levels.is_empty() && empty.to_from_levels.is_empty());
        assert!(pool
            .simulate_swap(
                ELFOMO_XETH_ADDRESS,
                ELFOMO_USDT0_ADDRESS,
                U256::from(1_000_000_000_000u64)
            )
            .is_err());
    }

    #[test]
    fn test_trade_without_block_number_requests_resync() {
        // 缺块号不能按 0 记账（账本锚点会拒收 → 金库增量静默漏记）→ 走 Resync
        let mut pool = pool_with_snapshot();
        let mut data = Vec::new();
        for w in [
            U256::from(0x1234u64),
            U256::from(0x5678u64),
            ELFOMO_XETH_ADDRESS.into_word().into(),
            ELFOMO_USDT0_ADDRESS.into_word().into(),
            U256::from(1_000_000_000u64),
            U256::from(2_000_000_000u64),
        ] {
            data.extend_from_slice(&w.to_be_bytes::<32>());
        }
        let log: Log = serde_json::from_value(serde_json::json!({
            "address": format!("{:#x}", ELFOMO_ROUTER_ADDRESS),
            "topics": [
                format!("{:#x}", ELFOMO_TRADE_EVENT),
                format!("0x{:064x}", 1u64),
            ],
            "data": format!("0x{}", alloy::hex::encode(&data)),
            "blockNumber": null,
            "transactionIndex": "0x0",
            "logIndex": "0x0",
        }))
        .unwrap();
        assert!(matches!(pool.sync(&log).unwrap(), SyncAction::Resync));
        // 金库余额不得被改动
        assert_eq!(pool.levels.vault_usdt0, snapshot().vault_usdt0);
        assert_eq!(pool.levels.vault_xeth, snapshot().vault_xeth);
    }

    #[test]
    fn test_seed_coverage_counts_blocks_and_resets_on_recovery() {
        let mut pool = ElfomoFiPropPool::default();
        pool.observe_block(1_000);
        assert_eq!(pool.blocks_since_seed, 0);

        // 1001..=1006 收口 1000..=1005：全部没有 raw-tx 种子
        for block in 1_001..=1_006 {
            pool.observe_block(block);
        }
        assert_eq!(pool.blocks_since_seed, 6);
        assert!(
            pool.seed_coverage_alerted,
            "达到阈值必须置告警位（只报一次）"
        );
        let (last_seed, missing) = pool.seed_coverage();
        assert_eq!((last_seed, missing), (0, 6));

        // 继续缺失：计数继续累计，但告警不重复触发（标志位保持）
        pool.observe_block(1_007);
        assert_eq!(pool.blocks_since_seed, 7);
        assert!(pool.seed_coverage_alerted);

        // raw-tx 通道恢复：块 1007 拿到种子 → 下一个块边界收口时清零复位
        pool.apply_price_seed(U256::from(0x143c60fu64), 1_007);
        assert_eq!(pool.seed_coverage(), (1_007, 7));
        pool.observe_block(1_008);
        assert_eq!(pool.blocks_since_seed, 0);
        assert!(!pool.seed_coverage_alerted);

        // 块 1008 又断了 → 收口时计 1；同一块号重复观测（同块多 slice）不重复计
        pool.observe_block(1_009);
        assert_eq!(pool.blocks_since_seed, 1);
        pool.observe_block(1_009);
        assert_eq!(pool.blocks_since_seed, 1);
        assert_eq!(pool.coverage_open_block, 1_009);
    }

    #[test]
    fn test_seed_coverage_treats_missed_blocks_as_missing() {
        // 流重连导致整段块没被观测到：中间块按缺失计（保守，宁可多报一次）
        let mut pool = ElfomoFiPropPool::default();
        pool.observe_block(2_000);
        pool.observe_block(2_005);
        // (2000, 2005) 内 2000..2004 共 5 块缺失
        assert_eq!(pool.blocks_since_seed, 5);
        assert!(pool.seed_coverage_alerted);
    }

    // ---- 逐档 profile（宽度/偏离表）= 动态链上状态 ----

    /// 链上实测 profile word：块 70352699（旧表）与 70352700（keeper 改写后）。
    fn word_from_hex(h: &str) -> U256 {
        U256::from_be_slice(&alloy::hex::decode(h).expect("hex"))
    }

    fn legacy_word() -> U256 {
        word_from_hex("0000000000000000320064002800140019000a000f000a000a00050007000106")
    }

    fn kairos_word() -> U256 {
        word_from_hex("00000000000000003c0064002d00140023000a0019000a00140005000f000106")
    }

    #[test]
    fn test_band_profile_decode_real_words_and_roundtrip() {
        // 旧表：宽度 [1,5,10,10,20,100]、偏离 [7,10,15,25,40,50]
        assert_eq!(
            ElfomoBandProfile::decode_word(legacy_word()).unwrap(),
            legacy_profile()
        );
        assert_eq!(legacy_profile().encode_word(), legacy_word());

        // keeper 改写后：宽度不变、偏离 [15,20,25,35,45,60]
        let kairos = ElfomoBandProfile::decode_word(kairos_word()).unwrap();
        assert_eq!(kairos.count, 6);
        assert_eq!(kairos.widths, [1, 5, 10, 10, 20, 100]);
        assert_eq!(kairos.deviations, [15, 20, 25, 35, 45, 60]);
        assert_eq!(kairos.encode_word(), kairos_word());

        // 非法 word：n=0 / n>6 一律拒绝（fail-closed）
        assert!(ElfomoBandProfile::decode_word(U256::ZERO).is_none());
        assert!(ElfomoBandProfile::decode_word(U256::from(7u64)).is_none());
    }

    #[test]
    fn test_parse_band_profile_calldata() {
        // ABI: selector + keys_off + words_off + keys_len + key0 + words_len + word0
        let word = kairos_word();
        let mut input = Vec::new();
        input.extend_from_slice(&ELFOMO_BAND_PROFILE_SELECTOR);
        let u256_bytes = |v: U256| v.to_be_bytes::<32>().to_vec();
        input.extend_from_slice(&u256_bytes(U256::from(64u64))); // keys_off
        input.extend_from_slice(&u256_bytes(U256::from(128u64))); // words_off
        input.extend_from_slice(&u256_bytes(U256::from(1u64))); // keys_len
        input.extend_from_slice(&u256_bytes(U256::from(0u64))); // keys[0]
        input.extend_from_slice(&u256_bytes(U256::from(1u64))); // words_len
        input.extend_from_slice(&u256_bytes(word)); // words[0]

        let parsed = ElfomoFiPropPool::parse_band_profile_calldata(&input).expect("解析");
        assert_eq!(parsed, vec![(U256::ZERO, word)]);

        // 不是该 selector → None（价格种子通道继续走 `parse_update_prices_calldata`）
        let bad = [0u8; 100];
        assert!(ElfomoFiPropPool::parse_band_profile_calldata(&bad).is_none());
        // 数组长度不等 → None
        let mut bad_len = input.clone();
        bad_len[4 + 64..4 + 96].copy_from_slice(&U256::from(2u64).to_be_bytes::<32>());
        assert!(ElfomoFiPropPool::parse_band_profile_calldata(&bad_len).is_none());
        // 截断 → None（不 panic）
        assert!(ElfomoFiPropPool::parse_band_profile_calldata(&input[..40]).is_none());
    }

    #[test]
    fn test_apply_band_profile_recomputes_and_keeps_watermark() {
        let s = snapshot();
        let mut pool = ElfomoFiPropPool {
            levels: s.clone(),
            price_seed: s.price_seed,
            ladder: xlayer_ladder(),
            ..ElfomoFiPropPool::default()
        };
        let before = pool.local_orderbook(s.price_seed, s.vault_usdt0, s.vault_xeth);

        // keeper 在块 1_010 改写 profile → 本地模型立即切换
        pool.apply_band_profile(kairos_word(), 1_010);
        assert_eq!(pool.ladder.profile.deviations, [15, 20, 25, 35, 45, 60]);
        assert_eq!(pool.profile_block, 1_010);
        assert_eq!(pool.last_synced_block, 1_010);
        let after = pool.local_orderbook(s.price_seed, s.vault_usdt0, s.vault_xeth);
        assert_ne!(
            before.from_to_levels, after.from_to_levels,
            "偏离表变了，档位价格必须跟着变"
        );

        // 旧块/重放不得回退 profile，也不得把水位 stamp 到更旧块号
        pool.apply_band_profile(legacy_word(), 1_005);
        assert_eq!(pool.ladder.profile.deviations, [15, 20, 25, 35, 45, 60]);
        assert_eq!(pool.profile_block, 1_010);

        // 同块重放可推进（调用方按 tx_index 排序，最后一笔赢）
        pool.apply_band_profile(legacy_word(), 1_010);
        assert_eq!(pool.ladder.profile.deviations, [7, 10, 15, 25, 40, 50]);
        assert_eq!(pool.profile_block, 1_010);

        // 非法 word（n=0）→ 忽略，保持原 profile
        pool.apply_band_profile(U256::ZERO, 1_011);
        assert_eq!(pool.ladder.profile.deviations, [7, 10, 15, 25, 40, 50]);
        assert_eq!(pool.profile_block, 1_010);
    }

    #[test]
    fn test_merge_snapshot_keeps_newer_realtime_profile() {
        let s = snapshot();
        let mut pool = ElfomoFiPropPool {
            levels: s.clone(),
            price_seed: s.price_seed,
            ladder: xlayer_ladder(),
            ..ElfomoFiPropPool::default()
        };
        // 块 1_010 raw-tx 应用了新 profile（乐观头）
        pool.apply_band_profile(kairos_word(), 1_010);
        assert_eq!(pool.profile_block, 1_010);

        // 快照读块 1_005（规范头，落后）带的是旧 word → 不得回退
        let mut snap = s.clone();
        snap.profile_word = legacy_word();
        assert!(pool.merge_snapshot(snap, 1_005));
        assert_eq!(
            pool.ladder.profile.deviations,
            [15, 20, 25, 35, 45, 60],
            "newer realtime profile must not be rolled back by a lagging snapshot"
        );
        assert_eq!(pool.profile_block, 1_010);

        // 更新的快照（块 1_011）带新 word → 采用并推进水位
        let mut snap2 = s.clone();
        snap2.profile_word = kairos_word();
        assert!(pool.merge_snapshot(snap2, 1_011));
        assert_eq!(pool.profile_block, 1_011);
    }

    #[test]
    fn test_verify_model_uses_snapshot_profile_not_local() {
        // 快照(moment)的档位是"那一刻 profile"的函数；本地 profile 可能已更新。
        // 对拍必须用快照自带的 profile，否则会在快照落后时假阴性、误停报价。
        let s = snapshot();
        let mut pool = pool_with_snapshot();
        // 本地已切到新 profile（块 1_010）
        pool.apply_band_profile(kairos_word(), 1_010);
        assert_eq!(pool.ladder.profile.deviations, [15, 20, 25, 35, 45, 60]);

        // 快照是旧 profile（块 1_005）算出来的档位：模型公式本身没问题，
        // 用快照 profile 对拍必须通过（不能因为本地 profile 更新而判不可信）
        assert!(pool.verify_model_against_chain(
            &s.from_to_levels,
            &s.to_from_levels,
            s.price_seed,
            s.vault_usdt0,
            s.vault_xeth,
            &legacy_profile(),
            "test-snapshot-profile",
            1_005,
        ));
        assert!(pool.model_verified);

        // 用新 profile 去对旧档位必然不一致——这正说明"必须用快照 profile"
        let local_profile = pool.ladder.profile;
        assert!(!pool.verify_model_against_chain(
            &s.from_to_levels,
            &s.to_from_levels,
            s.price_seed,
            s.vault_usdt0,
            s.vault_xeth,
            &local_profile,
            "test-local-profile",
            1_005,
        ));
    }

    #[test]
    fn test_band_profile_slot_matches_chain_layout() {
        // mapping(uint256=>uint256) @ slot0, key=0 → keccak256(0x00..00‖0x00..00)
        assert_eq!(
            format!("{:#x}", ElfomoFiPropPool::band_profile_slot(0)),
            "0xad3228b676f7d3cd4284a5443f17f1962b36e491b30a40b2405849e597ba5fb5"
        );
    }
}

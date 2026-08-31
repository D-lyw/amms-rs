//! # BinaryFi propAMM (XLayer)
//!
//! 反汇编逆向过程、内部存储/calldata 布局、ladder 编码与链上验证数据，
//! 详见 `docs/binaryfi_prop_internal.md`（长期维护必读）。
//!
//! BinaryFi 是 XLayer 上的 proprietary AMM（PropAMM）：链下做市引擎定期通过
//! `update(...)` 交易向引擎合约提交带签名的资产价格，池子合约对外提供线性报价
//! `quote()`，单笔输出受引擎侧 per-asset 输出上限与金库余额约束。
//!
//! ## 报价模型（structLogs 反汇编 + 链上采样逐位验证；2026-08 引擎升级后
//! 按账户费率 `fee_ppm` 输入侧扣费，旧锚点块 67160388 及此前样例不再适用）
//!
//! 引擎以 USDT0 为 numéraire（`p0 = 100` 固定，无点差），每资产维护整数中间价
//! `price`（2 位小数定点）与 buy/sell ladder 点差偏移（独立、可不对称）。
//! 费率 `fee_ppm`（ppm，1e6=100%）是引擎 **per-account storage**：
//! `getFee(account)` 读取（聚合器白名单 = 0，普通账户 = 运营方 `setFee` 现值，
//! 历史 500→1000→200），本地 `fee_ppm` 由快照批量合约带回 + `FeeUpdated`
//! 事件实时同步；**所有价格/阶梯均存无费口径**，报价时输入侧扣费
//! `rem = in − in×fee_ppm/1e6`（整数除法），fee 变更无需重校准价格：
//!
//! - 卖方向：`raw_i = price_i - sellOff_i`（`sellOff = bidOffsetRaw × scale/10000`；
//!   `bid_i = price_i - sellOff_i`；历史 fee=1000 时等价
//!   `bid = floor((price×999 - sellOff×1000)/1000)`）
//! - 买方向：`ask_i = price_i + askOff_i`（`askOff = askOffsetRaw × scale/10000`）；
//!   BUY 报价状态为小额报价 `q0j_j = floor(10^(dj+2) / ask_j)`（无费，`in=10^d0`）
//! - `USDT0→j`（BUY）：先扣费 `rem = in − in×fee_ppm/1e6`（整数除法，与 SELL 同款；
//!   链上逐位实证 fee=1000：tx1 in=44,291,018 → 324,080,619,644,034,278、
//!   tx2 in=122,156,425 → 891,476,871,940,974,505；**不能**用 `in×(1e6−fee)/1e6`
//!   替代——非整倍数输入有 ~1e-4% 偏差），
//!   `linear = floor(rem × 10^(dj+2) / (ask_j × 10^d0))`
//!   （fee=200 实测：asset4~9 BUY 全 -200.0ppm，逐位一致），再按阶梯上限截断：
//!   - 饱和型（阶梯容量 ≤ 金库余额，大额 probe 可观测 maxOut）：`out = min(linear, maxOut)`
//!   - 超阈值归零型（阶梯容量 > 金库余额，大额 probe = 0）：`linear > 金库余额 → 0`，
//!     否则原样返回（有效上限 = 金库余额，由 `getAssetReserves` 观测）
//! - `i→USDT0`（SELL）：多档阶梯逐档累加
//!   `out = Σ_k (price − w_k) × min(rem, qty_k×R) × 10^(d0−2) / 10^di`，
//!   `rem = in − in×fee_ppm/1e6`（费率先扣；链上实测 in<1000 时与 in×999/1000 不同）
//!   逐档递减，首档 weight = 小额报价偏移。阶梯来自 L2 update calldata `data0`
//!   （每档 24bit = 12bit weight + 12bit qty，最多 10 档），储备 R 来自快照
//!   100 整枚 probe + ladder 反推（链上长期稳定、不随 Swap 变化）；
//!   阶梯未知时回退单档线性（raw 有理数 + maxIn 截断 + 超容量归零）
//! - `i→j`（非 USDT0）：两段式（与引擎一致，含中间截断）：
//!   `v = floor(in × raw_i × 10^(d0-2) / 10^di)`（受 maxIn_i 截断 + 归零），
//!   `out = floor(v × 10^(dj-d0+2) / ask_j)`（第二段 BUY 无额外费率因子——
//!   费已在两段输入侧各扣一次，链上实测；直接 `0→j` 才扣一次），
//!   再按上述 BUY 规则截断/归零
//! - `i→USDT0` 消耗量受 SELL 阶梯上限截断（`consume = min(rem, maxIn_i)`，
//!   `rem = in − in×fee_ppm/1e6`；饱和时消耗满 `maxIn_i`，输出 fee 无关），
//!   `maxIn = ladderWeight_sell × engineReserve`，由 100 整枚 probe 精确恢复；
//!   多档阶梯资产（如 DOG）100 整枚 probe 与单档线性不兼容，本地仅小额区
//!   可精确复刻（生产 update 路径由 `apply_l2_update` 直接携带精确 raw）
//!
//! 验证样例（新引擎锚点，SKHYx price=13984/askOff=3/sellOff=3）：
//! - `0→SKHYx, in=1e6` → `7,142,346,464,574,247`（= q0j，ask=13987）
//! - `SKHYx→0, in=1e15` → `139,670`（bid = 13967）；
//!   `SKHYx→0, in=1e18` → `139,670,160`（= raw×10，raw=13,967,016，整数 bid 低估 120）
//! - xSOL（price=7263/sellOff=0）：`xSOL→0, in=1e11` → `3,627,868,500`
//!   （= raw×500，raw=7,255,737；整数 bid 7,255 低估 182,500）
//! - CRCLx（price=6333/sellOff=?）：`CRCLx→0, in=1e20` → `6,326,667,000`
//!   （raw=6,326,667；整数 bid 6,326 低估 667,000）
//! - xETH SELL 超容量归零：`in=9.13e18` → `17,343,984,269`（≤ 金库 17,347,345,227），
//!   `in=9.14e18` → `0`（> 金库）
//! - asset6 @ 0x402fdb5（price=76925/sell=24/ask=24）：`0→asset6 in=1e6` →
//!   `1,298,912,266,566,167`；`asset6→0 in=1e14` → `76,862`
//! - xETH @ 0x403047e（price=189935/sell=38/ask=39）：SELL 逐位一致；BUY 小额
//!   链上 `526,124,634,901,868` 与公式派生 `526,124,627,580,616` 差 7.3e6
//!   （相对 1.4e-5，仅 xETH 一个资产；快照 q0j 直接取链上 quote 不受影响，
//!   update 派生偏差由周期快照纠正）
//!
//! ## 三层数据同步（事件驱动，无轮询）
//!
//! 1. **L1 — Swap 事件**（池子 `0x2d651e...`）：
//!    `Swap(sender, tokenIn, tokenOut, amountIn, amountOut)` 更新**部署级共享
//!    金库账本**（`reserves[in]+=in`、`reserves[out]-=out`、
//!    `buy_ladder_remaining[out]-=out`）：单部署所有虚拟子池共用 vault，任何
//!    Swap（含跨 pair 抽干）都必须全局可见；费率/价格锚定仍限本 exposed pair
//!    （防跨 pair 污染），价格未知时才用 out/in 锚定兜底。
//! 2. **L2 — flashblocks 原始交易增强**（仅实时流）：
//!    引擎 `update` 日志本身 data 为空；`xlayer_flashblocks` 解析器在 raw bytes
//!    边缘通过 `keccak256(raw) == tx_hash` 定位交易，RLP 解码后解析
//!    `0x024b94f6` calldata，把 `(price, blockNumber, data0..2)` 5 个 word 注入
//!    日志 data，随后 `sync()` 直接用 price 重算所有相关 pair 的费率，零 RPC。
//!    2026-08-19 引擎升级后 MM 改用批量 update `0x34f7f748`：一笔交易携带
//!    N 条资产记录并发出 N 条 Update 日志（data 仍为空、topic[1]=asset_idx）；
//!    enrich 按日志 asset_idx 从批量记录挑对应 `(price, data0, data1)` 注入，
//!    点差偏移仍由 ladder 前 16 位解析（与单资产格式同编码，链上实测吻合；
//!    记录内显式 askOff/bidOff 字段不参与报价）。
//! 3. **L3 — 无 raw bytes 的 update 日志**（canonical get_logs 路径）：
//!    把相关 pair 标记 stale 并返回 `SyncAction::AsyncUpdate`，由 StateSpace 的
//!    pending_sync_queue 触发 `update()`：一次 `GetBinaryFiPropStateBatchRequest`
//!    静态调用批量拉取余额 + stale pair 的 `pool.quote`，从同块 `(0→j)/(j→0)`
//!    精确恢复每资产 bid/ask（含点差），再本地推导全部费率。
//!
//! ## reserves 快照写回：rebase-merge（2026-08-31 事故设计抉择，长期维护必读）
//!
//! ### 为什么 reserves 特殊：纯累积量，无自回归
//!
//! `reserves` 是**纯累积量**：swap 事件只在当前值上做 `+in/−out`（见 L1），
//! **没有任何事件会把链上真值写进来**。唯一写真值的是快照 `vaultBalances`
//! （`vault.balanceOf`），而快照块 `snap_block` 取自 canonical head
//! （`provider.get_block_number()`）；XLayer flashblock 事件流领先 canonical
//! 1-2 块 → 快照读到的是**旧一截的真值**。
//!
//! 对比其它字段（price/offset/q0j/sell_raw/max_*/buy_ladder_remaining）：
//! 它们都是"最新值"，由 update 事件每 1-3 块直接写入当前链上真值——即使被
//! 快照回退 1-2 块，下一次 update 立即纠正（**自回归**）。reserves 没有这种
//! 重置点：快照把旧值写进来后，swap 事件继续在错误基线上累积，错误持续到
//! 下一次快照——而下次快照读的还是旧块 → 幻影容量持续复活。
//!
//! ### 历史（为什么不能退回旧方案）
//!
//! - v1.19.6 及以前：`should_skip_async_apply` 按 `last_synced_block` 丢弃
//!   旧快照（BinaryFi 每块都有 MM update 事件，last_synced_block 恒新）→
//!   快照通道完全失效，reserves 漂移无法回归（2026-08-19 事故：引擎 quote
//!   606.7M 而真实金库 280.7M）。
//! - v1.19.7：对 BinaryFi 去掉 skip、无条件应用快照；当时假设快照字段与
//!   事件字段不相交 → 暴露 `reserves` 被旧值覆盖（2026-08-31 事故：快照把
//!   抽干前 649M 打回事件账本已扣到的 6,988，幻影容量持续 15s、9 笔失败）。
//! - v1.19.8（本实现）：rebase-merge，实时性（事件账本逐块精确）与收敛性
//!   （漂移每 15s 被链上真值回正）两者兼顾。
//!
//! ### 解法：ReservesDeltaLedger（checkpoint + redo log）
//!
//! 语义 `reserves = 快照(S) + Σ(块 > S 的 swap 事件净变化)`：
//! - 事件路径：`sync()` 每笔 swap 把 `(block, +in, −out)` 记入环形缓冲
//!   （同块累加、乱序/重复忽略、容量 128 块挤出跟踪 `evicted_up_to`）。
//! - 快照路径：`apply_snapshot` 调 `ledger.rebase`——
//!   `S ≥ last_event_block` → 直接锚定（快照覆盖全部事件，清空账本）；
//!   `base ≤ S < last_event_block` → `current + (快照 − 基底) − Σ(base, S]`，
//!   精确把基底推进到 S（漂移每 15s 被链上真值收敛）；
//!   重启后首次（base=0）→ 快照 + 缓冲内 > S 的事件 replay；
//!   `S < base` 或窗口挤出/无块号日志 → 退化为 guard（保持事件账本，不写回，
//!   绝不比旧行为差）。
//! - 代价：每池约 24KB 内存（128 块 × 12 资产 × 16B），可忽略。
//!
//! ### 归零门控（配套）
//!
//! 锚定后 `reserves == 0` 是"已空"（权威，`vault_known` 返回 `Some(0)`，
//! SELL 超容量归零、BUY 截断为 0）；未锚定 0 仍是"未知"（None，维持历史
//! 兜底）。防止"账本已精确到 0 却被当未知放行"。
//!
//! ### 长期维护注意
//!
//! - 真实事件写 `reserves` 的路径**只有 `sync()` 一处**（`anchor_rate` 是
//!   保留的测试/旧调用点）；新增写路径必须同步记入 ledger，否则 rebase 偏差。
//! - `simulate_swap_mut` 只应作用于克隆/测试池，不得污染真实池状态。
//! - 快照块号必须与"快照读取时对应的链上块"一致（rebase 数学依赖 S 与
//!   `vaultBalances` 同块）；不要用 `BlockId::latest()` 的隐式块。
//!
//! ## 协议形态与部署对照（BinaryFiEvm，2026-08 源码级核验）
//!
//! BinaryFiEvm 是**单一 operator 的链上订单簿 DEX**：链下做市引擎（operator）
//! 通过 `update` 提交带签名的价格/点差/阶梯，定价核心只通过 router 可达。
//! 同一份 router 实现部署在 XLayer 与 Base 两链（bytecode 除 WETH 常量外
//! 逐字节一致，见 `BINARYFI_ROUTER_ADDRESS`），因此本模块在 XLayer 上实证
//! 反推的费率/阶梯/时效规则对 Base 部署同样成立。
//!
//! - **router swap 入口**：`swap(address tokenIn, address tokenOut, uint256 amountIn,
//!   uint256 minAmountOut, uint256 expiry)`，选择器 `0x7a950f99`（exact-in）。
//!   `expiry` 是 **tx 级 deadline**：router 校验 `block.timestamp <= expiry`，
//!   **不是** `BINARYFI_QUOTE_TTL_BLOCKS` 那 5 块报价时效（时效在引擎 quote
//!   内部 `0x6ee50667` 路径，见上）。router 无 taker callback（无 ERC-1271/
//!   flash 回调），本地模拟无需回调路径。结算：router 从调用方 `transferFrom`
//!   拉 `tokenIn` 进 vault、从 vault 发 `tokenOut`；原生 ETH 统一按 WETH
//!   归一化（XLayer WETH9 = `0xe538905cf8410324e03a5a23c1c177a474d59b2b`，
//!   Base WETH = `0x4200000000000000000000000000000000000006`）。
//! - **BinaryFiEvmAdapter**（`0x4d0e78ba4c116f576fbe2908c75696d9041e5f04`）：
//!   聚合器用的**无状态清仓适配器**，`sellBase/sellQuote(to, pool, moreInfo)`，
//!   `moreInfo = abi.encode(tokenIn, tokenOut)`；流程为
//!   `balanceOf>0` 检查 → `forceApprove(pool, balance)` →
//!   `pool.swap(tokenIn, tokenOut, balance, 0, block.timestamp)`（minOut=0，
//!   滑点由上游聚合器保证）→ `forceApprove(pool, 0)` → 剩余 `tokenOut` 转给
//!   `to`、剩余 `tokenIn` 退回调用方。XLayer 部署为该源码的旧版（无原生 ETH
//!   处理）；Base 同地址为新版（多 `weth()`/`receive()`）。该合约仅供理解
//!   入口语义与事件结构，报价公式细节均在引擎内，本地 `engine_quote` 的
//!   两段式公式以 `debug_traceCall`/回放实证为准。
//! - **引擎函数入口参考**（bytecode 反汇编，0x 前缀为选择器 → 内部偏移）：
//!   `update 0x024b94f6 → 0x0362`、
//!   `swap(account,tokenIn,tokenOut,amountIn,extraFeePpm) e343fe12 → 0x1207`、
//!   `quote(account,tokenIn,tokenOut,amountIn) 08bb9367 → 0x0438`、
//!   `getAssets() 67e4ac2c → 0x0c37`、`getAssetReserves() 17ec4199 → 0x080c`、
//!   `getFee(account) b88c9148 → 0x10af`、`vault() fbfa77cf → 0x14e1`、
//!   `时效 6ee50667 → 0x0c9f`（内部 revert `0x86fa3e43`，外层 quote 捕获转 0）。
//!   （函数名与选择器由 PAmm1010Router 开源源码 + keccak 双向核验，2026-08。）
//!
//! ### PAmm1010Router（XLayer 池子 = 官方 Router，已验证源码）
//!
//! Router 是**薄代理**（ReentrancyGuard）：owner 可 `setTarget` 切换核心，
//! 链上实测 `target()` = 引擎 `0x6558dbe4...`（2026-08-19 迁移后新部署）；核心接口 `IPAmm1010` 的
//! `quote/swap` 首参均为 `account`（router 转发时 = `msg.sender`），
//! 核心 `swap` 末参为 `extraFeePpm`（router 普通 swap 传 0）。
//!
//! - 公开入口一：`swap(tokenIn, tokenOut, amountIn, minAmountOut, expiry)`
//!   （`0x7a950f99`，extraFeePpm=0，adapter/常规聚合器走此路径）；
//!   入口二：`swapWithExtraFees(tokenIn, tokenOut, amountIn, minAmountOut,
//!   expiry, extraFeePpm)`（`0x9e51844f`，**带额外费率的路径**——竞争对手若
//!   走此入口，成交价低于纯 swap 路径；本地模拟按 0 费率路径可能高估输出，
//!   识别竞争者套利时需留意）。
//! - `_swap` 结算顺序：`block.timestamp > expiry → revert Expired` →
//!   核心 `swap(msg.sender, tokenIn, tokenOut, amountIn, extraFeePpm)` →
//!   `amountOut < minAmountOut → revert InsufficientOutput` →
//!   `tokenIn` 进 vault（原生 ETH 走 WETH deposit；ERC20 走 transferFrom）→
//!   `tokenOut` 从 vault 出（原生 ETH 走 WETH withdraw；ERC20 走 transferFrom）
//!   → emit `Swap(msg.sender, tokenIn, tokenOut, amountIn, amountOut)`。
//! - 原生 ETH 支持：`NATIVE_ETH = 0xEeee...EeE`（ethIn 要求 `msg.value ==
//!   amountIn`、ethOut 要求 `msg.value == 0`）；核心/引擎内部统一按 WETH 处理。
//! - `getFee(account)`：核心按账户收取费率 → 引擎 bytecode 无硬编码 999/1000
//!   的原因（费率在核心 storage，per-account）；本地 `fee_ppm` 由快照批量合约
//!   `getFee(recipient)` 带回 + `FeeUpdated` 事件（topic0 `0xc58b3024…`，data
//!   前 32 字节 = 新 fee）实时同步，报价输入侧按 `fee_ppm` 扣费。
//! - Swap 事件 topics = `[sig, user, tokenIn, tokenOut]`、data = `(amountIn,
//!   amountOut)`，与模块 L1 解析（`token_in=topics[2]`、`token_out=topics[3]`）
//!   逐位一致。

pub mod factory;

use std::collections::VecDeque;

use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{address, b256, keccak256, Address, Bytes, LogData, B256, U256},
    providers::Provider,
    rpc::types::Log,
    sol_types::SolValue,
};
use serde::{Deserialize, Serialize};
use tracing::instrument;

use crate::amms::{
    amm::{AutomatedMarketMaker, SyncAction, AMM},
    error::AMMError,
    Token,
};

mod types;
pub use types::*;

// ============================================================================
// Constants
// ============================================================================

/// XLayer 链 ID
pub const BINARYFI_CHAIN_ID: u64 = 196;
/// 资产数量（getAssets 返回 12 个资产）
pub const BINARYFI_ASSET_COUNT: usize = 12;
/// `ReservesDeltaLedger` 环形缓冲容量（块数）。rebase 窗口只需覆盖 canonical
/// 滞后 flashblock 的 1-2 块，128 是宽裕余量；被挤出时退化为 guard 语义
/// （S ≥ last_event_block 才覆盖），不会回退事件账本。
pub const BINARYFI_LEDGER_CAPACITY: usize = 128;
/// XLayer 默认部署：池子合约（Swap 事件、quote 报价）。
/// 2026-08-19 BinaryFi 已迁移：旧部署 0x2d651e3f... / 0xeacf260a... 不再被官方路径使用。
/// 配置化部署时地址应由 poolindex/loader 经 `BinaryFiPropFactory::new` 传入，
/// 此常量仅作为默认值与文档参考。
/// 池子 = 官方 **PAmm1010Router**，swap 入口
/// `swap(tokenIn, tokenOut, amountIn, minAmountOut, expiry)`（选择器
/// `0x7a950f99`，exact-in）：router 校验 `block.timestamp <= expiry`（tx 级
/// deadline，非 5 块报价 TTL）、无 taker callback、`transferFrom` 拉入
/// `tokenIn` 进 vault、从 vault 发 `tokenOut`；另有带额外费率的
/// `swapWithExtraFees`（`0x9e51844f`）与原生 ETH 入口（`0xEeee...EeE`）；
/// 详见模块顶部“协议形态与部署对照”。
pub const BINARYFI_POOL_ADDRESS: Address = address!("0x4a8e34cfe4f643132e8de0e9752054a9ac862816");
/// XLayer 默认部署：引擎合约（update 事件、价格提交）
pub const BINARYFI_ENGINE_ADDRESS: Address = address!("0x6558dbe4c1bb50ed881e54105242491d03a98118");
/// XLayer 默认部署：金库合约（持有资产）
pub const BINARYFI_VAULT_ADDRESS: Address = address!("0x9b169052Ee1569Ec5bDF51DbF48D2962526cF6D9");
/// XLayer 默认部署：quote 的 recipient（Router）。
/// 官方 Router 为 PAmm1010Router（= 池子地址 0x4a8e34cf，与 poolindex 配置一致）；
/// 该常量仅作 `Default` 兜底初值，生产路径由 loader 从 ndjson config 覆盖。
/// 跨链对照：Base 同实现 router = `0x98537a558449bC1437C2e897d640847AF6102549`
/// （bytecode 与 XLayer 除 WETH 常量外逐字节一致；XLayer WETH9 =
/// `0xe538905cf8410324e03a5a23c1c177a474d59b2b`，Base WETH =
/// `0x4200000000000000000000000000000000000006`）。
pub const BINARYFI_ROUTER_ADDRESS: Address = address!("0x4a8e34cfe4f643132e8de0e9752054a9ac862816");
/// BinaryFiEvmAdapter（XLayer 部署，2026-08 反汇编 + 开源源码双向核验一致）：
/// 聚合器用无状态清仓适配器 `sellBase/sellQuote(to, pool, moreInfo)`，
/// `moreInfo = abi.encode(tokenIn, tokenOut)`，内部
/// `forceApprove(pool, balance) → pool.swap(tokenIn, tokenOut, balance, 0,
/// block.timestamp) → forceApprove(pool, 0)` 后把剩余 tokenOut/tokenIn 退回。
/// Base 同地址为新版（多 `weth()`/`receive()` 原生 ETH 处理）。仅供理解
/// 入口语义与事件结构；报价公式在引擎内，见模块顶部说明。
pub const BINARYFI_ADAPTER_ADDRESS: Address =
    address!("0x4d0e78ba4c116f576fbe2908c75696d9041e5f04");
/// 池子 Swap 事件签名
pub const BINARYFI_SWAP_EVENT: B256 =
    b256!("cd3829a3813dc3cdd188fd3d01dcf3268c16be2fdd2dd21d0665418816e46062");
/// 引擎 Update 事件签名
pub const BINARYFI_UPDATE_EVENT: B256 =
    b256!("af186e2e77ac28f0c051cdd1e2b3b92924e34b314650186bbc14742e373751c8");
/// 引擎 update 函数选择器
pub const BINARYFI_UPDATE_SELECTOR: [u8; 4] = [0x02, 0x4b, 0x94, 0xf6];
/// 引擎批量 update 函数选择器（`0x34f7f748`，2026-08-19 引擎升级后新格式）：
/// 一笔交易携带 N 条资产记录（6 words/条）并发出 N 条 Update 日志
/// （日志 data 为空、topic[1] = asset_idx）。
pub const BINARYFI_BATCH_UPDATE_SELECTOR: [u8; 4] = [0x34, 0xf7, 0xf7, 0x48];
/// 批量 update 单条记录长度（6 words = 192B）：
/// `(asset_idx, price, askOff, bidOff, data0=sellLadder, data1=buyLadder)`
pub const BINARYFI_BATCH_UPDATE_RECORD_LEN: usize = 32 * 6;
/// 引擎 setFee 函数选择器（`setFee(uint256)`，运营方调用；fee 单位 = ppm，1e6 = 100%）。
/// 链上实证：块 0x4073356 由 0x2db200f40f47… 调用设置 fee=0xc8(200)，该块内
/// `getFee` 从 1000 变为 200。
pub const BINARYFI_FEE_SELECTOR: [u8; 4] = [0x6f, 0x7c, 0x86, 0x8b];
/// 引擎 FeeUpdated 事件签名（data 前 32 字节 = 新 fee ppm）。
/// 链上实证：`setFee` 会发该事件（topic0 = 0xc58b3024…，data = 0xc8 = 200）。
pub const BINARYFI_FEE_EVENT: B256 =
    b256!("c58b3024a07432cfc160ea128eebc11329d444bb61bf7098f09bb6567b943c66");
/// 默认费率（ppm，1e6 = 100%）：未知/未同步时的兜底。
/// 历史实证：锚点块 67302528 = 500、失败交易窗口 6748xxxx = 1000、
/// 2026-08-10 16:16 UTC（块 0x4073356）起 = 200；聚合器 0xd9eccc… = 0（白名单）。
/// 除白名单外普通账户统一取同一默认值。
pub const BINARYFI_DEFAULT_FEE_PPM: u64 = 1_000;
/// update calldata 总长度（实测 452B）= selector(4) + head 6 words(192) +
/// data0..2(96) + data_len(32) + sig_len(32) + sig(96)
pub const BINARYFI_UPDATE_CALLDATA_LEN: usize = 4 + 32 * 6 + 96 + 32 + 32 + 96;
/// binaryFI 报价时效窗口（块数）：引擎 per-asset `lastUpdateBlock`（mapping
/// keccak256(assetId,9) 槽 +0）后链上 `pool.quote()` 仅在本窗口内有效，超过
/// `price_updated_block + BINARYFI_QUOTE_TTL_BLOCKS` 的块返回 0（内部 0x6ee50667
/// revert 0x86fa3e43，外层 quote 捕获转 0，非 revert）。
/// 链上逐块实测（67430640 起）：NVDAx(lastUpdate=67430638) 在 diff 2~5 正常、
/// diff=6 起归 0；SPYx(lastUpdate=67430639) 在 diff 1~5 正常、diff=6 归 0；
/// asset2/3 每 1~3 块就被 MM 重新 update（asset2: 67430640/42/43/48…，
/// asset3: 67430640/44/47/48…），5 块窗口从不过期——这就是“差 8 块仍新鲜”
/// 的真相（67430648 时真实 lastUpdate 已是 67430643/67430647），并非时效更长。
/// 本地用模块自身数据 `price_updated_block`（L2 用 calldata 注入块号、L3 用
/// 事件块号）与 `last_synced_block` 判定，不引入墙钟 TTL。
pub const BINARYFI_QUOTE_TTL_BLOCKS: u64 = 5;
/// 单笔输出截断比例（96% 池子余额，本地近似）
pub const BINARYFI_MAX_OUTPUT_BPS: u64 = 9600;
/// USDT0 的引擎报价默认值（2 位小数定点）
pub const BINARYFI_PRICE0_DEFAULT: u64 = 100;
/// 价格未知资产的默认点差（ask-bid，价格单位；SKHYx 实测 = 8）
pub const BINARYFI_DEFAULT_SPREAD: u64 = 8;
/// 每次 swap 模拟消耗的默认 gas
pub const DEFAULT_SWAP_GAS: u64 = 250_000;

/// `fee_ppm` 序列化兜底（旧状态无该字段时 = 1000，行为与历史一致）
fn default_fee_ppm() -> u64 {
    BINARYFI_DEFAULT_FEE_PPM
}

/// 确定性派生 BinaryFi 虚拟子池地址（token_a < token_b 排序，无方向歧义）。
///
/// 与 UniswapV4 的 poolId 不同：BinaryFi 链上不存在 pair 身份，虚拟地址必须由
/// amms 与下游（poolindex/loader）用同一函数链下派生，保证 StateSpace key 与
/// 拓扑/ndjson 中的地址一致。输入含真实池子地址 → 天然支持跨链/多部署。
pub fn binaryfi_virtual_address(pool: Address, token_a: Address, token_b: Address) -> Address {
    let (a, b) = if token_a < token_b {
        (token_a, token_b)
    } else {
        (token_b, token_a)
    };
    let digest = keccak256(("BinaryFiEvm", pool, a, b).abi_encode());
    Address::from_word(digest)
}

/// BinaryFi propAMM 池子。
///
/// 对外以"具体 token pair 的虚拟子池"呈现（与 UniswapV4/Caliber 一致）：
/// StateSpace 中一个实例 = 一个可交易对；`virtual_address` 为 StateSpace key，
/// `exposed_pair` 限定对外暴露的资产对。实例内部仍保留全 12 资产数组
/// （numéraire 假设、全局 asset_idx、报价公式与引擎一致），未暴露资产数据
/// 不参与任何对外计算。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryFiPropPool {
    /// 池子真实合约地址（链上批量调用 + Swap 日志匹配；不再是 StateSpace key）
    pub pool_address: Address,
    /// StateSpace key（虚拟子池地址）；ZERO = canonical 模式（等于 pool_address）
    #[serde(default)]
    pub virtual_address: Address,
    /// 对外暴露的资产对（全局资产索引，与引擎资产列表对齐）；None = 暴露全部
    #[serde(default)]
    pub exposed_pair: Option<(usize, usize)>,
    /// 引擎合约地址（update 日志来源）
    pub engine_address: Address,
    /// 金库合约地址
    pub vault_address: Address,
    /// quote 批量读取合约使用的 recipient（Router，随部署配置传入）
    #[serde(default)]
    pub router_address: Address,
    /// 引擎按账户费率（ppm，1e6 = 100%）：报价时输入侧扣
    /// `rem = in − in×fee/1e6`。链上 `getFee(account)`（0xb88c9148）按账户返回，
    /// 除白名单（AGG=0）外普通账户统一取同一默认值（500→1000→200 随时间变，
    /// 2026-08-10 16:16 UTC 起 = 200）。由 fee 事件（0xc58b3024…）/批量快照
    /// getFee 实时更新；序列化兜底 = 1000（与历史行为一致）。
    #[serde(default = "default_fee_ppm")]
    pub fee_ppm: u64,
    /// 链 ID
    pub chain_id: u64,
    /// 创建区块号（StateSpace 扫描起点）
    pub created_block: u64,
    /// 最后同步区块号
    pub last_synced_block: u64,
    /// 12 个资产（index 与引擎资产索引对齐）
    pub assets: Vec<Token>,
    /// 引擎中间价（USDT0 计价定点；点差为奇数时中间价为分数，存 floor）
    pub prices: Vec<U256>,
    /// 每资产点差 ask-bid（价格单位；USDT0 恒为 0）
    pub spreads: Vec<u64>,
    /// 每资产卖方向偏移（bid = price - bid_offset）。
    /// 无费口径：`bid_offset = sell_off`（fee 在报价时输入侧扣；fee=1000 时
    /// 等价历史 `bid_offset = ceil(price/1000) + sell_off`）；快照路径直接存
    /// `bid_offset=0, price=bid`（0 = 显式无偏移）
    #[serde(default)]
    pub bid_offsets: Vec<u64>,
    /// 每资产买方向偏移（ask = price + ask_offset；仅供诊断/参考，BUY 报价走 q0j）
    #[serde(default)]
    pub ask_offsets: Vec<u64>,
    /// 每资产 BUY 小额报价状态 q0j（0→j, in=10^d0 的链上 quote）。
    /// 快照取链上含费 quote 后按 `fee_ppm` 无费化（`unfee_quote`），update 由
    /// `floor(10^(dj+2)/(price+ask_off))` 派生（无费）。
    /// 直接 `0→j` 报价 `linear = floor(in × q0j / 10^d0)`；跨资产第二段用
    /// `ask = price + ask_off`（快照从 q0j 反推，`recover_ask_eff`）
    #[serde(default)]
    pub q0j: Vec<Option<U256>>,
    /// 每资产 SELL 精确有理数分子 `raw = price - sell_off`（无费）
    /// （SELL 输出 `= floor(in × raw × 10^(d0-2) / 10^di)`，链上实测
    /// 大额 quote 与整数 bid 不同，含小数部分）。
    /// 快照从小额 j→0 quote（`bid = raw`）与 100 整枚 probe 反推；
    /// update 由 `apply_l2_update` 直接计算。`None` = 未知（多档阶梯或 probe 归零，
    /// 兜底用 `bid`，等价整数 bid 线性近似）
    #[serde(default)]
    pub sell_raw: Vec<Option<U256>>,
    /// 引擎每资产 scale（内部价格 = calldata price × scale/10000；asset[2]=100000，其余 10000）
    #[serde(default)]
    pub price_scales: Vec<u64>,
    /// 每资产买入是否被引擎禁用（0→j quote 恒为 0；卖出不受影响）
    pub buy_disabled: Vec<bool>,
    /// BUY（0→j）输出上限（饱和型）：大额 probe 被截断时 q_big 即 maxOut；
    /// 引擎 `out = min(linear, maxOut)`；`None` = 未知/未达上限
    #[serde(default)]
    pub max_outputs: Vec<Option<U256>>,
    /// BUY 超阈值归零型（0→j 大额 probe = 0 且小额 > 0：阶梯容量 > 金库余额）。
    /// 引擎在 `linear > 金库余额` 时返回 0（而非饱和截断），有效上限 = 金库余额
    #[serde(default)]
    pub buy_zero_over_vault: Vec<bool>,
    /// SELL（j→0）输入上限 maxIn = ladderWeight × engineReserve；`None` = 未知/未达上限
    #[serde(default)]
    pub max_inputs: Vec<Option<U256>>,
    /// 每资产 SELL 多档阶梯（L2 update calldata data0 解析；weight 已按
    /// scale/10000 折算为内部价格单位，qty = 引擎储备倍数）。引擎 SELL 输出
    /// 逐档累加：`out += (price − w_k) × min(rem, qty_k×R) × 10^(d0−2)/10^di`，
    /// `rem = in − in×fee_ppm/1e6` 逐档递减。`None` = 未知（仅快照路径无阶梯信息，
    /// 回退单档线性）
    #[serde(default)]
    pub sell_ladders: Vec<Option<Vec<(u32, u32)>>>,
    /// 每资产 BUY 多档阶梯（L2 update calldata data1 解析；weight 已折算内部单位）。
    /// BUY 线性区用首档 ask（= price + w1），封顶 = Σqty×R（链上实测：asset2
    /// in≥5e10 平顶 62,000,000 = (767+1150+1183)×20000，逐位一致）。
    /// 报价路径通过 `buy_ladder_remaining` 消费逐档容量。
    #[serde(default)]
    pub buy_ladders: Vec<Option<Vec<(u32, u32)>>>,
    /// 每资产 BUY 剩余阶梯容量（= Σqty×R，随 Swap 消费递减；update 重置）。
    /// update 路径由 `buy_ladders × ladder_reserves` 精确推导；快照路径取大额
    /// probe 观测的 maxOut；`None` = 未知（不截断）。BUY 报价封顶优先取它，
    /// 其次才用快照 `max_outputs`。
    #[serde(default)]
    pub buy_ladder_remaining: Vec<Option<U256>>,
    /// 每资产引擎阶梯储备 R（quote 内部使用；与金库余额不同，链上长期稳定、
    /// 不随 Swap 变化）。由快照大额 probe（100 整枚 SELL 封顶）+ ladder 反推；
    /// `None` = 未知（ladder 路径不可用，回退单档线性）
    #[serde(default)]
    pub ladder_reserves: Vec<Option<U256>>,
    /// 池子各资产余额（输出截断基准）
    pub reserves: Vec<U256>,
    /// 共享金库事件净变化账本（环形缓冲）：快照 rebase-merge 用。
    /// 序列化跳过（`skip_serializing`），重启后 `base_block = 0`，
    /// 首个快照走 replay 分支精确恢复，无需落盘/迁移。
    #[serde(default, skip_serializing)]
    pub ledger: ReservesDeltaLedger,
    /// 有向费率 num/den，index = i * N + j（对角为空）
    pub rates: Vec<Rate>,
    /// 待批量刷新 pair（index 同上）
    pub stale_pairs: Vec<usize>,
    /// 每资产最近一次价格更新块号（0 = 未知）
    pub price_updated_block: Vec<u64>,
    /// price0 是否已通过 Swap 锚定标定
    pub price0_calibrated: bool,
}

/// 共享金库 `reserves` 的事件净变化账本（环形缓冲）。
///
/// 语义：`reserves = 快照(S) + Σ(块 > S 的 swap 事件净变化)`。事件路径逐块
/// 记录 Δ（同块多笔累加、乱序/重复忽略）；快照路径在 `apply_snapshot` 时
/// rebase-merge：快照块 S 落在窗口内 → 精确合并（漂移每 15s 被链上真值收敛），
/// 落在窗口外 → 退化为「S ≥ last_event_block 才覆盖」的 guard，绝不回退
/// 事件账本。这是 reserves 与其它"最新值"字段的本质区别——reserves 是纯
/// 累积量，事件只在当前值上增减、从不写真值，唯一写真值的快照又滞后
/// canonical 1-2 块；不合并则幻影容量持续复活（2026-08-31 事故根因）。
/// 完整设计抉择（历史/为什么 guard 不够/退化语义）见模块顶部
/// "reserves 快照写回：rebase-merge" 一节。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReservesDeltaLedger {
    /// (块号, 每资产 Δ)，按块升序；同块多笔 swap 累加进同一条目
    entries: VecDeque<(u64, Vec<i128>)>,
    /// 缓冲容量（块数）
    capacity: usize,
    /// 最近一次成功锚定的快照块；0 = 尚未锚定（重启/冷启动）
    base_block: u64,
    /// 锚定时的快照 reserves（做差用；与资产数组对齐）
    base_reserves: Vec<i128>,
    /// 已记录 swap 事件的最高块号（单调）
    last_event_block: u64,
    /// 被容量挤出的事件最大块号；锚定后重置。`base_block < evicted_up_to`
    /// 表示「基底之后的事件已部分丢失」，无法精确合并 → 退化 guard。
    evicted_up_to: u64,
    /// 收到过无块号日志（`log.block_number == None`），其 Δ 未入账 →
    /// 禁止 rebase 合并（退化 guard），避免合并结果偏差。
    incomplete: bool,
}

impl Default for ReservesDeltaLedger {
    fn default() -> Self {
        Self::new(BINARYFI_LEDGER_CAPACITY)
    }
}

impl ReservesDeltaLedger {
    pub const fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity,
            base_block: 0,
            base_reserves: Vec::new(),
            last_event_block: 0,
            evicted_up_to: 0,
            incomplete: false,
        }
    }

    /// 记录一笔 swap 的净变化。块号单调推进：乱序/重复事件忽略（防双计），
    /// 同块多笔累加进同一条目。
    pub fn record(&mut self, block: u64, i: usize, delta_i: i128, j: usize, delta_j: i128) {
        if block < self.last_event_block {
            return;
        }
        if block > self.last_event_block {
            self.last_event_block = block;
            self.entries.push_back((block, Vec::new()));
            while self.entries.len() > self.capacity {
                if let Some((popped, _)) = self.entries.pop_front() {
                    self.evicted_up_to = self.evicted_up_to.max(popped);
                }
            }
        }
        if let Some((_, deltas)) = self.entries.back_mut() {
            let need = i.max(j) + 1;
            if deltas.len() < need {
                deltas.resize(need, 0);
            }
            deltas[i] += delta_i;
            deltas[j] += delta_j;
        }
    }

    /// 是否已成功锚定过快照（锚定后 `reserves == 0` 语义为"已空"，而非未知）。
    pub fn is_anchored(&self) -> bool {
        self.base_block != 0
    }

    /// 标记存在未入账事件（无块号日志，`log.block_number == None`）；
    /// 之后快照合并退化为 guard，避免合并结果偏差。
    pub fn mark_incomplete(&mut self) {
        self.incomplete = true;
    }

    /// 快照合并：`current = 快照(S) + Σ(块 > S 的事件净变化)`。
    /// `current` 必须已 resize 到 `snap.len()`。分支见 [`LedgerApply`]。
    pub fn rebase(&mut self, current: &mut [U256], snap: &[U256], snap_block: u64) -> LedgerApply {
        let n = snap.len();
        if n == 0 || current.len() != n {
            return LedgerApply::SkippedStale;
        }
        // S 已覆盖全部已记录事件：直接锚定，无需 replay/prefix
        if snap_block >= self.last_event_block {
            for k in 0..n {
                current[k] = snap[k];
            }
            self.entries.clear();
            self.base_block = snap_block;
            self.base_reserves = snap.iter().map(|v| v.to::<u128>() as i128).collect();
            self.evicted_up_to = 0;
            self.incomplete = false;
            return LedgerApply::Anchored;
        }
        // 陈旧快照：早于当前基底，丢弃（事件账本为准）
        if self.base_block != 0 && snap_block < self.base_block {
            return LedgerApply::SkippedStale;
        }
        // 首次锚定（重启/冷启动）：快照覆盖历史，窗口内事件补上 S 之后的部分
        if self.base_block == 0 {
            while self
                .entries
                .front()
                .map(|(blk, _)| *blk <= snap_block)
                .unwrap_or(false)
            {
                self.entries.pop_front();
            }
            let mut replay = vec![0i128; n];
            for (_, deltas) in self.entries.iter() {
                for (k, d) in deltas.iter().enumerate().take(n) {
                    replay[k] += d;
                }
            }
            for k in 0..n {
                let v = snap[k].to::<u128>() as i128 + replay[k];
                current[k] = U256::from(v.max(0) as u128);
            }
            self.base_block = snap_block;
            self.base_reserves = snap.iter().map(|v| v.to::<u128>() as i128).collect();
            return LedgerApply::Merged;
        }
        // base ≤ S < last_event_block：rebase。prefix = Σ (base, S] 的事件变化，
        // 需基底之后的事件完整在窗口内；否则退化为 guard（保持事件账本）。
        if self.base_block < self.evicted_up_to || self.incomplete {
            return LedgerApply::SkippedWindowMiss;
        }
        let mut prefix = vec![0i128; n];
        let mut drop = 0usize;
        for (idx, (blk, deltas)) in self.entries.iter().enumerate() {
            if *blk > snap_block {
                break;
            }
            for (k, d) in deltas.iter().enumerate().take(n) {
                prefix[k] += d;
            }
            drop = idx + 1;
        }
        for _ in 0..drop {
            self.entries.pop_front();
        }
        for k in 0..n {
            let base_v = self.base_reserves.get(k).copied().unwrap_or(0);
            let snap_v = snap[k].to::<u128>() as i128;
            let cur_v = current[k].to::<u128>() as i128;
            let v = cur_v + (snap_v - base_v) - prefix[k];
            current[k] = U256::from(v.max(0) as u128);
        }
        self.base_block = snap_block;
        self.base_reserves = snap.iter().map(|v| v.to::<u128>() as i128).collect();
        LedgerApply::Merged
    }
}

/// [`ReservesDeltaLedger::rebase`] 的合并结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerApply {
    /// S ≥ last_event_block：直接以快照锚定（清空账本）
    Anchored,
    /// 精确合并：`current = 快照(S) + Σ(块 > S 事件)`，基底推进到 S
    Merged,
    /// 陈旧快照（S < base_block）或数据不足：保持事件账本，不写回
    SkippedStale,
    /// base 后事件已丢失/无块号日志，无法精确合并：保持事件账本（退化 guard）
    SkippedWindowMiss,
}

impl BinaryFiPropPool {
    /// 部署分组 key（同 pool/engine/router/vault = 同一部署，共享链上状态）。
    /// 用于 init_batch 去重与周期同步按部署刷新。
    pub fn deployment_key(&self) -> (Address, Address, Address, Address) {
        (
            self.pool_address,
            self.engine_address,
            self.router_address,
            self.vault_address,
        )
    }

    /// pair 的线性存储 index
    pub fn pair_index(&self, i: usize, j: usize) -> usize {
        let n = self.assets.len().max(1);
        i * n + j
    }

    /// 从线性 index 解出 (i, j)
    pub fn pair_indices(&self, pair: usize) -> (usize, usize) {
        let n = self.assets.len().max(1);
        (pair / n, pair % n)
    }

    fn token_index(&self, token: Address) -> Option<usize> {
        self.assets.iter().position(|t| t.address == token)
    }

    /// 批量初始化（供 Variant::init_batch 调用）
    pub async fn init_batch<N, P>(
        amms: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        factory::init_batch::<N, P>(amms, block_number, provider).await
    }

    /// 每资产点差（ask-bid）；USDT0 为 0，未知资产用默认值
    fn spread(&self, i: usize) -> u64 {
        if i == 0 {
            return 0;
        }
        // 买入被禁用的资产：bid == price（点差恒为 0，不能被默认值覆盖）
        if self.buy_disabled.get(i).copied().unwrap_or(false) {
            return 0;
        }
        self.spreads
            .get(i)
            .copied()
            .filter(|&s| s != 0)
            .unwrap_or(BINARYFI_DEFAULT_SPREAD)
    }

    /// bid = price - bid_offset。
    /// 无费口径：`bid_offset = sell_off`（fee 在报价时按 `fee_ppm` 输入侧扣，
    /// fee=1000 时等价历史 `bid = floor((price×999 - sell_off×1000)/1000)`；
    /// 见 apply_l2_update）；
    /// 快照路径直接存 `bid_offset=0, price=bid`（0 = 显式无偏移）。
    pub fn bid_price(&self, i: usize) -> Option<U256> {
        // SELL 容量权威为 0（快照观测 j→0 quote=0）时，卖出价视为不可用，
        // 防止残留价格继续驱动该方向费率/报价（engine_quote 由 maxIn=0 兜底）
        if self.max_inputs.get(i).copied().flatten() == Some(U256::ZERO) {
            return None;
        }
        let p = self.prices.get(i).copied().filter(|p| !p.is_zero())?;
        let off = match self.bid_offsets.get(i) {
            // 已显式存储（update/快照）：0 = 无偏移（bid == price）
            Some(&o) => o,
            // 未初始化：默认点差兜底
            None => self.spread(i) / 2,
        };
        Some(p.saturating_sub(U256::from(off)))
    }

    /// ask = price + ask_offset（引擎买方向偏移；直接 `0→j` 报价走 `q0j`，
    /// 跨资产第二段与诊断用此值，见 engine_quote）
    pub fn ask_price(&self, i: usize) -> Option<U256> {
        let p = self.prices.get(i).copied().filter(|p| !p.is_zero())?;
        let off = match self.ask_offsets.get(i) {
            Some(&o) => o,
            None => (self.spread(i) + 1) / 2,
        };
        Some(p + U256::from(off))
    }

    /// 从 BUY 小额报价 q0j（0→j, in=10^d0，**含费**）反推引擎无费 ask：
    /// `q0j = floor(rem × 10^(dj+2) / (ask × 10^d0))`，`rem = fee_rem(10^d0)`
    /// → `ask = floor(rem × 10^(dj+2) / (q0j × 10^d0))`。
    /// 与 unfee_quote 路径等价但无 ceil 二次舍入（fee=1000 时逐位等于历史
    /// `floor(10^(dj+2)×999/(1000×q0j))`，实测误差 ≤1 被大额 probe 修正）。
    /// q0j 足够大时解唯一（高小数位资产精确；低小数位资产可能差 1，用大额 probe 兜底）。
    fn recover_ask_eff(q0j: U256, dj: u32, d0: u32, fee_ppm: u64) -> Option<U256> {
        if q0j.is_zero() || dj > 30 || d0 > 30 {
            return None;
        }
        let rem = fee_rem(U256::from(10u64).pow(U256::from(d0)), fee_ppm);
        let num = rem.checked_mul(U256::from(10u64).pow(U256::from(dj + 2)))?;
        let den = q0j.checked_mul(U256::from(10u64).pow(U256::from(d0)))?;
        if den.is_zero() {
            return None;
        }
        Some(num / den)
    }

    /// 从 BUY 大额线性 quote（0→j, in=10^(d0+4)，未封顶，**含费**）反推精确 ask：
    /// `big = floor(rem × 10^(dj+2) / (ask × 10^d0))`，`rem = fee_rem(big_in)`
    /// → `ask = floor(rem × 10^(dj+2) / (big × 10^d0))`。
    /// 低小数位资产 q0j 反推误差可达数千（q0j 只有 3-4 位有效数字），
    /// 大额 probe 消除误差；反推值 ±1 内验证能复刻 big 才采用。
    fn recover_ask_big(
        big_out: U256,
        big_in: U256,
        dj: u32,
        d0: u32,
        fee_ppm: u64,
    ) -> Option<U256> {
        if big_out.is_zero() || dj > 30 || d0 > 30 {
            return None;
        }
        let rem = fee_rem(big_in, fee_ppm);
        let num = rem.checked_mul(U256::from(10u64).pow(U256::from(dj + 2)))?;
        let den = big_out.checked_mul(U256::from(10u64).pow(U256::from(d0)))?;
        if den.is_zero() {
            return None;
        }
        let ask = num / den;
        // 验证（±1 内）能复刻大额 probe，否则回退 q0j 反推
        for cand in [ask.saturating_sub(U256::from(1)), ask, ask + U256::from(1)] {
            if cand.is_zero() {
                continue;
            }
            let out = fee_rem(big_in, fee_ppm)
                .checked_mul(U256::from(10u64).pow(U256::from(dj + 2)))?
                / (cand.checked_mul(U256::from(10u64).pow(U256::from(d0)))?);
            if out == big_out {
                return Some(cand);
            }
        }
        Some(ask)
    }

    /// 引擎 price 更新：重算所有涉及该资产 pair 的费率（幂等 SET）
    pub fn apply_price_update(&mut self, asset_idx: usize, price: U256, block_number: u64) {
        let n = self.assets.len();
        if n == 0 || asset_idx >= n || price.is_zero() {
            return;
        }
        // 引擎内部价格 = calldata price × scale/10000（asset[2] scale=100000，其余 10000）
        let scale = self.price_scales.get(asset_idx).copied().unwrap_or(10_000);
        let scaled = price
            .checked_mul(U256::from(scale))
            .map(|p| p / U256::from(10_000))
            .unwrap_or(price);
        if scaled.is_zero() {
            return;
        }
        self.prices[asset_idx] = scaled;
        self.price_updated_block[asset_idx] = block_number;
        for j in 0..n {
            if j == asset_idx {
                continue;
            }
            let idx_ab = self.pair_index(asset_idx, j);
            let idx_ba = self.pair_index(j, asset_idx);
            if let Some(rate) = self.derive_rate(asset_idx, j) {
                self.rates[idx_ab] = rate;
            }
            if let Some(rate) = self.derive_rate(j, asset_idx) {
                self.rates[idx_ba] = rate;
            }
        }
        self.stale_pairs
            .retain(|&p| p / n != asset_idx && p % n != asset_idx);
    }

    /// 引擎 update 日志（L2 原始交易增强）完整应用：price + 阶梯点差一次到位。
    ///
    /// calldata 的 `data0`（sellLadder）/`data1`（buyLadder）为左对齐 256 位字段，
    /// 前 16 位为点差偏移字段（ladder 空间单位）；实际点差偏移
    /// `= (字段/16) × (scale/10000)`（asset[2] scale=100000，其余 10000）。
    /// 引擎按账户费率（`getFee(recipient)`，ppm）在报价时输入侧扣：
    /// - 卖方向：`bid = price - sell_off`（无费原始价；fee 在报价时
    ///   `rem = in − in×fee/1e6` 先扣，`fee=1000` 时等价历史
    ///   `bid = floor((price×999 - sell_off×1000)/1000)`）
    /// - 买方向：`ask = price + ask_off`，BUY 小额报价状态
    ///   `q0j = floor(10^(dj+2) / ask)`（无费；fee 由 engine_quote 输入侧扣）
    /// `spread = ask_offset + bid_offset` 仅作诊断参考（不对称时 price 非算术中点）。
    pub fn apply_l2_update(
        &mut self,
        asset_idx: usize,
        price: U256,
        block_number: u64,
        ask_offset_raw: u64,
        bid_offset_raw: u64,
    ) {
        let scale = self.price_scales.get(asset_idx).copied().unwrap_or(10_000);
        let factor = (scale / 10_000).max(1);
        let ask_off = ask_offset_raw.saturating_mul(factor);
        let sell_off = bid_offset_raw.saturating_mul(factor);
        // 引擎内部价格 = calldata price × scale/10000（与 apply_price_update 一致）
        let scaled = price
            .checked_mul(U256::from(scale))
            .map(|p| p / U256::from(10_000))
            .unwrap_or(price);
        // 无费语义：bid = price − sell_off（fee 在报价时按 fee_ppm 输入侧扣，
        // 不再打包进 bid_offset；历史 `ceil(price/1000)+sell_off` 等价于
        // fee=1000 时先扣费再报 bid，见模块顶部公式说明）
        let bid_off = U256::from(sell_off);
        let bid_off_u64 = bid_off.to::<u64>();
        if let Some(o) = self.bid_offsets.get_mut(asset_idx) {
            *o = bid_off_u64;
        }
        if let Some(o) = self.ask_offsets.get_mut(asset_idx) {
            *o = ask_off;
        }
        if let Some(s) = self.spreads.get_mut(asset_idx) {
            *s = ask_off.saturating_add(bid_off_u64);
        }
        // BUY 小额报价状态（无费）：q0j = floor(10^(dj+2) / (price+ask_off))
        if asset_idx != 0 {
            let dj = self.assets.get(asset_idx).map(|t| t.decimals as u32);
            let ask_eff = scaled + U256::from(ask_off);
            let q = dj
                .filter(|&d| d <= 30)
                .filter(|_| !ask_eff.is_zero())
                .map(|d| {
                    let num = U256::from(10u64).pow(U256::from(d + 2));
                    num / ask_eff
                });
            if let Some(qv) = self.q0j.get_mut(asset_idx) {
                *qv = q;
            }
        }
        // SELL 精确有理数分子（无费）：raw = scaled − sell_off
        // （fee 在报价时按 fee_ppm 输入侧扣，见 sell_out/ladder_sell_out）
        let sell_raw = scaled.checked_sub(U256::from(sell_off));
        if let Some(r) = self.sell_raw.get_mut(asset_idx) {
            *r = sell_raw;
        }
        // update 只携带价格与 ladder 点差，不携带容量；SELL 0-cap（max_inputs=Some(0)）
        // 来自快照权威观测，不能被价格更新复活；容量恢复由下一次快照重新观测。
        self.apply_price_update(asset_idx, price, block_number);
    }

    /// 引擎 update 日志（L2 原始交易增强）完整应用：价格 + 点差 + 多档阶梯一次到位。
    /// 与 `apply_l2_update` 相同，另从 calldata `data0`（sellLadder）/`data1`
    /// （buyLadder）解码多档阶梯（weight 按 scale/10000 折算内部价格单位）。
    pub fn apply_l2_update_full(
        &mut self,
        asset_idx: usize,
        price: U256,
        block_number: u64,
        ask_offset_raw: u64,
        bid_offset_raw: u64,
        data0: U256,
        data1: U256,
    ) {
        self.apply_l2_update(
            asset_idx,
            price,
            block_number,
            ask_offset_raw,
            bid_offset_raw,
        );
        let factor = {
            let scale = self.price_scales.get(asset_idx).copied().unwrap_or(10_000);
            (scale / 10_000).max(1) as u32
        };
        let decode = |data: U256| -> Option<Vec<(u32, u32)>> {
            decode_ladder(data).map(|l| {
                l.into_iter()
                    .map(|(w, q)| (w.saturating_mul(factor), q))
                    .collect()
            })
        };
        if let Some(l) = self.sell_ladders.get_mut(asset_idx) {
            *l = decode(data0);
        }
        if let Some(l) = self.buy_ladders.get_mut(asset_idx) {
            *l = decode(data1);
        }
        // SELL 梯子为空（MM 在链上清空 SELL 侧，data0=0）：方向死亡，容量权威归零。
        // 否则 sell_out 线性回退会沿用快照 max_inputs，快照若来自 alive 期则残留
        // 非零容量——链上 j→0 quote=0 而本地仍报价（幻影利润）。表示与
        // apply_snapshot 的死方向一致（max_inputs[j]=Some(0)），快照/update 恢复时重观测。
        if asset_idx != 0
            && self
                .sell_ladders
                .get(asset_idx)
                .cloned()
                .flatten()
                .map_or(true, |l| l.is_empty())
        {
            if let Some(v) = self.max_inputs.get_mut(asset_idx) {
                *v = Some(U256::ZERO);
            }
        }
        // BUY 阶梯容量重置：Σqty×R（R 已知时；链上实测 asset2 平顶 = Σqty×R）。
        // 之后由 Swap 事件逐笔消费（见 anchor_rate），直到下一次 update 重置。
        // 梯子为空（data1=0，MM 清空 BUY 侧）：方向死亡，容量权威归零——残留
        // alive 期非零容量会让链上 0→j quote=0 时本地仍报价（本次事故根因）。
        if asset_idx != 0 {
            let ladder = self.buy_ladders.get(asset_idx).cloned().flatten();
            if ladder.as_ref().map_or(true, |l| l.is_empty()) {
                if let Some(v) = self.buy_ladder_remaining.get_mut(asset_idx) {
                    *v = Some(U256::ZERO);
                }
            } else if let (Some(ladder), Some(r)) = (
                ladder,
                self.ladder_reserves.get(asset_idx).copied().flatten(),
            ) {
                if !r.is_zero() {
                    let total: Option<U256> = ladder.iter().try_fold(U256::ZERO, |acc, &(_, q)| {
                        acc.checked_add(U256::from(q).checked_mul(r)?)
                    });
                    if let Some(v) = self.buy_ladder_remaining.get_mut(asset_idx) {
                        *v = total.filter(|t| !t.is_zero());
                    }
                }
            }
        }
    }

    /// SELL（i→USDT0）输出：ladder + 引擎储备 R 已知时多档阶梯逐位复刻；
    /// 否则回退单档线性（raw 有理数 + maxIn 截断 + 超容量归零）。
    fn sell_out(&self, i: usize, amount_in: U256) -> Option<U256> {
        let d0 = self.assets[0].decimals as u32;
        let di = self.assets[i].decimals as u32;
        let fee = self.fee_ppm;
        if let (Some(ladder), Some(r)) = (
            self.sell_ladders.get(i).cloned().flatten(),
            self.ladder_reserves.get(i).copied().flatten(),
        ) {
            if !ladder.is_empty() && !r.is_zero() {
                if let Some(p) = self.prices.get(i).copied().filter(|p| !p.is_zero()) {
                    if let Some(out) = ladder_sell_out(p, &ladder, r, amount_in, d0, di, fee) {
                        return self.sell_zero_over_vault(out);
                    }
                }
            }
        }
        // 单档线性回退（raw 有理数）：先按 fee 扣输入再按 maxIn 截断消耗量，
        // 与引擎逐档 `min(rem, qty×R)` 一致——饱和时消耗满 maxIn（fee 无关，
        // 链上实测 asset2 fee=1000 饱和输出 = maxIn×raw）。不能先 `min(in, maxIn)`
        // 再扣费：会把饱和输出低估 ~0.1%（= fee 比例），旧版公式即此偏差。
        let p10 = |e: u32| U256::from(10u64).pow(U256::from(e));
        let k0 = d0.saturating_sub(2);
        let raw = self
            .sell_raw
            .get(i)
            .copied()
            .flatten()
            .or_else(|| self.bid_price(i))?;
        let rem = fee_rem(amount_in, fee);
        let consume = match self.max_inputs.get(i).copied().flatten() {
            Some(m) => rem.min(m),
            None => rem,
        };
        let out = consume.checked_mul(raw)?.checked_mul(p10(k0))? / p10(di);
        self.sell_zero_over_vault(out)
    }

    /// SELL 超容量归零：i→0 输出 > USDT0 金库余额 → 0（链上实测；
    /// 对称 buy_zero_over_vault）。锚定后余额 0 = 已空（权威归零，
    /// 抽干后不再报价）；未锚定 0 = 未知，不归零。
    fn sell_zero_over_vault(&self, v: U256) -> Option<U256> {
        match self.vault_known(0) {
            Some(r) if v > r => Some(U256::ZERO),
            _ => Some(v),
        }
    }

    /// 金库余额读取：锚定后 `0` 为"已空"（权威，参与归零/截断），
    /// 未锚定 `0` 为"未知"（None，维持历史兜底语义）。
    fn vault_known(&self, j: usize) -> Option<U256> {
        let r = self.reserves.get(j).copied().unwrap_or(U256::ZERO);
        if r.is_zero() {
            if self.ledger.is_anchored() {
                Some(U256::ZERO)
            } else {
                None
            }
        } else {
            Some(r)
        }
    }

    /// SELL 全档封顶输出（ladder + R 已知时逐档累加；未知 → None）。
    /// 用于 `max_achievable_out` 的输出可达性校验。
    fn sell_cap(&self, i: usize) -> Option<U256> {
        let (Some(ladder), Some(r)) = (
            self.sell_ladders.get(i).cloned().flatten(),
            self.ladder_reserves.get(i).copied().flatten(),
        ) else {
            return None;
        };
        if ladder.is_empty() || r.is_zero() {
            return None;
        }
        let p = self.prices.get(i).copied().filter(|p| !p.is_zero())?;
        let d0 = self.assets[0].decimals as u32;
        let di = self.assets[i].decimals as u32;
        ladder_sell_out(p, &ladder, r, U256::MAX, d0, di, self.fee_ppm)
            .and_then(|v| self.sell_zero_over_vault(v))
    }

    /// 用候选储备 R 重算 100 整枚 SELL probe 输出并与链上 q_big 对拍；
    /// probe 饱和（总容量 ≤ probe rem）且逐位一致时返回 Some(R)。
    fn verify_sell_probe(&self, j: usize, r: U256, q_big: U256) -> Option<U256> {
        let ladder = self.sell_ladders.get(j).cloned().flatten()?;
        if ladder.is_empty() || r.is_zero() {
            return None;
        }
        let p = self.prices.get(j).copied().filter(|p| !p.is_zero())?;
        let d0 = self.assets[0].decimals as u32;
        let di = self.assets[j].decimals as u32;
        if di > 30 || d0 > 30 {
            return None;
        }
        let p10 = |e: u32| U256::from(10u64).pow(U256::from(e));
        let probe_in = U256::from(100u64).checked_mul(p10(di))?;
        let rem = fee_rem(probe_in, self.fee_ppm);
        let total: U256 = ladder.iter().try_fold(U256::ZERO, |acc, &(_, q)| {
            acc.checked_add(U256::from(q).checked_mul(r)?)
        })?;
        if total > rem {
            return None; // 未饱和：probe 输出非封顶，无法反推 R
        }
        if ladder_sell_out(p, &ladder, r, probe_in, d0, di, self.fee_ppm) == Some(q_big) {
            Some(r)
        } else {
            None
        }
    }

    /// 由 raw/q0j 推导 rate(i→j)，与引擎报价公式一致（精确有理数，约分后大整数分数）：
    ///   - `i→0`：`rate = raw_i × 10^(d0-2) / 10^di`
    ///     （raw = price - sell_off，无费；再按 fee_ppm 折算可执行价）
    ///   - `0→j`：`rate = q0j_j / 10^d0`（BUY 小额报价状态，无费）
    ///   - `i→j`：两段式 `rate = raw_i × q0j_j / 10^(di+2)`（无费）
    /// 返回值均为**含当前 fee 的可执行中间价**（无费 rate × (1e6−fee)/1e6），
    /// 供 spot/预过滤与下游可执行价对齐。
    fn derive_rate(&self, i: usize, j: usize) -> Option<Rate> {
        if i == j || i >= self.assets.len() || j >= self.assets.len() {
            return None;
        }
        // 引擎侧 j 买入被禁用：所有进入 j 的费率置零
        if self.buy_disabled.get(j).copied().unwrap_or(false) {
            return Some(Rate::zero());
        }
        let di = self.assets[i].decimals as u32;
        let dj = self.assets[j].decimals as u32;
        let d0 = self.assets[0].decimals as u32;
        if di > 30 || dj > 30 || d0 > 30 {
            return None;
        }
        let p10 = |e: u32| U256::from(10u64).pow(U256::from(e));
        let k0 = d0.saturating_sub(2);
        // SELL 精确有理数分子（无费；None → bid_price 兜底，等价整数 bid 线性）
        let raw = |k: usize| -> Option<U256> {
            self.sell_raw
                .get(k)
                .copied()
                .flatten()
                .or_else(|| self.bid_price(k))
        };
        if j == 0 {
            let raw_i = raw(i)?;
            let rate = if di >= k0 {
                Rate {
                    num: raw_i,
                    den: p10(di - k0),
                }
            } else {
                Rate {
                    num: raw_i.checked_mul(p10(k0 - di))?,
                    den: U256::from(1),
                }
            };
            // 含当前 fee 的可执行中间价（输出侧 ×(1e6−fee)/1e6）
            apply_fee_rate(rate, self.fee_ppm)
        } else if i == 0 {
            // rate = q0j / 10^d0（0→j 小额报价精确值，无费；再按 fee 折算可执行价）
            let q = self
                .q0j
                .get(j)
                .copied()
                .flatten()
                .filter(|q| !q.is_zero())?;
            apply_fee_rate(
                Rate {
                    num: q,
                    den: p10(d0),
                },
                self.fee_ppm,
            )
        } else {
            // 两段式相乘（无费）：raw_i×10^(d0-2)/10^di × q0j/10^d0
            // = raw_i×q0j / 10^(di+2)
            // （跨资产第二段不含费因子，链上实测；第一段扣费一次，
            //  可执行价整体 ×(1e6−fee)/1e6）
            let raw_i = raw(i)?;
            let q = self
                .q0j
                .get(j)
                .copied()
                .flatten()
                .filter(|q| !q.is_zero())?;
            apply_fee_rate(
                Rate {
                    num: raw_i.checked_mul(q)?,
                    den: p10(di + 2),
                },
                self.fee_ppm,
            )
        }
    }

    /// 仅设置费率（批量快照/update 用，不改变余额）
    pub fn set_rate(&mut self, i: usize, j: usize, amount_in: U256, amount_out: U256) {
        let n = self.assets.len();
        if i >= n || j >= n || i == j || amount_in.is_zero() {
            return;
        }
        let idx = self.pair_index(i, j);
        if !amount_out.is_zero() {
            self.rates[idx] = Rate {
                num: amount_out,
                den: amount_in,
            };
        }
    }

    /// Swap 事件锚定（完整路径，测试/旧调用点保留）：共享金库账本 + price0 标定。
    ///
    /// 费率以引擎价格精确推导为准；仅当方向两侧价格未知时才用 Swap 的
    /// `out/in` 锚定（保证池子在未收到 update 时可用，不作为精度来源）。
    pub fn anchor_rate(&mut self, i: usize, j: usize, amount_in: U256, amount_out: U256) {
        self.update_shared_vault(i, j, amount_in, amount_out);
        self.anchor_prices_only(i, j, amount_in, amount_out);
    }

    /// 共享金库账本更新（部署级，任何 Swap 都动共享 vault）：
    /// `reserves[tokenIn] += in`、`reserves[tokenOut] -= out`、
    /// `buy_ladder_remaining[tokenOut] -= out`（BUY 阶梯容量按输出资产消耗）。
    /// 不涉及价格——跨 pair swap 对共享金库的增减必须全局可见
    /// （2026-08-19 事故：其他 pair 的 SELL 抽干 USDT0 金库，本实例不可见
    /// → 本地金库门控放行必然失败的交易）。
    fn update_shared_vault(&mut self, i: usize, j: usize, amount_in: U256, amount_out: U256) {
        if let Some(r) = self.reserves.get_mut(i) {
            *r = r.saturating_add(amount_in);
        }
        if let Some(r) = self.reserves.get_mut(j) {
            *r = r.saturating_sub(amount_out);
        }
        // BUY 阶梯容量消费：0→j（及跨资产第二段）消耗 j 的 BUY 容量，
        // 与链上 swap 消耗 ladder 一致；直到下一次 update/快照重置。
        if j != 0 {
            if let Some(rem) = self.buy_ladder_remaining.get_mut(j) {
                if let Some(r) = rem.as_mut() {
                    *r = r.saturating_sub(amount_out);
                }
            }
        }
    }

    /// 价格锚定（仅限本 pair 命中时调用，防跨 pair 污染价格）：
    /// 价格未知时用 `out/in` 兜底 + price0 标定。
    fn anchor_prices_only(&mut self, i: usize, j: usize, amount_in: U256, amount_out: U256) {
        let price_known = self.prices.get(i).map(|p| !p.is_zero()).unwrap_or(false)
            && self.prices.get(j).map(|p| !p.is_zero()).unwrap_or(false);
        if !price_known {
            self.set_rate(i, j, amount_in, amount_out);
        }
        if !self.price0_calibrated {
            if let Some(p0) = self.implied_price0(i, j, amount_in, amount_out) {
                if !p0.is_zero() {
                    self.prices[0] = p0;
                    self.price0_calibrated = true;
                }
            }
        }
    }

    /// 由 0 参与的 Swap 锚定反推 price0（校准引擎定点精度）
    fn implied_price0(
        &self,
        i: usize,
        j: usize,
        amount_in: U256,
        amount_out: U256,
    ) -> Option<U256> {
        let n = self.assets.len();
        if n == 0 {
            return None;
        }
        let d0 = self.assets[0].decimals as u32;
        let p10_0 = U256::from(10u64).pow(U256::from(d0.min(30)));
        if i == 0 {
            let pj = self.prices.get(j).copied()?;
            if pj.is_zero() {
                return None;
            }
            let dj = self.assets[j].decimals as u32;
            let p10_j = U256::from(10u64).pow(U256::from(dj.min(30)));
            // rate(0→j) = out/in = (price0/pj) * 10^(dj-d0)
            // price0 = out * pj * 10^d0 / (in * 10^dj)
            let num = amount_out.checked_mul(pj)?.checked_mul(p10_0)?;
            let den = amount_in.checked_mul(p10_j)?;
            if den.is_zero() {
                return None;
            }
            Some(num / den)
        } else if j == 0 {
            let pi = self.prices.get(i).copied()?;
            if pi.is_zero() {
                return None;
            }
            let di = self.assets[i].decimals as u32;
            let p10_i = U256::from(10u64).pow(U256::from(di.min(30)));
            // rate(i→0) = out/in = (pi/price0) * 10^(d0-di)
            // price0 = pi * in * 10^di / (out * 10^d0)
            let num = pi.checked_mul(amount_in)?.checked_mul(p10_i)?;
            let den = amount_out.checked_mul(p10_0)?;
            if den.is_zero() {
                return None;
            }
            Some(num / den)
        } else {
            None
        }
    }

    /// 标记某资产相关的全部 pair 为 stale（等待 update() 批量刷新）
    pub fn mark_stale_for_asset(&mut self, asset_idx: usize) {
        let n = self.assets.len();
        if asset_idx >= n {
            return;
        }
        for j in 0..n {
            if j == asset_idx {
                continue;
            }
            for p in [self.pair_index(asset_idx, j), self.pair_index(j, asset_idx)] {
                if !self.stale_pairs.contains(&p) {
                    self.stale_pairs.push(p);
                }
            }
        }
    }

    /// 清除已刷新 pair 的 stale 标记
    pub fn clear_stale_pairs(&mut self, pairs: &[usize]) {
        for p in pairs {
            self.stale_pairs.retain(|&x| x != *p);
        }
    }

    /// 与链上引擎 quote 完全一致的计算（含两段式截断与阶梯上限）：
    ///   - SELL（j==0）：`out = min(rem, maxIn_i) · bid_i · 10^(d0-2) / 10^di`，
    ///     `rem = in − in×fee_ppm/1e6`（先扣费再截断；饱和时消耗满 maxIn，
    ///     输出 fee 无关，链上实测 asset2）
    ///   - BUY（i==0）：先扣费 `rem = in − in×fee_ppm/1e6`，再
    ///     `linear = floor(rem × 10^(dj+2) / (ask × 10^d0))`（链上逐位对拍，
    ///     tx1/tx2 失败交易零偏差），再按 `buy_capped` 截断（饱和型
    ///     `min(linear, maxOut)` / 归零型 `linear > 金库余额 → 0` / 未知不截断）
    ///   - 跨资产：先按 maxIn_i 截断输入走 i→0，第二段
    ///     `floor(v × 10^(dj-d0+2) / ask_j)`（无额外费率因子——费已在两段输入
    ///     侧各扣一次，链上实测），
    ///     再按 BUY 规则截断/归零
    /// 价格未知或资产缺失时返回 `None`。
    /// 引擎报价核心：`i→j` 全方向（含 SELL 截断、BUY 阶梯上限/归零、跨资产两段式）
    pub fn engine_quote(&self, i: usize, j: usize, amount_in: U256) -> Option<U256> {
        if i == j || amount_in.is_zero() {
            return Some(U256::ZERO);
        }
        if i >= self.assets.len() || j >= self.assets.len() {
            return None;
        }
        // binaryFI 链上时效：引擎 update 后 quote 仅在 5 块窗口内有效，过期后
        // pool.quote() 返回 0（链上实测，非 revert）。用模块自身数据
        // price_updated_block（update calldata 注入块号）与 last_synced_block
        // 判定；price_updated_block == 0（快照/锚定路径）不判过期。
        if !self.asset_price_fresh(j) || (i != 0 && !self.asset_price_fresh(i)) {
            return Some(U256::ZERO);
        }
        // 引擎侧该资产买入被禁用（0→j quote 恒为 0）
        if self.buy_disabled.get(j).copied().unwrap_or(false) {
            return Some(U256::ZERO);
        }
        let di = self.assets[i].decimals as u32;
        let dj = self.assets[j].decimals as u32;
        let d0 = self.assets[0].decimals as u32;
        if di > 30 || dj > 30 || d0 > 30 {
            return None;
        }
        let p10 = |e: u32| U256::from(10u64).pow(U256::from(e));
        if j == 0 {
            // SELL：ladder + R 已知时多档阶梯逐位复刻；否则单档线性回退
            self.sell_out(i, amount_in)
        } else if i == 0 {
            // BUY：先扣费（rem = in − in×fee_ppm/1e6，整数除法，与 SELL 同款），
            // 再对 rem 线性报价：out = floor(rem × 10^(dj+2) / (ask × 10^d0))。
            // 链上实测逐位一致（tx1 in=44,291,018 → 324,080,619,644,034,278；
            // tx2 in=122,156,425 → 891,476,871,940,974,505，fee=1000）；
            // fee=200 实测 asset4~9 全 -200.0ppm 逐位一致。不能用
            // in×(1e6−fee)/1e6 替代——非整倍数输入有 ~1e-4% 偏差。
            let ask = self.ask_price(j)?;
            if ask.is_zero() {
                return Some(U256::ZERO);
            }
            let rem = fee_rem(amount_in, self.fee_ppm);
            let linear = rem.checked_mul(p10(dj + 2))? / (ask.checked_mul(p10(d0))?);
            Some(self.buy_capped(j, linear))
        } else {
            let ask = self.ask_price(j)?;
            if ask.is_zero() {
                return Some(U256::ZERO);
            }
            // 两段式：第一段 i→USDT0（sell_out 含多档阶梯 + 超容量归零），
            // 第二段 0→j；第二段 **无额外费率因子**（费已由 sell_out 输入侧
            // 扣一次，链上实测，见模块文档）
            let value = self.sell_out(i, amount_in)?;
            let linear = if dj + 2 >= d0 {
                value.checked_mul(p10(dj + 2 - d0))? / ask
            } else {
                value / (ask.checked_mul(p10(d0 - dj - 2))?)
            };
            Some(self.buy_capped(j, linear))
        }
    }

    /// 资产价格是否在链上时效窗口内（最后 update 后 ≤ BINARYFI_QUOTE_TTL_BLOCKS 块）。
    /// `price_updated_block == 0`（无 update 日志佐证）不判过期；过期后链上
    /// quote 返回 0，本地同步返回 0，避免用过期价格算出幻影利润。
    fn asset_price_fresh(&self, asset_idx: usize) -> bool {
        match self
            .price_updated_block
            .get(asset_idx)
            .copied()
            .unwrap_or(0)
        {
            0 => true,
            updated => self.last_synced_block.saturating_sub(updated) <= BINARYFI_QUOTE_TTL_BLOCKS,
        }
    }

    /// BUY（0→j）输出应用引擎阶梯上限：
    ///   - 超阈值归零型（大额 probe = 0，阶梯容量 > 金库余额）：
    ///     linear ≤ 金库余额才返回，否则 0
    ///   - 饱和型（maxOut 已知）：`min(linear, maxOut)`
    ///   - 未知：不截断（保持线性）
    fn buy_capped(&self, j: usize, linear: U256) -> U256 {
        // 归零型：阶梯容量 > 金库余额，maxOut 不可观测；linear ≤ 金库才返回
        // （与 max_outputs 互斥：apply_snapshot 置归零型时 maxOut 恒为 None）
        if self.buy_zero_over_vault.get(j).copied().unwrap_or(false) {
            let vault = self.reserves.get(j).copied().unwrap_or(U256::ZERO);
            return if linear <= vault { linear } else { U256::ZERO };
        }
        // maxOut 未知时 capped = linear；归零型（buy_zero_over_vault）等价于
        // maxOut = ∞（capped = linear），已在上方分支处理。
        // 封顶优先取 buy_ladder_remaining（update 路径 Σqty×R 精确、随 Swap 消费
        // 递减），其次快照观测的 max_outputs。
        let capped = match self
            .buy_ladder_remaining
            .get(j)
            .copied()
            .flatten()
            .or_else(|| self.max_outputs.get(j).copied().flatten())
        {
            Some(m) => linear.min(m),
            None => linear,
        };
        // 实时金库零门槛：**封顶后**输出超出当前金库余额 → 链上归零。
        //  - 饱和型：min(linear, maxOut) > vault → 0（NVDAx 金库被抽干后
        //    maxOut=1.301e18 > vault≈1.07e12，链上 quote 恒 0，本地不得继续
        //    按快照 maxOut 报价制造幻影利润）
        //  - 饱和型且 maxOut ≤ vault：min(linear, maxOut) 正常返回（锚点块
        //    5 资产实测：linear > vault 但链上仍返回 maxOut）
        // 金库未知（0）时不门控，与 capped_out 的"余额未知不截断"一致。
        let vault = self.reserves.get(j).copied().unwrap_or(U256::ZERO);
        if !vault.is_zero() && capped > vault {
            U256::ZERO
        } else {
            capped
        }
    }

    /// 该方向的阶梯上限（maxIn/maxOut）是否已知；已知时 `engine_quote` 已精确
    /// 复刻链上 cap，无需 96%·金库余额兜底截断（避免低估合法输出）。
    fn ladder_cap_known(&self, i: usize, j: usize) -> bool {
        let n = self.assets.len();
        if i >= n || j >= n || i == j {
            return false;
        }
        let max_in = |k: usize| self.max_inputs.get(k).copied().flatten().is_some();
        let max_out = |k: usize| {
            self.buy_ladder_remaining
                .get(k)
                .copied()
                .flatten()
                .is_some()
                || self.max_outputs.get(k).copied().flatten().is_some()
                || self.buy_zero_over_vault.get(k).copied().unwrap_or(false)
        };
        if j == 0 {
            // SELL 超容量归零已内置 engine_quote（out > USDT0 金库余额 → 0），
            // 免 96% 兜底截断（避免低估合法线性输出）
            true
        } else if i == 0 {
            max_out(j)
        } else {
            // 两段式：两侧上限都必须已知才能免去兜底截断
            max_in(i) && max_out(j)
        }
    }

    /// 该方向在引擎 cap 约束下可达到的最大输出：
    ///   - SELL（j==0）：maxIn 已知 → 输入截断到 maxIn 的线性输出；未知 → 96% 金库兜底
    ///   - BUY（i==0）：归零型 → 金库余额；饱和型 → maxOut；未知 → 96% 金库兜底
    ///   - 跨资产：输入侧 maxIn 两段式输出 与 输出侧上限 取小
    /// 用于 `simulate_swap_exact_out` 的输出可达性校验（与 `simulate_swap` 的
    /// `engine_quote` 语义一致，避免用 96% 兜底高估合法输出）。
    fn max_achievable_out(&self, i: usize, j: usize) -> Option<U256> {
        let n = self.assets.len();
        if i >= n || j >= n || i == j {
            return None;
        }
        // 时效门控与 engine_quote 一致：过期资产不可达，输出上限 = 0
        if !self.asset_price_fresh(j) || (i != 0 && !self.asset_price_fresh(i)) {
            return Some(U256::ZERO);
        }
        let out_ceiling = if self.buy_zero_over_vault.get(j).copied().unwrap_or(false) {
            Some(self.reserves.get(j).copied().unwrap_or(U256::ZERO))
        } else {
            // 饱和型同样受实时金库约束（buy_capped 零门槛）：可达上限 =
            // min(maxOut, 当前金库)；maxOut 优先取 buy_ladder_remaining
            match self
                .buy_ladder_remaining
                .get(j)
                .copied()
                .flatten()
                .or_else(|| self.max_outputs.get(j).copied().flatten())
            {
                Some(m) => match self.vault_known(j) {
                    Some(v) => Some(m.min(v)),
                    None => Some(m),
                },
                None => None,
            }
        };
        if j == 0 {
            if let Some(cap) = self.sell_cap(i) {
                return Some(cap);
            }
            return match self.max_inputs.get(i).copied().flatten() {
                // 满档输入（U256::MAX）→ rem ≥ maxIn → consume = maxIn，
                // 输出 = 全阶梯容量（fee 无关），与 sell_cap 语义一致；
                // 用 maxIn 本身会走 fee_rem(maxIn) 低估 ~0.1%。
                Some(_) => self.engine_quote(i, 0, U256::MAX),
                None => Some(self.capped_out(0, U256::MAX)),
            };
        }
        if i == 0 {
            return Some(out_ceiling.unwrap_or_else(|| self.capped_out(j, U256::MAX)));
        }
        let from_in = match self.max_inputs.get(i).copied().flatten() {
            Some(m) => self.engine_quote(i, j, m),
            None => None,
        };
        match (from_in, out_ceiling) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => Some(self.capped_out(j, U256::MAX)),
        }
    }

    /// 应用批量快照：填充资产/余额，从同块 `(0→j)/(j→0)` quote 精确恢复每资产
    /// bid（SELL 侧）与 `q0j`（BUY 小额报价状态），再由 bid/q0j 推导全部费率
    /// （不做 quote 锚定，避免 1-ulp 放大误差）；同时从大额 probe 恢复 BUY maxOut
    /// （饱和型直接取 q_big，归零型标记 `buy_zero_over_vault`）与 SELL maxIn
    /// （100 整枚 probe 反推）。
    pub fn apply_snapshot(&mut self, snap: &Snapshot, snap_block: u64) -> usize {
        let mut derived = 0usize;
        if !snap.assets.is_empty() {
            self.assets = snap
                .assets
                .iter()
                .zip(snap.decimals.iter())
                .map(|(addr, d)| Token {
                    address: *addr,
                    decimals: *d,
                    symbol: String::new(),
                    chain_id: self.chain_id,
                    fot_tax: None,
                })
                .collect();
        }
        let n = self.assets.len();
        if n == 0 {
            return 0;
        }
        self.prices.resize(n, U256::ZERO);
        self.spreads.resize(n, 0);
        self.bid_offsets.resize(n, 0);
        self.ask_offsets.resize(n, 0);
        self.q0j.resize(n, None);
        self.q0j[0] = None;
        self.sell_raw.resize(n, None);
        self.sell_raw[0] = None;
        self.price_scales.resize(n, 10_000);
        self.buy_disabled.resize(n, false);
        self.buy_zero_over_vault.resize(n, false);
        self.max_outputs.resize(n, None);
        self.max_inputs.resize(n, None);
        self.sell_ladders.resize(n, None);
        self.buy_ladders.resize(n, None);
        self.buy_ladder_remaining.resize(n, None);
        self.ladder_reserves.resize(n, None);
        self.reserves.resize(n, U256::ZERO);
        self.rates.resize(n * n, Rate::zero());
        self.price_updated_block.resize(n, 0);
        self.prices[0] = U256::from(BINARYFI_PRICE0_DEFAULT);
        self.spreads[0] = 0;
        self.bid_offsets[0] = 0;
        self.ask_offsets[0] = 0;

        // 引擎 scale（asset[2] = 100000，其余 10000；asset[0] 无引擎配置保持默认）
        if snap.scales.len() == n {
            for (j, s) in snap.scales.iter().enumerate() {
                if j == 0 || s.is_zero() || *s > U256::from(u64::MAX) {
                    continue;
                }
                let scale = s.to::<u64>();
                if (10_000..=1_000_000).contains(&scale) {
                    self.price_scales[j] = scale;
                }
            }
        }

        // 引擎按账户费率（ppm）：批量合约 `getFee(recipient)` 带回，非 0 时更新。
        // 必须先于下方 quote 反推——快照 quote 是含费可执行输出，无费化反推
        // （unfee_quote）依赖当前 fee 口径；fee 变更只换字段、价格无需重校准
        // （L2 存无费原始价，报价时按 fee 扣）。
        if !snap.fee.is_zero() {
            let new_fee = snap.fee.min(U256::from(1_000_000u64)).to::<u64>();
            if new_fee != self.fee_ppm {
                tracing::debug!(
                    old_fee = self.fee_ppm,
                    new_fee = new_fee,
                    snap_block = snap_block,
                    "binaryfi engine fee updated"
                );
                self.fee_ppm = new_fee;
            }
        }

        // 余额以 **真实共享金库 ERC20 余额**（vault.balanceOf）为准：vault 持有
        // 全部 12 资产，任何 pair 的 swap 都会增减它；引擎内部记账
        // （getAssetReserves）会与真实金库脱节（2026-08-19 事故：引擎 quote
        // 606.7M 而真实金库仅 280.7M），本地门控必须以真实余额为准才能挡住
        // 必然失败的交易。读取失败回退引擎内部记账，再回退 pool 余额。
        //
        // 写回走 `ReservesDeltaLedger::rebase` 合并（2026-08-31 事故根因修复）：
        // 快照块 S 取自 canonical head（滞后 flashblock 1-2 块），无条件覆盖
        // 会把抽干前余额打回事件账本已扣到的精确值 → 幻影容量持续复活。
        // 合并语义 `reserves = 快照(S) + Σ(块 > S 事件净变化)`：窗口内精确，
        // 窗口外退化 guard（绝不回退事件账本）。reserves 是纯累积量，事件
        // 只增减不写真值，这是它区别于其它"最新值"字段必须合并的根本原因。
        let balances: Option<&[U256]> = if snap.vaultBalances.len() == n {
            Some(&snap.vaultBalances)
        } else if snap.vaultReserves.len() == n {
            Some(&snap.vaultReserves)
        } else if snap.poolBalances.len() == n {
            Some(&snap.poolBalances)
        } else {
            None
        };
        if let Some(balances) = balances {
            match self.ledger.rebase(&mut self.reserves, balances, snap_block) {
                LedgerApply::Anchored => tracing::debug!(
                    snap_block,
                    "binaryfi reserves anchored by snapshot (S >= last event block)"
                ),
                LedgerApply::Merged => tracing::debug!(
                    snap_block,
                    base_block = self.ledger.base_block,
                    "binaryfi reserves rebase-merged onto snapshot"
                ),
                LedgerApply::SkippedStale => tracing::warn!(
                    snap_block,
                    base_block = self.ledger.base_block,
                    "binaryfi reserves snapshot skipped: stale (kept event ledger)"
                ),
                LedgerApply::SkippedWindowMiss => tracing::warn!(
                    snap_block,
                    base_block = self.ledger.base_block,
                    last_event_block = self.ledger.last_event_block,
                    "binaryfi reserves snapshot skipped: ledger window miss (kept event ledger)"
                ),
            }
        }

        // 1) 从同块 quote 恢复每资产 bid 与 BUY 小额报价 q0j：
        //    - `j→0`（amountIn=10^(dj-4)）无舍入：out = bid 精确值
        //    - `0→j`（amountIn=10^d0）：out = q0j（BUY 报价状态，直接取链上值）
        //    - `0→j` 大额（amountIn=10^(d0+4)，pair >= n*n）：阶梯上限恢复（见 1.5）
        let mut j0_out: Vec<Option<U256>> = vec![None; n];
        let mut zj_out: Vec<Option<U256>> = vec![None; n];
        let mut big_out: Vec<Option<U256>> = vec![None; n];
        let mut mid_out: Vec<Option<U256>> = vec![None; n];
        let mut big_sell_out: Vec<Option<U256>> = vec![None; n];
        for (k, pair) in snap.quotePairs.iter().enumerate() {
            if k >= snap.quotes.len() || !snap.quotes[k].success {
                continue;
            }
            let p = pair.to::<usize>();
            let nn = n * n;
            if p >= 3 * nn {
                // (0→j) 中额报价（10^(d0+3)，1000x 小额）：检测非单调阶梯退化
                let j = p - 3 * nn;
                if j > 0 && j < n {
                    mid_out[j] = Some(snap.quotes[k].amountOut);
                }
                continue;
            }
            if p >= 2 * nn {
                // (j→0) 100 整枚报价：恢复 SELL 侧 maxIn
                let j = p - 2 * nn;
                if j > 0 && j < n {
                    big_sell_out[j] = Some(snap.quotes[k].amountOut);
                }
                continue;
            }
            if p >= nn {
                let j = p - nn;
                if j > 0 && j < n {
                    big_out[j] = Some(snap.quotes[k].amountOut);
                }
                continue;
            }
            let (i, j) = self.pair_indices(p);
            if i == 0 && j != 0 && j < n {
                zj_out[j] = Some(snap.quotes[k].amountOut);
            } else if j == 0 && i != 0 && i < n {
                j0_out[i] = Some(snap.quotes[k].amountOut);
            }
        }

        // 价格反推统一无费口径（与 L2 calldata 路径一致）：链上 quote 为含费
        // 可执行输出，需按 fee 反推无费 bid/ask/q0j/raw。BUY q0j 直接
        // `unfee_quote` 无费化（供 spot/预过滤，fee 由 apply_fee_rate 再折回）；
        // **bid/ask 不用 unfee 路径**——ceil 二次舍入会把误差放大 ±1
        // （0.004% 量级，NVDAx 实测），改用含费 quote 带费率直接反推
        // （`recover_ask_*` / 下方 bid 公式，与 1.4 SELL raw 的 ±1 修正同款精度）。
        let zj_nofee: Vec<Option<U256>> = zj_out
            .iter()
            .map(|o| o.and_then(|v| unfee_quote(v, self.fee_ppm)))
            .collect();
        // j→0（amountIn=10^(dj-4)，含费）→ 无费 bid：`bid = out×10^dj /
        // (fee_rem(10^(dj-4))×10^(d0-2))`（d0=6、fee=1000 时 = out×1e14/9.99e13，
        // 逐位等于链上 raw；unfee 路径会差 1）
        let d0_all = self.assets[0].decimals as u32;
        let bid_nofee: Vec<Option<U256>> = (0..n)
            .map(|j| {
                j0_out[j].filter(|out| !out.is_zero()).and_then(|out| {
                    if j == 0 {
                        return None;
                    }
                    let dj = self.assets[j].decimals as u32;
                    if dj > 30 || d0_all > 30 {
                        return None;
                    }
                    let in_j = U256::from(10u64).pow(U256::from(dj.saturating_sub(4)));
                    let rem = fee_rem(in_j, self.fee_ppm);
                    let num = out.checked_mul(U256::from(10u64).pow(U256::from(dj)))?;
                    let den = rem
                        .checked_mul(U256::from(10u64).pow(U256::from(d0_all.saturating_sub(2))))?;
                    if den.is_zero() {
                        None
                    } else {
                        Some(num / den)
                    }
                })
            })
            .collect();
        for j in 1..n {
            let d0 = self.assets[0].decimals as u32;
            let bid = bid_nofee[j];
            let zj = zj_nofee[j];
            // 0→j quote 成功返回 0：引擎禁用该资产买入（BUY 不可报价）
            let disabled = zj_out[j] == Some(U256::ZERO);
            // 保鲜判断（日志优先、快照补缺）：该资产 update 日志已给出 >= 快照块的
            // 价格时，快照 quote 不覆盖本地（防止旧块 quote 回退新日志价格）；
            // 容量/禁用状态仍以快照为准（见 1.5 节）
            let log_fresh = snap_block != 0
                && self.price_updated_block.get(j).copied().unwrap_or(0) >= snap_block;
            if disabled {
                self.buy_disabled[j] = true;
                self.q0j[j] = Some(U256::ZERO);
                // BUY 容量字段一并归零：buy_disabled 只门控 engine_quote，
                // calculate_price/max_achievable_out 直接读 buy_ladder_remaining，
                // 残留 alive 期非零值会让 spot/可达性预过滤误判方向存活。
                self.buy_ladder_remaining[j] = Some(U256::ZERO);
                if !log_fresh {
                    // 卖出侧看 bid：可用则 price=bid（SELL 报价精确），否则清空
                    if let Some(b) = bid {
                        self.prices[j] = b;
                        self.spreads[j] = 0;
                        self.bid_offsets[j] = 0;
                        self.ask_offsets[j] = 0;
                    } else {
                        self.prices[j] = U256::ZERO;
                        self.spreads[j] = 0;
                        self.bid_offsets[j] = 0;
                        self.ask_offsets[j] = 0;
                    }
                }
                continue;
            }
            self.buy_disabled[j] = false;
            if !log_fresh {
                // BUY 小额报价（无费口径，与 L2 q0j 一致）
                self.q0j[j] = zj.filter(|z| !z.is_zero());
            }
            match (bid, zj) {
                (Some(b), Some(_)) => {
                    if !log_fresh {
                        // bid 权威 = 无费 j→0 小额 quote；快照不解析 ladder 偏移，
                        // 直接存 price=bid、bid_offset=0（bid_price == price 精确）；
                        // ask 由 q0j 反推（跨资产第二段需要），存 ask_offset = ask - price；
                        // 低小数位资产 q0j 反推误差大，big 线性（>= q0j×10^4）时
                        // 用大额 probe 反推精确 ask（链上实测 asset2 需此修正）
                        self.prices[j] = b;
                        self.spreads[j] = 0;
                        self.bid_offsets[j] = 0;
                        let dj = self.assets[j].decimals as u32;
                        let ask_eff = big_out[j]
                            .filter(|qb| !qb.is_zero())
                            .and_then(|qb| {
                                // 阈值比较用含费口径（两边同含费，比值一致）
                                let q4 = zj_out[j]?.checked_mul(U256::from(10_000))?;
                                if qb < q4 {
                                    return None; // 封顶，非线性
                                }
                                Self::recover_ask_big(
                                    qb,
                                    U256::from(10u64).pow(U256::from(d0 + 4)),
                                    dj,
                                    d0,
                                    self.fee_ppm,
                                )
                            })
                            .or_else(|| Self::recover_ask_eff(zj_out[j]?, dj, d0, self.fee_ppm))
                            .unwrap_or(U256::ZERO);
                        self.ask_offsets[j] = ask_eff.saturating_sub(b).to::<u64>();
                    }
                }
                (Some(b), None) => {
                    if !log_fresh {
                        // 仅 bid 可用（0→j 缺失/失败）：默认点差诊断（BUY 不可报价）
                        self.spreads[j] = BINARYFI_DEFAULT_SPREAD;
                        self.prices[j] = b + U256::from(BINARYFI_DEFAULT_SPREAD / 2);
                        self.bid_offsets[j] = BINARYFI_DEFAULT_SPREAD / 2;
                        self.ask_offsets[j] = (BINARYFI_DEFAULT_SPREAD + 1) / 2;
                    }
                }
                (None, Some(_)) => {
                    if !log_fresh {
                        // 仅 BUY 可用（j→0 缺失/为 0）：SELL 不可报价（price=0），
                        // ask 由 q0j 反推（price=0 → ask_price = ask_offset）
                        let dj = self.assets[j].decimals as u32;
                        let ask_eff = big_out[j]
                            .filter(|qb| !qb.is_zero())
                            .and_then(|qb| {
                                let q4 = zj_out[j]?.checked_mul(U256::from(10_000))?;
                                if qb < q4 {
                                    return None;
                                }
                                Self::recover_ask_big(
                                    qb,
                                    U256::from(10u64).pow(U256::from(d0 + 4)),
                                    dj,
                                    d0,
                                    self.fee_ppm,
                                )
                            })
                            .or_else(|| Self::recover_ask_eff(zj_out[j]?, dj, d0, self.fee_ppm))
                            .unwrap_or(U256::ZERO);
                        self.prices[j] = U256::ZERO;
                        self.spreads[j] = 0;
                        self.bid_offsets[j] = 0;
                        self.ask_offsets[j] = ask_eff.to::<u64>();
                    }
                }
                (None, None) => {
                    if !log_fresh {
                        // 两侧报价均缺失/为 0：清掉旧价格，防止残留 bid/ask
                        // 继续驱动费率（下一次快照恢复）
                        self.prices[j] = U256::ZERO;
                        self.spreads[j] = 0;
                        self.bid_offsets[j] = 0;
                        self.ask_offsets[j] = 0;
                    }
                }
            }
        }

        // 1.4) 反推 SELL 精确有理数分子 raw = price - sell_off（无费口径）：
        let d0 = self.assets[0].decimals as u32;
        let k0 = d0.saturating_sub(2);
        //    - `bid = 带费率直接反推的 j→0 小额 quote`（= raw 权威；d0=6 时
        //      小额 in=10^(dj-4) 经 fee_rem 后线性系数恰为 1，反推输出 = raw
        //      精确整数，unfee 路径会差 1）
        //    - 100 整枚 probe（含费）线性时用 `est = qb×10^di /
        //      (fee_rem(big_in)×10^(d0-2))` 在 `[bid−1, bid+1]` 内反推，
        //      修正 unfee_quote 的 ±1 舍入（fee=1000 时代替历史
        //      `[1000×bid, 1000×bid+999]` 的 raw/1000 小数部分反推）
        //    - probe 归零（超容量）→ `bid`（δ 不可观测，等价整数 bid 线性）；
        //      probe 与单档不兼容（多档阶梯，如 DOG）→ None（engine_quote 同样兜底
        //      `bid_price`，但测试可识别放宽）
        let p10a = |e: u32| U256::from(10u64).pow(U256::from(e));
        for j in 1..n {
            let Some(bid) = bid_nofee[j].filter(|o| !o.is_zero()) else {
                continue;
            };
            let dj = self.assets[j].decimals as u32;
            if dj > 30 {
                continue;
            }
            let big_in = U256::from(100u64)
                .checked_mul(p10a(dj))
                .unwrap_or(U256::ZERO);
            let raw = match big_sell_out[j] {
                Some(qb) if !qb.is_zero() && !big_in.is_zero() => {
                    // raw_est = qb × 10^di / (fee_rem(big_in)×10^(d0-2))
                    // （big_in 为 100 整枚，含费输出 = fee_rem(big_in)×raw×10^(d0-2)/10^di）
                    let rem = fee_rem(big_in, self.fee_ppm);
                    let num = qb.checked_mul(p10a(dj)).unwrap_or(U256::ZERO);
                    let den = rem.checked_mul(p10a(k0)).unwrap_or(U256::ZERO);
                    if den.is_zero() {
                        Some(bid)
                    } else {
                        let est = num / den;
                        let ok = |r: U256| -> bool {
                            if r < bid.saturating_sub(U256::from(1))
                                || r > bid + U256::from(1)
                                || r.is_zero()
                            {
                                return false;
                            }
                            let out = fee_rem(big_in, self.fee_ppm)
                                .checked_mul(r)
                                .and_then(|v| v.checked_mul(p10a(k0)))
                                .map(|v| v / p10a(dj))
                                .unwrap_or(U256::ZERO);
                            out == qb
                        };
                        if ok(est) {
                            Some(est)
                        } else if ok(est + U256::from(1)) {
                            Some(est + U256::from(1))
                        } else if est > U256::ZERO && ok(est - U256::from(1)) {
                            Some(est - U256::from(1))
                        } else if ok(bid) {
                            Some(bid)
                        } else {
                            // 多档阶梯：probe 与单档线性不兼容
                            None
                        }
                    }
                }
                _ => Some(bid),
            };
            self.sell_raw[j] = raw;
        }

        // 1.5) 恢复阶梯上限（引擎 per-asset cap = ladderWeight × engineReserve）：
        //   - BUY maxOut：0→j 大额 quote 被截断（q_big < q_small·10^4）时 q_big 即 maxOut
        //   - SELL maxIn：j→0 100 整枚 quote 被截断（q < q_small·10^6）时
        //     maxIn = q·10^di / (q_small·10^4)（q_small = bid·10^(d0-2)/10^4）
        for j in 1..n {
            // BUY 侧：0→j 大额 quote 被截断（q_big < q_small·10^4）时 q_big 即 maxOut
            // （饱和型，min）；q_big 为 0 表示阶梯容量 > 金库余额（超阈值归零型），
            // 有效上限 = 金库余额，linear > 金库余额时引擎返回 0。
            // 每次快照以本次 probe 为准：先清除旧 cap 再按结果设置，避免引擎
            // 调整 ladder（cap 从有到无）后本地残留旧上限导致报价被错误截断。
            if let (Some(qs), Some(qb)) = (zj_out[j], big_out[j]) {
                if !qs.is_zero() {
                    if qb.is_zero() {
                        self.buy_zero_over_vault[j] = true;
                        self.max_outputs[j] = None;
                        self.buy_ladder_remaining[j] = None;
                    } else if qb < qs.checked_mul(U256::from(10_000)).unwrap_or(U256::MAX) {
                        // 阶梯退化检测：mid probe（1000x 小额，仍在线性区）输出
                        // > big probe 输出 → 曲线非单调回落（NVDAx 实测：1e9→
                        // 4.456e18 线性、≥5e9 骤降平顶 1.301e18），big 落在退化
                        // 平顶区，不是全输入范围的有效 maxOut。若用其截断线性区
                        // 会低估 66%（in=867,053,194 本地 1.301e18 vs 链上 3.863e18）。
                        // 清掉 maxOut，线性区报价恢复；超大额由 buy_capped 的
                        // 实时金库零门槛兜底（防幻影利润）。
                        let degenerate = mid_out[j].is_some_and(|qm| !qm.is_zero() && qm > qb);
                        if degenerate {
                            self.max_outputs[j] = None;
                            self.buy_zero_over_vault[j] = false;
                            // 退化平顶不是有效 maxOut；但 update 推导的 Σqty×R
                            // 是精确容量（asset2 实测），保留作为封顶
                        } else {
                            self.max_outputs[j] = Some(qb);
                            self.buy_zero_over_vault[j] = false;
                            // 快照容量权威：饱和型 probe 直接观测当前 maxOut，
                            // 覆盖（可能已过时的）update 推导剩余容量
                            self.buy_ladder_remaining[j] = Some(qb);
                        }
                    } else {
                        self.max_outputs[j] = None;
                        self.buy_zero_over_vault[j] = false;
                        // 大额 probe 未饱和：无容量信息，保留 update 推导的
                        // Σqty×R（精确容量，asset2 实测大额 probe 不饱和）
                    }
                }
            }
            // SELL 侧：j→0 100 整枚 quote 被截断（q < q_small·10^6）时
            // maxIn = q·10^di / (q_small·10^4)（q_small = bid·10^(d0-2)/10^4）。
            // j→0 小额 quote 权威为 0 → 引擎 SELL 容量为 0（ladder×reserve=0），
            // 写死 Some(0) 上限；update 日志只更新价格、不复活该方向容量。
            match j0_out[j] {
                Some(qs) if qs.is_zero() => {
                    self.max_inputs[j] = Some(U256::ZERO);
                }
                Some(qs) => {
                    if let Some(qb) = big_sell_out[j] {
                        let lin = qs.checked_mul(U256::from(1_000_000)).unwrap_or(U256::MAX);
                        if qb.is_zero() || qb >= lin {
                            self.max_inputs[j] = None;
                        } else {
                            let dj = self.assets[j].decimals as u32;
                            let den = qs.checked_mul(U256::from(10_000)).unwrap_or(U256::ZERO);
                            if dj <= 30 && !den.is_zero() {
                                let num = qb.checked_mul(U256::from(10u64).pow(U256::from(dj)));
                                if let Some(v) = num {
                                    // maxIn_real = ceil(q_big·10^di / (q_small·10^4))：
                                    // floor 会低估 1 单位，导致饱和区输出 sim = chain - 1
                                    self.max_inputs[j] = Some((v + den - U256::from(1)) / den);
                                }
                            }
                        }
                    }
                }
                None => {}
            }
        }

        // 1.5b) SELL 多档阶梯储备 R 反推（ladder 已知 + 100 整枚 probe 饱和时）：
        //   R = q_big×10^di / (10^(d0−2)×Σ(price−w_k)×qty_k)；重算 probe 输出与
        //   q_big 逐位一致且 probe 饱和才采用；此时 maxIn = Σqty×R（精确输入容量），
        //   覆盖 1.5 节 probe 反推的单档近似值。R 链上长期稳定（引擎存储，非金库
        //   余额），快照周期性刷新即可，无需 Swap 事件跟踪。
        for j in 1..n {
            let Some(ladder) = self.sell_ladders.get(j).cloned().flatten() else {
                continue;
            };
            if ladder.is_empty() {
                continue;
            }
            let Some(qb) = big_sell_out[j].filter(|q| !q.is_zero()) else {
                continue;
            };
            let Some(p) = self.prices.get(j).copied().filter(|p| !p.is_zero()) else {
                continue;
            };
            let dj = self.assets[j].decimals as u32;
            if dj > 30 {
                continue;
            }
            let mut sum = U256::ZERO;
            for &(w, q) in &ladder {
                if let Some(v) = p.checked_sub(U256::from(w)) {
                    sum = sum + v.checked_mul(U256::from(q)).unwrap_or(U256::ZERO);
                }
            }
            if sum.is_zero() {
                continue;
            }
            let num = qb.checked_mul(p10a(dj)).unwrap_or(U256::ZERO);
            let den = p10a(k0).checked_mul(sum).unwrap_or(U256::ZERO);
            if den.is_zero() {
                continue;
            }
            let base = num / den;
            let mut found = None;
            'cand: for d in 0..=2u32 {
                for cand in [base.saturating_sub(U256::from(d)), base + U256::from(d)] {
                    if cand.is_zero() {
                        continue;
                    }
                    if self.verify_sell_probe(j, cand, qb).is_some() {
                        found = Some(cand);
                        break 'cand;
                    }
                }
            }
            if let Some(r) = found {
                self.ladder_reserves[j] = Some(r);
                let total: U256 = ladder
                    .iter()
                    .fold(U256::ZERO, |acc, &(_, q)| acc + U256::from(q));
                self.max_inputs[j] = Some(total * r);
            } else {
                // 闭式候选未命中（100 整枚 probe 未饱和时总容量 > 输入，
                // 饱和公式失效）→ 单调二分求解：out(R) 关于 R 单调不减
                // （每档 consume = min(rem_k, q_k×R)），下界 = 闭式饱和解
                // base（未饱和时实际输出 ≤ 饱和输出 → 真 R ≥ base），
                // 上界倍增到 out(R) ≥ q_big，再二分最小命中 R（逐位一致）。
                let probe_in = U256::from(100u64)
                    .checked_mul(p10a(dj))
                    .unwrap_or(U256::ZERO);
                if !probe_in.is_zero() {
                    let mut lo = base;
                    let mut hi = base.max(U256::from(1));
                    for _ in 0..256 {
                        let out_hi =
                            ladder_sell_out(p, &ladder, hi, probe_in, d0, dj, self.fee_ppm)
                                .unwrap_or(U256::ZERO);
                        if out_hi >= qb {
                            break;
                        }
                        match hi.checked_mul(U256::from(2)) {
                            Some(h) => hi = h,
                            None => {
                                hi = U256::MAX;
                                break;
                            }
                        }
                    }
                    while lo < hi {
                        let mid = lo + (hi - lo) / U256::from(2);
                        let out_mid =
                            ladder_sell_out(p, &ladder, mid, probe_in, d0, dj, self.fee_ppm)
                                .unwrap_or(U256::ZERO);
                        if out_mid < qb {
                            lo = mid + U256::from(1);
                        } else {
                            hi = mid;
                        }
                    }
                    if ladder_sell_out(p, &ladder, lo, probe_in, d0, dj, self.fee_ppm) == Some(qb) {
                        self.ladder_reserves[j] = Some(lo);
                        let total: U256 = ladder
                            .iter()
                            .fold(U256::ZERO, |acc, &(_, q)| acc + U256::from(q));
                        self.max_inputs[j] = Some(total * lo);
                    }
                }
            }
        }

        // 2) 由 bid/ask 推导全部费率（精确 BigInt）
        for i in 0..n {
            for j in 0..n {
                if i == j {
                    continue;
                }
                if let Some(rate) = self.derive_rate(i, j) {
                    let idx = self.pair_index(i, j);
                    self.rates[idx] = rate;
                    derived += 1;
                }
            }
        }

        // 3) 价格未知的 pair 兜底：quote 锚定（保证可用，非精度来源）
        for (k, pair) in snap.quotePairs.iter().enumerate() {
            if k >= snap.quotes.len() || !snap.quotes[k].success {
                continue;
            }
            let (i, j) = self.pair_indices(pair.to::<usize>());
            if i >= n || j >= n || i == j {
                continue;
            }
            if self.rates[self.pair_index(i, j)].is_zero() {
                let di = self.assets[i].decimals as u32;
                let d0 = self.assets[0].decimals as u32;
                if di <= 30 && d0 <= 30 {
                    // 与批量合约报价金额一致（0→j 用 10^d0，其余用 10^(di-4)）
                    let amount_in = if i == 0 {
                        U256::from(10u64).pow(U256::from(d0))
                    } else {
                        U256::from(10u64).pow(U256::from(di.saturating_sub(4)))
                    };
                    self.set_rate(i, j, amount_in, snap.quotes[k].amountOut);
                }
            }
        }
        derived
    }

    /// 批量拉取快照：静态调用批量合约取 quotes/decimals/余额（合约内部调用
    /// 引擎 getAssetReserves() 填金库余额）；若为空则单独调用引擎兜底。
    async fn fetch_snapshot<N, P>(
        provider: P,
        pool: Address,
        engine: Address,
        router: Address,
        assets: Vec<Address>,
        quote_pairs: Vec<U256>,
        big_quote_pairs: Vec<U256>,
        big_sell_pairs: Vec<U256>,
        block: BlockId,
    ) -> Result<Snapshot, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let return_data = GetBinaryFiPropStateBatchRequest::deploy_builder(
            provider.clone(),
            pool,
            engine,
            router,
            assets,
            quote_pairs,
            big_quote_pairs,
            big_sell_pairs,
        )
        .call_raw()
        .block(block)
        .await?;
        let mut snap = <Snapshot as SolValue>::abi_decode(&return_data)?;

        // 兜底：批量合约未取到金库余额时单独调用引擎 getAssetReserves()
        if snap.vaultReserves.is_empty() {
            let engine_contract = IBinaryFiEngine::new(engine, provider.clone());
            if let Ok(ret) = engine_contract.getAssetReserves().call().block(block).await {
                snap.vaultReserves = ret.reserves;
            }
        }
        Ok(snap)
    }
}

// ============================================================================
// 精度辅助
// ============================================================================

/// 解码左对齐 ladder 字段（update calldata data0/data1）：每档 24 bit
/// （12bit weight + 12bit qty），最多 10 档；遇到全零档即终止。
/// 空/全零 → None（无阶梯信息）。
fn decode_ladder(data: U256) -> Option<Vec<(u32, u32)>> {
    let mut out = Vec::with_capacity(10);
    for k in 0..10u32 {
        let shift = 256 - 24 * (k + 1);
        let bits = (data >> U256::from(shift)) & U256::from(0xffffffu64);
        if bits.is_zero() {
            break;
        }
        let weight = (bits >> U256::from(12)).to::<u32>();
        let qty = (bits & U256::from(0xfffu64)).to::<u32>();
        if qty == 0 {
            continue;
        }
        out.push((weight, qty));
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// SELL 逐档输出（纯函数，与引擎逐位一致）：
/// `rem = in − in×fee/1e6`（费率因子先扣，fee=1000 时 = in − in/1000；
/// 链上实测 in<1000 时与 in×999/1000 不同，必须整数除法），
/// 每档 `consume = min(rem, qty_k×R)`，
/// `out += (price − w_k) × consume × 10^(d0−2) / 10^di`，rem 递减。
fn ladder_sell_out(
    price: U256,
    ladder: &[(u32, u32)],
    reserve: U256,
    amount_in: U256,
    d0: u32,
    di: u32,
    fee_ppm: u64,
) -> Option<U256> {
    if di > 30 || d0 > 30 || reserve.is_zero() || ladder.is_empty() {
        return None;
    }
    let p10 = |e: u32| U256::from(10u64).pow(U256::from(e));
    let k0 = d0.saturating_sub(2);
    let scale = p10(k0);
    let den = p10(di);
    let mut rem = fee_rem(amount_in, fee_ppm);
    let mut out = U256::ZERO;
    for &(w, q) in ladder {
        if rem.is_zero() {
            break;
        }
        let cap = U256::from(q).checked_mul(reserve)?;
        let consume = rem.min(cap);
        let p_eff = price.checked_sub(U256::from(w))?;
        out = out.checked_add(p_eff.checked_mul(consume)?.checked_mul(scale)? / den)?;
        rem = rem.checked_sub(consume)?;
    }
    Some(out)
}

/// 引擎输入侧扣费：`rem = in − in×fee/1e6`（整数除法）。
/// fee=1000 时逐位等于历史 `in − in/1000`；fee=200 时 = `in − in/5000`。
/// 乘法溢出（实际不可能：输入受引擎容量上限约束）时回退历史 1000ppm 口径。
fn fee_rem(amount_in: U256, fee_ppm: u64) -> U256 {
    let fee = U256::from(fee_ppm);
    match amount_in.checked_mul(fee) {
        Some(v) => amount_in - v / U256::from(1_000_000u64),
        None => amount_in - amount_in / U256::from(1000),
    }
}

/// 无费 rate 转"含当前 fee 的可执行中间价"：`rate × (1e6 − fee) / 1e6`
/// （输出侧乘；与引擎输入侧扣费在线性区等价，供 spot/预过滤使用）。
fn apply_fee_rate(rate: Rate, fee_ppm: u64) -> Option<Rate> {
    let one = U256::from(1_000_000u64);
    let fee = U256::from(fee_ppm.min(1_000_000));
    let num = rate.num.checked_mul(one.checked_sub(fee)?)?;
    let den = rate.den.checked_mul(one)?;
    Some(Rate { num, den })
}

/// 链上含费 quote → 无费 quote：`out_nofee = ceil(out × 1e6 / (1e6 − fee))`
/// （引擎 `out = floor(out_nofee × (1e6−fee)/1e6)`，反推取 ceil；对实际
/// fee 范围（≤1000）误差 ≤ 1，由下游 ±1 反推验证吸收；fee=0 时恒等）。
/// 仅供快照**价格反推**使用（bid/ask/q0j/raw 需无费口径与 L2 路径统一）；
/// 容量/饱和判断与报价验证仍用原始含费 quote（比值中 fee 抵消）。
fn unfee_quote(out: U256, fee_ppm: u64) -> Option<U256> {
    if fee_ppm == 0 {
        return Some(out);
    }
    let one = U256::from(1_000_000u64);
    let den = one.checked_sub(U256::from(fee_ppm.min(1_000_000)))?;
    let num = out.checked_mul(one)?;
    Some((num + den - U256::from(1)) / den)
}

/// 将 U256 全精度转换为 f64
fn u256_to_f64(value: &U256) -> f64 {
    let limbs = value.as_limbs();
    let mut result = limbs[0] as f64;
    result += (limbs[1] as f64) * (2.0f64.powi(64));
    result += (limbs[2] as f64) * (2.0f64.powi(128));
    result += (limbs[3] as f64) * (2.0f64.powi(192));
    result
}

// ============================================================================
// AutomatedMarketMaker 实现
// ============================================================================

impl AutomatedMarketMaker for BinaryFiPropPool {
    fn address(&self) -> Address {
        if !self.virtual_address.is_zero() {
            self.virtual_address
        } else {
            self.pool_address
        }
    }

    fn supported_chains(&self) -> Option<Vec<u64>> {
        Some(vec![BINARYFI_CHAIN_ID])
    }

    fn sync_events(&self) -> Vec<B256> {
        vec![
            BINARYFI_SWAP_EVENT,
            BINARYFI_UPDATE_EVENT,
            BINARYFI_FEE_EVENT,
        ]
    }

    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError> {
        let topics = log.topics();

        // L0: 引擎 FeeUpdated 事件 → 按账户费率实时更新。
        // 费率是引擎 per-account storage（getFee），报价在输入侧按 fee_ppm 扣；
        // 无费价格/阶梯无需重校准，仅更新 fee_ppm 即让后续 engine_quote/sell_out
        // 立即对齐链上（事件与 setFee 交易同块）。
        // 实际变更时返回 AsyncUpdate：快照观测的含费 maxOut/maxIn/大额 probe
        // 依赖旧费率（快照与事件之间费率已变），需按新费率重新探测容量锚点；
        // 费率未变（重复事件）不触发。
        if log.address() == self.engine_address
            && topics.len() == 1
            && topics[0] == BINARYFI_FEE_EVENT
        {
            let data = log.data().data.as_ref();
            if data.len() >= 32 {
                let new_fee = U256::from_be_slice(&data[..32])
                    .min(U256::from(1_000_000u64))
                    .to::<u64>();
                if new_fee != self.fee_ppm {
                    tracing::debug!(
                        old_fee = self.fee_ppm,
                        new_fee = new_fee,
                        block = log.block_number,
                        "binaryfi engine fee event"
                    );
                    self.fee_ppm = new_fee;
                    // 同步重导全部 rates（spot/预过滤立即对齐新费率），
                    // 不依赖 AsyncUpdate 快照的异步窗口；无费价格/阶梯不变。
                    let n = self.assets.len();
                    for i in 0..n {
                        for j in 0..n {
                            if i == j {
                                continue;
                            }
                            if let Some(rate) = self.derive_rate(i, j) {
                                let idx = self.pair_index(i, j);
                                self.rates[idx] = rate;
                            }
                        }
                    }
                    return Ok(SyncAction::AsyncUpdate);
                }
            }
            return Ok(SyncAction::None);
        }

        // L1: 池子 Swap 事件 → 余额增减 + 费率锚定
        if log.address() == self.pool_address
            && topics.len() == 4
            && topics[0] == BINARYFI_SWAP_EVENT
        {
            let token_in = Address::from_word(topics[2]);
            let token_out = Address::from_word(topics[3]);
            let data = log.data().data.as_ref();
            if data.len() < 64 {
                return Ok(SyncAction::Resync);
            }
            let amount_in = U256::from_be_slice(&data[..32]);
            let amount_out = U256::from_be_slice(&data[32..64]);
            let (Some(i), Some(j)) = (self.token_index(token_in), self.token_index(token_out))
            else {
                return Ok(SyncAction::Resync);
            };
            if i == j {
                return Ok(SyncAction::Resync);
            }
            // 共享金库账本（部署级）：同一 router/engine/vault 的所有虚拟子池
            // 共用金库，任何 Swap（无论是否落在本实例 exposed pair 内）都增减
            // reserves、消耗 BUY 阶梯容量。2026-08-19 事故根因：跨 pair SELL
            // 抽干 USDT0 金库对本实例不可见 → 本地金库门控放行必然失败的交易。
            self.update_shared_vault(i, j, amount_in, amount_out);
            // reserves 事件净变化入账（rebase-merge 用）：块号缺失（canonical
            // 路径）时标记 incomplete，快照合并退化为 guard，防止合并偏差。
            match log.block_number {
                Some(blk) => self.ledger.record(
                    blk,
                    i,
                    amount_in.to::<u128>() as i128,
                    j,
                    -(amount_out.to::<u128>() as i128),
                ),
                None => self.ledger.mark_incomplete(),
            }
            // 费率锚定仍限本 pair（防跨 pair 污染价格）：仅当 Swap 完全落在自身
            // exposed pair 内才用成交价标定；跨 pair 只更新金库、不动价格。
            if let Some((a, b)) = self.exposed_pair {
                if (i == a || i == b) && (j == a || j == b) {
                    self.anchor_prices_only(i, j, amount_in, amount_out);
                }
            } else {
                self.anchor_prices_only(i, j, amount_in, amount_out);
            }
            return Ok(SyncAction::None);
        }

        // L2/L3: 引擎 update 日志
        if log.address() == self.engine_address
            && topics.len() == 2
            && topics[0] == BINARYFI_UPDATE_EVENT
        {
            let asset_idx = U256::from_be_bytes(topics[1].0).to::<usize>();
            // 虚拟子池只处理自身暴露 pair 涉及的资产价格更新
            if let Some((a, b)) = self.exposed_pair {
                if a != asset_idx && b != asset_idx {
                    return Ok(SyncAction::None);
                }
            }
            let data = log.data().data.as_ref();
            // 增强后的 data 携带 (price, blockNumber, data0..2, askOffsetRaw, bidOffsetRaw)
            // 7 个 word；price + 点差（由 ladder 前 16 位解析）一次到位
            if data.len() >= 224 {
                let price = U256::from_be_slice(&data[..32]);
                let block_number = U256::from_be_slice(&data[32..64]).to::<u64>();
                let data0 = U256::from_be_slice(&data[64..96]);
                let data1 = U256::from_be_slice(&data[96..128]);
                let ask_offset_raw = U256::from_be_slice(&data[160..192]).to::<u64>();
                let bid_offset_raw = U256::from_be_slice(&data[192..224]).to::<u64>();
                // 买入被禁用的资产（0→j quote=0）：calldata price 无法确定性恢复
                // 真实 bid（链上 bid 与 calldata mid 的偏移随更新变化），走批量
                // 快照从 j→0 quote 精确恢复 bid + spread=0。
                if self.buy_disabled.get(asset_idx).copied().unwrap_or(false) {
                    // 卖出方向同样被禁（快照权威观测 j→0 quote=0 → maxIn=0，bid 不可用）：
                    // 资产完全不可交易，本次价格更新无意义，直接忽略，避免无谓 AsyncUpdate。
                    if self.max_inputs.get(asset_idx).copied().flatten() == Some(U256::ZERO) {
                        return Ok(SyncAction::None);
                    }
                    self.mark_stale_for_asset(asset_idx);
                    return Ok(SyncAction::AsyncUpdate);
                }
                self.apply_l2_update_full(
                    asset_idx,
                    price,
                    block_number,
                    ask_offset_raw,
                    bid_offset_raw,
                    data0,
                    data1,
                );
                return Ok(SyncAction::None);
            }
            // canonical 路径无 raw bytes：标记 stale，交由 update() 批量刷新；
            // 时效时钟仍以本事件块号推进——链上引擎每次 update 交易都会写
            // per-asset lastUpdateBlock（即使价格/ladder 不变，实测 67430645/47
            // 两笔相同 NVDAx update 都刷新了），仅靠快照不会刷新时效，若此处不
            // 推进，时效门控在 canonical 路径会整体失效（用快照价格算出链上已
            // 过期=0 的幻影利润）。asset2/3 每 1~3 块被 MM 重新 update，5 块窗口
            // 从不过期（“差 8 块仍新鲜”= 67430648 时真实 lastUpdate 已是
            // 67430643/67430647）；NVDAx/SPYx 更新间隔长，diff=6 起链上 quote=0。
            if let Some(ev_block) = log.block_number {
                if let Some(updated) = self.price_updated_block.get_mut(asset_idx) {
                    *updated = (*updated).max(ev_block);
                }
            }
            self.mark_stale_for_asset(asset_idx);
            return Ok(SyncAction::AsyncUpdate);
        }

        Ok(SyncAction::Resync)
    }

    fn last_synced_block(&self) -> u64 {
        self.last_synced_block
    }

    fn set_last_synced_block(&mut self, block_number: u64) {
        // 单调不回退（与 UniswapV3 一致）：防止周期任务/AsyncUpdate
        // 用更旧块号覆盖本地已推进的日志状态
        self.last_synced_block = self.last_synced_block.max(block_number);
    }

    fn tokens(&self) -> Vec<Address> {
        if let Some((a, b)) = self.exposed_pair {
            let mut out = Vec::with_capacity(2);
            if let Some(ta) = self.assets.get(a) {
                out.push(ta.address);
            }
            if let Some(tb) = self.assets.get(b) {
                out.push(tb.address);
            }
            return out;
        }
        self.assets.iter().map(|t| t.address).collect()
    }

    fn calculate_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
        let Some(i) = self.token_index(base_token) else {
            return Err(AMMError::TokenNotFound(base_token));
        };
        let Some(j) = self.token_index(quote_token) else {
            return Err(AMMError::TokenNotFound(quote_token));
        };
        if i == j {
            return Ok(0.0);
        }
        let rate = self
            .rates
            .get(self.pair_index(i, j))
            .copied()
            .unwrap_or_default();
        if rate.is_zero() {
            return Ok(0.0);
        }
        // 时效：任一端价格过期 → 池子不可交易（链上 quote 返回 0）
        if !self.asset_price_fresh(i) || !self.asset_price_fresh(j) {
            return Ok(0.0);
        }
        // 方向可交易性（spot 与 simulate_swap/链上 quote 对齐）：链上该方向容量
        // 恒为 0 时 quote 恒 0，spot 也必须为 0，避免 multihop/2hop 预过滤把
        // 已死方向排高（最终 sim 虽会剪掉，但白占候选位）。
        //   - SELL（j==0）：maxIn = ladderWeight_sell×engineReserve == 0（MM
        //     只买不卖/引擎储备归零）；快照路径 prices 已清零天然为 0，L2 路径
        //     prices 来自 calldata 非零，此处补齐门控
        //   - BUY（i==0）：buy_ladder_remaining（Σqty×R）或快照 maxOut == 0
        //   - 跨资产：输入侧 SELL 或输出侧 BUY 任一为死方向 → 0
        // 未知（None）不门控：与 ladder_cap_known/96% 兜底口径一致。
        let sell_dead = |k: usize| self.max_inputs.get(k).copied().flatten() == Some(U256::ZERO);
        let buy_dead = |k: usize| {
            self.buy_ladder_remaining.get(k).copied().flatten() == Some(U256::ZERO)
                || self.max_outputs.get(k).copied().flatten() == Some(U256::ZERO)
        };
        if (j == 0 && sell_dead(i))
            || (i == 0 && buy_dead(j))
            || (i != 0 && j != 0 && (sell_dead(i) || buy_dead(j)))
        {
            return Ok(0.0);
        }
        let di = self.assets[i].decimals as i32;
        let dj = self.assets[j].decimals as i32;
        Ok(u256_to_f64(&rate.num) / u256_to_f64(&rate.den) * 10f64.powi(di - dj))
    }

    fn has_sufficient_liquidity(&self) -> bool {
        self.rates.iter().any(|r| !r.is_zero()) && self.reserves.iter().any(|r| !r.is_zero())
    }

    fn decimals(&self, token: Address) -> u8 {
        self.token_index(token)
            .and_then(|i| self.assets.get(i))
            .map(|t| t.decimals)
            .unwrap_or(0)
    }

    fn simulate_swap(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let Some(i) = self.token_index(token_in) else {
            return Err(AMMError::TokenNotFound(token_in));
        };
        let Some(j) = self.token_index(token_out) else {
            return Err(AMMError::TokenNotFound(token_out));
        };
        if i == j || amount_in.is_zero() {
            return Ok(U256::ZERO);
        }
        // 虚拟子池只对自身暴露 pair 报价（防御性校验；tokens() 已限定）
        if let Some((a, b)) = self.exposed_pair {
            let hit = (i == a || i == b) && (j == a || j == b);
            if !hit {
                return Ok(U256::ZERO);
            }
        }
        let (out, cap_known) = match self.engine_quote(i, j, amount_in) {
            Some(out) => (out, self.ladder_cap_known(i, j)),
            None => {
                // 价格未知：退回费率锚定（保证可用）
                let rate = self
                    .rates
                    .get(self.pair_index(i, j))
                    .copied()
                    .unwrap_or_default();
                if rate.is_zero() {
                    return Ok(U256::ZERO);
                }
                (
                    amount_in
                        .checked_mul(rate.num)
                        .map(|v| v / rate.den)
                        .unwrap_or(U256::ZERO),
                    false,
                )
            }
        };
        Ok(if cap_known {
            out
        } else {
            self.capped_out(j, out)
        })
    }

    fn simulate_swap_mut(
        &mut self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let Some(i) = self.token_index(token_in) else {
            return Err(AMMError::TokenNotFound(token_in));
        };
        let Some(j) = self.token_index(token_out) else {
            return Err(AMMError::TokenNotFound(token_out));
        };
        if i == j || amount_in.is_zero() {
            return Ok(U256::ZERO);
        }
        // 虚拟子池只对自身暴露 pair 报价（防御性校验；tokens() 已限定）
        if let Some((a, b)) = self.exposed_pair {
            let hit = (i == a || i == b) && (j == a || j == b);
            if !hit {
                return Ok(U256::ZERO);
            }
        }
        let (out, cap_known) = match self.engine_quote(i, j, amount_in) {
            Some(out) => (out, self.ladder_cap_known(i, j)),
            None => {
                let rate = self
                    .rates
                    .get(self.pair_index(i, j))
                    .copied()
                    .unwrap_or_default();
                if rate.is_zero() {
                    return Ok(U256::ZERO);
                }
                (
                    amount_in
                        .checked_mul(rate.num)
                        .map(|v| v / rate.den)
                        .unwrap_or(U256::ZERO),
                    false,
                )
            }
        };
        let out = if cap_known {
            out
        } else {
            self.capped_out(j, out)
        };
        if let Some(r) = self.reserves.get_mut(i) {
            *r = r.saturating_add(amount_in);
        }
        if let Some(r) = self.reserves.get_mut(j) {
            *r = r.saturating_sub(out);
        }
        Ok(out)
    }

    fn simulate_swap_exact_out(
        &self,
        token_in: Address,
        token_out: Address,
        amount_out: U256,
    ) -> Result<U256, AMMError> {
        let Some(i) = self.token_index(token_in) else {
            return Err(AMMError::TokenNotFound(token_in));
        };
        let Some(j) = self.token_index(token_out) else {
            return Err(AMMError::TokenNotFound(token_out));
        };
        if i == j || amount_out.is_zero() {
            return Err(AMMError::Msg("binaryfi: invalid exact out".to_string()));
        }
        // 虚拟子池只对自身暴露 pair 报价（防御性校验；tokens() 已限定）
        if let Some((a, b)) = self.exposed_pair {
            let hit = (i == a || i == b) && (j == a || j == b);
            if !hit {
                return Err(AMMError::Msg("binaryfi: pair not exposed".to_string()));
            }
        }
        let rate = self
            .rates
            .get(self.pair_index(i, j))
            .copied()
            .unwrap_or_default();
        if rate.is_zero() {
            return Err(AMMError::Msg("binaryfi: no rate for pair".to_string()));
        }
        // 输出可达性：用精确 cap（maxOut/maxIn/金库）而非 96% 兜底，避免高估合法输出
        match self.max_achievable_out(i, j) {
            Some(max_out) if amount_out > max_out => {
                return Err(AMMError::Msg(
                    "binaryfi: amount out exceeds pool cap".to_string(),
                ));
            }
            Some(_) => {}
            // cap 完全未知（价格也未恢复）时退回 96% 兜底检查
            None => {
                if amount_out > self.capped_out(j, amount_out) {
                    return Err(AMMError::Msg(
                        "binaryfi: amount out exceeds pool cap".to_string(),
                    ));
                }
            }
        }
        let num = amount_out
            .checked_mul(rate.den)
            .ok_or(AMMError::ArithmeticError)?;
        let (q, r) = (num / rate.num, num % rate.num);
        Ok(if r.is_zero() { q } else { q + U256::from(1) })
    }

    #[instrument(skip_all, fields(pool = %self.pool_address))]
    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        // 全量快照：assets 未知 → 传空由合约 getAssets() 获取；132 对 quote +
        // 11 对 (0→j) 大额 quote（用于锁定 ask）
        let mut quote_pairs = Vec::with_capacity(BINARYFI_ASSET_COUNT * BINARYFI_ASSET_COUNT);
        for i in 0..BINARYFI_ASSET_COUNT {
            for j in 0..BINARYFI_ASSET_COUNT {
                if i != j {
                    quote_pairs.push(U256::from(i * BINARYFI_ASSET_COUNT + j));
                }
            }
        }
        // 11 对 (0→j) 大额（1e10，锁定 ask + BUY maxOut）+ 11 对中额
        // （1e9，检测非单调阶梯退化）
        let mut big_quote_pairs = Vec::with_capacity(2 * (BINARYFI_ASSET_COUNT - 1));
        for j in 1..BINARYFI_ASSET_COUNT {
            big_quote_pairs.push(U256::from(BINARYFI_ASSET_COUNT * BINARYFI_ASSET_COUNT + j));
            big_quote_pairs.push(U256::from(
                3 * BINARYFI_ASSET_COUNT * BINARYFI_ASSET_COUNT + j,
            ));
        }
        // 11 对 (j→0) 100 整枚大额报价：恢复 SELL 侧 maxIn 上限
        let mut big_sell_pairs = Vec::with_capacity(BINARYFI_ASSET_COUNT - 1);
        for j in 1..BINARYFI_ASSET_COUNT {
            big_sell_pairs.push(U256::from(
                2 * BINARYFI_ASSET_COUNT * BINARYFI_ASSET_COUNT + j,
            ));
        }
        // 固定到具体块号：快照 quote 与保鲜判断（日志价格 >= snap_block 不覆盖）同块一致，
        // 且避免 provider 被 fetch_snapshot 移动后再使用
        let snap_block = match block_number {
            BlockId::Number(alloy::eips::BlockNumberOrTag::Number(num)) => num,
            _ => provider.get_block_number().await?,
        };
        let block = BlockId::Number(alloy::eips::BlockNumberOrTag::Number(snap_block));
        let snap = Self::fetch_snapshot(
            provider,
            self.pool_address,
            self.engine_address,
            self.router_address,
            vec![],
            quote_pairs,
            big_quote_pairs,
            big_sell_pairs,
            block,
        )
        .await?;
        let derived = self.apply_snapshot(&snap, snap_block);
        if derived == 0 {
            tracing::warn!(target: "amms::binaryfi_prop", pool = %self.pool_address, "binaryfi: init snapshot derived 0 rates");
        }
        self.last_synced_block = snap_block;
        Ok(self)
    }

    #[instrument(skip_all, fields(pool = %self.pool_address, stale = self.stale_pairs.len()))]
    async fn update<N, P>(&mut self, provider: P) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        self.update_at(provider, BlockId::latest()).await
    }
}

impl BinaryFiPropPool {
    /// 在指定区块拉取批量快照并刷新 stale pair（StateSpace update 与回放验证共用）。
    pub async fn update_at<N, P>(&mut self, provider: P, block: BlockId) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let n = self.assets.len();
        if n == 0 {
            return Ok(());
        }
        // 固定到具体块号：快照 quote 与保鲜判断（日志价格 >= snap_block 不覆盖）同块一致，
        // 避免 BlockId::latest() 的隐式取数块与本地日志推进产生竞态
        let snap_block: u64 = match block {
            BlockId::Number(alloy::eips::BlockNumberOrTag::Number(num)) => num,
            _ => provider.get_block_number().await?,
        };
        let block = BlockId::Number(alloy::eips::BlockNumberOrTag::Number(snap_block));
        let snap = self.fetch_stale_snapshot(provider, block).await?;
        self.apply_snapshot(&snap, snap_block);
        if !snap.quotePairs.is_empty() {
            let refreshed: Vec<usize> = snap.quotePairs.iter().map(|p| p.to::<usize>()).collect();
            self.clear_stale_pairs(&refreshed);
        }
        Ok(())
    }

    /// 按当前 stale_pairs 组装 quote 请求并发起批量快照（只读，不修改本地状态）。
    ///
    /// update_at 与 fetch_full_snapshot 共用：一次链上批量读取，避免两处重复
    /// 组装 big_quote/big_sell 请求参数。
    async fn fetch_stale_snapshot<N, P>(
        &self,
        provider: P,
        block: BlockId,
    ) -> Result<Snapshot, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let n = self.assets.len();
        let assets: Vec<Address> = self.assets.iter().map(|t| t.address).collect();
        let quote_pairs: Vec<U256> = self.stale_pairs.iter().map(|&p| U256::from(p)).collect();
        // stale 资产涉及的 (0→j) 方向附带大额报价，锁定 ask + BUY maxOut
        let mut big_quote_pairs: Vec<U256> = Vec::new();
        // stale 资产涉及的 (j→0) 100 整枚报价，恢复 SELL maxIn
        let mut big_sell_pairs: Vec<U256> = Vec::new();
        for &p in &self.stale_pairs {
            let (i, j) = self.pair_indices(p);
            if i == 0 && j != 0 {
                big_quote_pairs.push(U256::from(n * n + j));
                big_quote_pairs.push(U256::from(3 * n * n + j));
            }
            if i != 0 && j == 0 {
                big_sell_pairs.push(U256::from(2 * n * n + i));
            }
        }
        Self::fetch_snapshot(
            provider,
            self.pool_address,
            self.engine_address,
            self.router_address,
            assets,
            quote_pairs,
            big_quote_pairs,
            big_sell_pairs,
            block,
        )
        .await
    }

    /// 拉取并返回全量快照（只读，不修改本地状态）：标记全部资产 stale 后做一次
    /// 链上批量读取，返回 `(snapshot, snap_block)`。
    ///
    /// 供 state_space 周期任务按部署做"一次链上调用、多实例内存分发"：各虚拟子池
    /// 实例自行调用 [`Self::apply_snapshot`]，利用实例自身的 `price_updated_block`
    /// 保鲜判断（日志价格 >= snap_block 不覆盖），避免周期快照回退更新日志价格。
    pub(crate) async fn fetch_full_snapshot<N, P>(
        &mut self,
        provider: P,
    ) -> Result<(Snapshot, u64), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let n = self.assets.len();
        if n == 0 {
            let empty = Snapshot {
                assets: Vec::new(),
                decimals: Vec::new(),
                scales: Vec::new(),
                poolBalances: Vec::new(),
                vaultReserves: Vec::new(),
                vaultBalances: Vec::new(),
                quotePairs: Vec::new(),
                quotes: Vec::new(),
                fee: U256::ZERO,
            };
            return Ok((empty, 0));
        }
        for j in 1..n {
            self.mark_stale_for_asset(j);
        }
        let snap_block = provider.get_block_number().await?;
        let block = BlockId::Number(alloy::eips::BlockNumberOrTag::Number(snap_block));
        let snap = self.fetch_stale_snapshot(provider, block).await?;
        Ok((snap, snap_block))
    }
}

impl BinaryFiPropPool {
    /// 输出截断兜底：out ≤ 96% * 金库余额(tokenOut)；余额未知时不截断。
    ///
    /// 仅当该方向的阶梯上限（maxIn/maxOut）未知时启用（见 `ladder_cap_known`），
    /// 保守近似防止超大额输入的虚假报价；已知精确上限时不叠加，避免低估。
    fn capped_out(&self, j: usize, out: U256) -> U256 {
        match self.vault_known(j) {
            Some(reserve) => {
                let cap = reserve * U256::from(BINARYFI_MAX_OUTPUT_BPS) / U256::from(10_000);
                out.min(cap)
            }
            None => out,
        }
    }
}

impl Default for BinaryFiPropPool {
    fn default() -> Self {
        Self {
            pool_address: BINARYFI_POOL_ADDRESS,
            virtual_address: Address::ZERO,
            exposed_pair: None,
            engine_address: BINARYFI_ENGINE_ADDRESS,
            vault_address: BINARYFI_VAULT_ADDRESS,
            router_address: BINARYFI_ROUTER_ADDRESS,
            fee_ppm: BINARYFI_DEFAULT_FEE_PPM,
            chain_id: BINARYFI_CHAIN_ID,
            created_block: 0,
            last_synced_block: 0,
            assets: Vec::new(),
            prices: Vec::new(),
            spreads: Vec::new(),
            bid_offsets: Vec::new(),
            ask_offsets: Vec::new(),
            q0j: Vec::new(),
            sell_raw: Vec::new(),
            price_scales: Vec::new(),
            buy_disabled: Vec::new(),
            buy_zero_over_vault: Vec::new(),
            max_outputs: Vec::new(),
            max_inputs: Vec::new(),
            reserves: Vec::new(),
            rates: Vec::new(),
            stale_pairs: Vec::new(),
            price_updated_block: Vec::new(),
            sell_ladders: Vec::new(),
            buy_ladders: Vec::new(),
            buy_ladder_remaining: Vec::new(),
            ladder_reserves: Vec::new(),
            price0_calibrated: false,
            ledger: ReservesDeltaLedger::default(),
        }
    }
}

// ============================================================================
// Flashblocks 原始交易增强
// ============================================================================

/// 批量 update（`0x34f7f748`，2026-08-19 引擎升级后新格式）的日志增强。
///
/// calldata 布局：selector(4) + head 6 words + data 区。head =
/// `[dataOffset, sigOffset, timestamp, blockNumber, _, _]`，dataOffset 为字节偏移
/// （相对 data 区起点，即 selector 之后），指向 count word；其后是
/// count × 6 words 记录：`(asset_idx, price, askOff, bidOff,
/// data0=sellLadder, data1=buyLadder)`（192B/条）。
///
/// 引擎每笔批量交易发出 N 条 Update 日志（data 为空、topic[1] = asset_idx），
/// 本函数按日志 asset_idx 挑对应记录，注入与单资产 update 相同的 7-word data：
/// `(price, blockNumber, data0, data1, data2=0, askOffsetRaw, bidOffsetRaw)`。
///
/// ladder 编码与单资产 update 一致（前 16 位为点差偏移字段，实际偏移
/// = (字段/16) × scale/10000）；记录中的显式 askOff/bidOff 字段**不参与报价**
/// （链上实测 4 资产：ask = price + (data1>>240)/16 逐位吻合、显式字段不符）。
/// 找不到对应资产记录 / 布局非法时返回 `None`（调用方保留原始日志走 L3）。
fn enrich_batch_update_log_data(input: &[u8], log_data: &LogData) -> Option<LogData> {
    if input.len() < 4 + 32 * 6 + 32 + BINARYFI_BATCH_UPDATE_RECORD_LEN {
        return None;
    }
    let d = &input[4..];
    let u256 = |off: usize| U256::from_be_slice(&d[off..off + 32]);
    let data_offset = u256(0).to::<usize>();
    // dataOffset 相对 data 区起点；count word 至少应位于 6 个 head word 之后
    if data_offset < 32 * 6 {
        return None;
    }
    let block_number = u256(3 * 32).to::<u64>();
    let count = u256(data_offset).to::<usize>();
    if count == 0 || count > 64 {
        return None;
    }
    let rec_start = data_offset + 32;
    let rec_end = rec_start.checked_add(count.checked_mul(BINARYFI_BATCH_UPDATE_RECORD_LEN)?)?;
    if rec_end > d.len() {
        return None;
    }
    let target = U256::from_be_bytes(log_data.topics().get(1)?.0).to::<usize>();
    // 同资产多次出现时取最后一条（保留循环最后一次命中）
    let mut found: Option<(U256, U256, U256)> = None;
    for k in 0..count {
        let r = rec_start + k * BINARYFI_BATCH_UPDATE_RECORD_LEN;
        if u256(r).to::<usize>() == target {
            let price = u256(r + 32);
            let data0 = u256(r + 128);
            let data1 = u256(r + 160);
            found = Some((price, data0, data1));
        }
    }
    let (price, data0, data1) = found?;
    // ladder 左对齐：前 16 位为点差偏移字段（实际偏移 = (字段/16) × scale/10000）
    let ask_offset_raw = (data1 >> U256::from(240)) / U256::from(16);
    let bid_offset_raw = (data0 >> U256::from(240)) / U256::from(16);

    let mut words = Vec::with_capacity(224);
    for w in [
        price,
        U256::from(block_number),
        data0,
        data1,
        U256::ZERO, // data2：批量记录无此字段，透传 0（sync 不消费）
        ask_offset_raw,
        bid_offset_raw,
    ] {
        words.extend_from_slice(&w.to_be_bytes::<32>());
    }
    LogData::new(log_data.topics().to_vec(), Bytes::from(words))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amms::Token;

    fn u(v: u128) -> U256 {
        U256::from(v)
    }

    fn test_pool(n: usize) -> BinaryFiPropPool {
        let mut pool = BinaryFiPropPool::default();
        pool.assets = (0..n)
            .map(|i| Token {
                address: Address::repeat_byte(i as u8),
                decimals: 18,
                symbol: String::new(),
                chain_id: BINARYFI_CHAIN_ID,
                fot_tax: None,
            })
            .collect();
        pool.reserves = vec![U256::ZERO; n];
        pool
    }

    /// S ≥ last_event_block：直接锚定，清空账本
    #[test]
    fn test_ledger_anchor_fresh() {
        let mut ledger = ReservesDeltaLedger::default();
        let mut cur = vec![u(100), u(200)];
        let snap = vec![u(300), u(400)];
        assert_eq!(ledger.rebase(&mut cur, &snap, 100), LedgerApply::Anchored);
        assert_eq!(cur, snap);
        assert!(ledger.is_anchored());
        // 账本已清空：再锚定仍走 Anchored
        assert_eq!(ledger.rebase(&mut cur, &snap, 101), LedgerApply::Anchored);
    }

    /// 重启后首次锚定（base=0）：快照 + 块 > S 事件 replay
    #[test]
    fn test_ledger_replay_first_anchor() {
        let mut ledger = ReservesDeltaLedger::default();
        ledger.record(101, 0, -50, 1, 40);
        ledger.record(102, 0, -10, 1, 8);
        let mut cur = vec![u(1000), u(1000)]; // 旧状态，被快照覆盖
        let snap = vec![u(500), u(600)]; // canonical 头 S=100
        assert_eq!(ledger.rebase(&mut cur, &snap, 100), LedgerApply::Merged);
        // snap + (块101+102 Δ) = [500-60, 600+48]
        assert_eq!(cur, vec![u(440), u(648)]);
        assert!(ledger.is_anchored());
    }

    /// rebase 精确性：锚定后事件推进，S 落在 (base, latest]，合并 = 手工重放
    #[test]
    fn test_ledger_rebase_precise() {
        let mut ledger = ReservesDeltaLedger::default();
        let mut cur = vec![u(500), u(600)];
        assert_eq!(
            ledger.rebase(&mut cur, &vec![u(500), u(600)], 100),
            LedgerApply::Anchored
        );
        // 事件 101/102 应用（当前值含事件）+ 记录
        cur[0] = u(430);
        cur[1] = u(648);
        ledger.record(101, 0, -60, 1, 48);
        ledger.record(102, 0, -10, 1, 0);
        // 快照 S=101（链上 101 块状态）→ 合并 = snap + 块102 Δ
        assert_eq!(
            ledger.rebase(&mut cur, &vec![u(430), u(660)], 101),
            LedgerApply::Merged
        );
        assert_eq!(cur, vec![u(420), u(660)]);
    }

    /// 陈旧快照（S < base）：丢弃，事件账本保持
    #[test]
    fn test_ledger_stale_skip() {
        let mut ledger = ReservesDeltaLedger::default();
        let mut cur = vec![u(500), u(600)];
        assert_eq!(
            ledger.rebase(&mut cur, &vec![u(500), u(600)], 100),
            LedgerApply::Anchored
        );
        cur[0] = u(490);
        ledger.record(101, 0, -10, 1, 0);
        assert_eq!(
            ledger.rebase(&mut cur, &vec![u(400), u(600)], 99),
            LedgerApply::SkippedStale
        );
        assert_eq!(cur, vec![u(490), u(600)]);
    }

    /// 同块多笔 swap 累加进同一条目
    #[test]
    fn test_ledger_same_block_accumulate() {
        let mut ledger = ReservesDeltaLedger::default();
        ledger.record(101, 0, -50, 1, 40);
        ledger.record(101, 0, -10, 1, 8);
        let mut cur = vec![u(1000), u(1000)];
        assert_eq!(
            ledger.rebase(&mut cur, &vec![u(500), u(600)], 100),
            LedgerApply::Merged
        );
        // 同块两笔 → 单条目 [-60, 48]
        assert_eq!(cur, vec![u(440), u(648)]);
    }

    /// 乱序/重复事件（block < last_event_block）：忽略，防双计
    #[test]
    fn test_ledger_out_of_order_ignore() {
        let mut ledger = ReservesDeltaLedger::default();
        ledger.record(102, 0, -10, 1, 8);
        ledger.record(101, 0, -50, 1, 40); // 乱序 → 忽略
        ledger.record(102, 0, -5, 1, 2); // 同块重复 → 累加
        let mut cur = vec![u(1000), u(1000)];
        assert_eq!(
            ledger.rebase(&mut cur, &vec![u(500), u(600)], 100),
            LedgerApply::Merged
        );
        assert_eq!(cur, vec![u(485), u(610)]); // 只含块102 的 [-15, +10]
    }

    /// 窗口挤出：base 后事件被容量挤出 → 退化 guard（保持事件账本）；
    /// S 覆盖全部事件时仍可锚定
    #[test]
    fn test_ledger_window_miss() {
        let mut ledger = ReservesDeltaLedger::new(2);
        let mut cur = vec![u(1000), u(1000)];
        assert_eq!(
            ledger.rebase(&mut cur, &vec![u(1000), u(1000)], 100),
            LedgerApply::Anchored
        );
        // 事件 101/102/103：capacity=2 → 块101 被挤出
        for blk in [101u64, 102, 103] {
            cur[0] = cur[0].saturating_sub(U256::from(10));
            cur[1] = cur[1].saturating_add(U256::from(5));
            ledger.record(blk, 0, -10, 1, 5);
        }
        assert_eq!(cur, vec![u(970), u(1015)]);
        // S=102：base=100 < evicted_up_to=101 → 无法精确合并 → 退化
        assert_eq!(
            ledger.rebase(&mut cur, &vec![u(980), u(1010)], 102),
            LedgerApply::SkippedWindowMiss
        );
        assert_eq!(cur, vec![u(970), u(1015)]); // 事件账本保持
                                                // S=103 ≥ last_event_block=103 → 直接锚定
        assert_eq!(
            ledger.rebase(&mut cur, &vec![u(960), u(1030)], 103),
            LedgerApply::Anchored
        );
        assert_eq!(cur, vec![u(960), u(1030)]);
    }

    /// 事件把本地余额扣到负（saturating 兜底）后 rebase 不 panic、方向正确
    #[test]
    fn test_ledger_saturating_clamp() {
        let mut ledger = ReservesDeltaLedger::default();
        let mut cur = vec![u(10), u(20)];
        assert_eq!(
            ledger.rebase(&mut cur, &vec![u(10), u(20)], 100),
            LedgerApply::Anchored
        );
        // 事件 101：asset0 -50（本地 saturating 到 0），asset1 +5
        cur[0] = U256::ZERO;
        cur[1] = u(25);
        ledger.record(101, 0, -50, 1, 5);
        // 快照 S=100（=base）：合并 = snap + 块101 Δ，clamp 到非负
        assert_eq!(
            ledger.rebase(&mut cur, &vec![u(10), u(25)], 100),
            LedgerApply::Merged
        );
        assert_eq!(cur, vec![U256::ZERO, u(30)]);
    }

    /// 锚定后 reserves==0 = 已空（权威），参与归零/截断；未锚定 0 = 未知
    #[test]
    fn test_vault_known_zero_after_anchor() {
        let mut pool = test_pool(2);
        pool.reserves = vec![u(0), u(100)];
        assert_eq!(pool.vault_known(0), None); // 未锚定：未知
        assert_eq!(pool.vault_known(1), Some(u(100)));
        assert_eq!(pool.sell_zero_over_vault(u(5)), Some(u(5))); // 不归零
        assert_eq!(pool.capped_out(0, u(5)), u(5)); // 不截断

        let snap = vec![u(0), u(100)];
        assert_eq!(
            pool.ledger.rebase(&mut pool.reserves, &snap, 100),
            LedgerApply::Anchored
        );
        assert_eq!(pool.vault_known(0), Some(U256::ZERO)); // 锚定后：已空
        assert_eq!(pool.sell_zero_over_vault(u(5)), Some(U256::ZERO)); // 归零
        assert_eq!(pool.capped_out(0, u(5)), U256::ZERO); // 截断为 0
    }

    /// 序列化跳过 ledger；重启后 ledger 空，首个快照 replay 精确恢复
    #[test]
    fn test_ledger_serde_roundtrip() {
        let mut pool = test_pool(2);
        pool.reserves = vec![u(1000), u(1000)];
        pool.ledger.record(101, 0, -50, 1, 40);
        let json = serde_json::to_string(&pool).unwrap();
        assert!(!json.contains("ledger"), "ledger must not be serialized");
        let mut p2: BinaryFiPropPool = serde_json::from_str(&json).unwrap();
        assert!(!p2.ledger.is_anchored());
        // 重启后新事件
        p2.reserves = vec![u(1000), u(1000)];
        p2.ledger.record(102, 0, -10, 1, 8);
        // 首个快照 S=100 → replay = snap + 块102 Δ
        assert_eq!(
            p2.ledger
                .rebase(&mut p2.reserves, &vec![u(500), u(600)], 100),
            LedgerApply::Merged
        );
        assert_eq!(p2.reserves, vec![u(490), u(608)]);
    }

    /// 真实 XLayer 事故场景回放（2026-08-31 13:35，块 69414266-71）：
    /// USDT0 金库起点 27,774，swap 事件累计后余额 = 6,988（= 链上 balanceOf）。
    /// 旧实现：快照 S=69414269（vaultBalances=649,051,239，canonical 滞后）
    /// 无条件覆盖 → 幻影 649M 持续 15s（0xc0ca1d 等 9 笔失败报价）。
    /// 新实现：rebase-merge → 649,051,239 + 块 69414270/71 净变化 = 6,988。
    #[test]
    fn test_replay_real_xlayer_scenario() {
        let mut ledger = ReservesDeltaLedger::default();
        let mut cur = vec![u(27_774), U256::ZERO]; // 事件账本（sync 已应用）
                                                   // 69414266: +1,048,650,414 / -1,034,838,367 / +470,458,782
        cur[0] = cur[0].saturating_add(u(1_048_650_414));
        ledger.record(69_414_266, 0, 1_048_650_414, 1, 0);
        cur[0] = cur[0].saturating_sub(u(1_034_838_367));
        ledger.record(69_414_266, 0, -1_034_838_367, 1, 0);
        cur[0] = cur[0].saturating_add(u(470_458_782));
        ledger.record(69_414_266, 0, 470_458_782, 1, 0);
        // 69414267: -483,897,106
        cur[0] = cur[0].saturating_sub(u(483_897_106));
        ledger.record(69_414_267, 0, -483_897_106, 1, 0);
        // 69414269: +648,649,742
        cur[0] = cur[0].saturating_add(u(648_649_742));
        ledger.record(69_414_269, 0, 648_649_742, 1, 0);
        // 69414270: -648,449,350 / -590,295 / +408,888,610 / -407,889,807 / -1,003,374
        for d in [
            -648_449_350i128,
            -590_295,
            408_888_610,
            -407_889_807,
            -1_003_374,
        ] {
            if d > 0 {
                cur[0] = cur[0].saturating_add(u(d as u128));
            } else {
                cur[0] = cur[0].saturating_sub(u((-d) as u128));
            }
            ledger.record(69_414_270, 0, d, 1, 0);
        }
        // 69414271: +290,727,961 / -289,903,586 / -824,410
        for d in [290_727_961i128, -289_903_586, -824_410] {
            if d > 0 {
                cur[0] = cur[0].saturating_add(u(d as u128));
            } else {
                cur[0] = cur[0].saturating_sub(u((-d) as u128));
            }
            ledger.record(69_414_271, 0, d, 1, 0);
        }
        assert_eq!(cur[0], u(6_988)); // 事件账本精确 = 链上 balanceOf

        // 场景 1（重启后首次锚定）：快照 S=69414269 = 该块链上值 649,051,239
        let snap = vec![u(649_051_239), U256::ZERO];
        assert_eq!(
            ledger.rebase(&mut cur, &snap, 69_414_269),
            LedgerApply::Merged
        );
        // 合并 = 快照 + 块 69414270/71 净变化 = 6,988（幻影容量消失）
        assert_eq!(cur[0], u(6_988));

        // 场景 2（rebase，S=69414270）：新快照 7,023 → 仍精确 = 6,988
        let snap2 = vec![u(7_023), U256::ZERO];
        assert_eq!(
            ledger.rebase(&mut cur, &snap2, 69_414_270),
            LedgerApply::Merged
        );
        assert_eq!(cur[0], u(6_988));
    }

    /// 无块号日志（canonical 路径）→ incomplete：中间块快照退化为 guard，
    /// S ≥ last_event_block 时仍可锚定并重置
    #[test]
    fn test_ledger_incomplete_degrades_to_guard() {
        let mut ledger = ReservesDeltaLedger::default();
        let mut cur = vec![u(500), u(600)];
        assert_eq!(
            ledger.rebase(&mut cur, &vec![u(500), u(600)], 100),
            LedgerApply::Anchored
        );
        ledger.record(102, 0, -10, 1, 5);
        ledger.mark_incomplete();
        cur[0] = u(490);
        cur[1] = u(605);
        // 中间块 S=101：无法精确合并 → 保持事件账本
        assert_eq!(
            ledger.rebase(&mut cur, &vec![u(480), u(610)], 101),
            LedgerApply::SkippedWindowMiss
        );
        assert_eq!(cur, vec![u(490), u(605)]);
        // S=102 ≥ last_event_block=102：直接锚定并重置 incomplete
        assert_eq!(
            ledger.rebase(&mut cur, &vec![u(470), u(620)], 102),
            LedgerApply::Anchored
        );
        assert_eq!(cur, vec![u(470), u(620)]);
    }
}

/// 从 flashblocks 的原始交易字节中定位引擎 `update` 交易并解析 calldata，
/// 返回注入 7 个 word（price / blockNumber / data0..2 / askOffsetRaw /
/// bidOffsetRaw）后的日志 data。其中：
///   - `data0` = sellLadder、`data1` = buyLadder（左对齐 256 位字段），前 16 位为
///     点差偏移字段；`askOffsetRaw = (data1 >> 240) / 16`、
///     `bidOffsetRaw = (data0 >> 240) / 16`（ladder 空间单位，scale 由池子应用）
///
/// 支持旧单资产 update（`0x024b94f6`）与 2026-08-19 升级后的批量 update
/// （`0x34f7f748`，见 `enrich_batch_update_log_data`）。找不到对应 raw bytes、
/// RLP 解码失败或目标/选择器不匹配时返回 `None`，调用方应保留原始日志原样。
pub fn enrich_update_log_data(
    raw_txs: &[impl AsRef<str>],
    tx_hash: Option<B256>,
    log_data: &LogData,
    engine_address: Address,
) -> Option<LogData> {
    use alloy::consensus::Transaction;
    use alloy::rlp::Decodable;

    let tx_hash = tx_hash?;

    let mut raw_bytes: Option<Vec<u8>> = None;
    for raw in raw_txs {
        let s = raw.as_ref().strip_prefix("0x").unwrap_or(raw.as_ref());
        let Ok(bytes) = alloy::hex::decode(s) else {
            continue;
        };
        if keccak256(&bytes) == tx_hash {
            raw_bytes = Some(bytes);
            break;
        }
    }
    let raw = raw_bytes?;

    let mut slice: &[u8] = raw.as_slice();
    let envelope = alloy::consensus::TxEnvelope::decode(&mut slice).ok()?;
    let (input, to) = match &envelope {
        alloy::consensus::TxEnvelope::Legacy(tx) => (&tx.tx().input, tx.tx().to()),
        alloy::consensus::TxEnvelope::Eip2930(tx) => (&tx.tx().input, tx.tx().to()),
        alloy::consensus::TxEnvelope::Eip1559(tx) => (&tx.tx().input, tx.tx().to()),
        _ => return None,
    };
    if to? != engine_address {
        return None;
    }
    if input.len() < 4 {
        return None;
    }
    if input[..4] == BINARYFI_BATCH_UPDATE_SELECTOR {
        return enrich_batch_update_log_data(input, log_data);
    }
    if input.len() < BINARYFI_UPDATE_CALLDATA_LEN || input[..4] != BINARYFI_UPDATE_SELECTOR {
        return None;
    }
    // 布局: index / offset / blockNumber / price / a / b / data0..2(96B) /
    // data_len / sig_len / sig(96B)；price 与 ladder(data0/data1) 的点差字段
    // 参与报价，a / b / data2 透传
    let d = &input[4..];
    let price = U256::from_be_slice(&d[96..128]);
    let block_number = U256::from_be_slice(&d[64..96]);
    let data0 = U256::from_be_slice(&d[192..224]);
    let data1 = U256::from_be_slice(&d[224..256]);
    let data2 = U256::from_be_slice(&d[256..288]);
    // ladder 左对齐：前 16 位为点差偏移字段（实际偏移 = (字段/16) × scale/10000）
    let ask_offset_raw = (data1 >> U256::from(240)) / U256::from(16);
    let bid_offset_raw = (data0 >> U256::from(240)) / U256::from(16);

    let mut words = Vec::with_capacity(224);
    for w in [
        price,
        block_number,
        data0,
        data1,
        data2,
        ask_offset_raw,
        bid_offset_raw,
    ] {
        words.extend_from_slice(&w.to_be_bytes::<32>());
    }
    LogData::new(log_data.topics().to_vec(), Bytes::from(words))
}

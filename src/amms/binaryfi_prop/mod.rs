//! # BinaryFi propAMM (XLayer)
//!
//! BinaryFi 是 XLayer 上的 proprietary AMM（PropAMM）：链下做市引擎定期通过
//! `update(...)` 交易向引擎合约提交带签名的资产价格，池子合约对外提供线性报价
//! `quote()`，单笔输出受引擎侧 per-asset 输出上限与金库余额约束。
//!
//! ## 报价模型（已通过 structLogs 反汇编 + mainfork 采样逐位验证）
//!
//! 引擎以 USDT0 为 numéraire（`p0 = 100` 固定，无点差），每资产维护整数中间价
//! `price` 与整数点差 `spread = ask - bid`（价格单位，可为奇数 → 中间价为分数，
//! 存储值取 `floor`）：
//!
//! - `bid_i = price_i - spread_i/2`，`ask_i = price_i + (spread_i+1)/2`
//! - `USDT0→j`：`linear = floor(in * 10^(dj-d0+2) / ask_j)`，再按阶梯上限截断：
//!   - 饱和型（阶梯容量 ≤ 金库余额，大额 probe 可观测 maxOut）：`out = min(linear, maxOut)`
//!   - 超阈值归零型（阶梯容量 > 金库余额，大额 probe = 0）：`linear > 金库余额 → 0`，
//!     否则原样返回（有效上限 = 金库余额，由 `getAssetReserves` 观测）
//! - `i→USDT0`：`out = floor(in * bid_i * 10^(d0-2) / 10^di)`
//! - `i→j`（非 USDT0）：两段式（与引擎一致，含中间截断）：
//!   `v = floor(in * bid_i * 10^(d0-2) / 10^di)`（受 maxIn_i 截断），
//!   `out = floor(v * 10^(dj-d0+2) / ask_j)`（再按上述 BUY 规则截断/归零）
//! - `i→USDT0` 输入受 SELL 阶梯上限 `maxIn_i` 截断（`min(in, maxIn_i)`），
//!   `maxIn = ladderWeight_sell × engineReserve`，由 100 整枚 probe 精确恢复
//!
//! 验证样例（fork 块 67160388，当前价格）：
//! - `0→SKHYx, in=35,357,671` → `235,576,460,790,192,551`（真实套利交易）
//! - `SKHYx→0, in=10^18` → `150,010,000`（bid = 15001）
//! - `0→SKHYx, in=10^6` → `6,662,669,065,227,530`（ask = 15009，spread = 8）
//! - `0→SKHYx, in=10^10` → `7,643,300,000,000,000,000`（饱和型 maxOut = 61×1.253e17）
//! - `0→xSOL, in=11,870,000` → `160,861,905`，`in=11,880,000` → `0`
//!   （归零型：linear 超金库 160,992,934 后引擎返回 0）
//! - 跨资产 `SKHYx→xETH` 等两段式逐位一致；update calldata 提交的 price 与
//!   同块 bid/ask 恢复出的 mid 完全吻合（如 xETH price=186787）。
//!
//! ## 三层数据同步（事件驱动，无轮询）
//!
//! 1. **L1 — Swap 事件**（池子 `0x2d651e...`）：
//!    `Swap(sender, tokenIn, tokenOut, amountIn, amountOut)` 更新本地池子余额；
//!    费率以引擎价格精确推导为准（价格未知时才用 out/in 锚定兜底）。
//! 2. **L2 — flashblocks 原始交易增强**（仅实时流）：
//!    引擎 `update` 日志本身 data 为空；`xlayer_flashblocks` 解析器在 raw bytes
//!    边缘通过 `keccak256(raw) == tx_hash` 定位交易，RLP 解码后解析
//!    `0x024b94f6` calldata，把 `(price, blockNumber, data0..2)` 5 个 word 注入
//!    日志 data，随后 `sync()` 直接用 price 重算所有相关 pair 的费率，零 RPC。
//! 3. **L3 — 无 raw bytes 的 update 日志**（canonical get_logs 路径）：
//!    把相关 pair 标记 stale 并返回 `SyncAction::AsyncUpdate`，由 StateSpace 的
//!    pending_sync_queue 触发 `update()`：一次 `GetBinaryFiPropStateBatchRequest`
//!    静态调用批量拉取余额 + stale pair 的 `pool.quote`，从同块 `(0→j)/(j→0)`
//!    精确恢复每资产 bid/ask（含点差），再本地推导全部费率。

pub mod factory;

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
/// XLayer 默认部署：池子合约（Swap 事件、quote 报价）。
/// 配置化部署时地址应由 poolindex/loader 经 `BinaryFiPropFactory::new` 传入，
/// 此常量仅作为默认值与文档参考。
pub const BINARYFI_POOL_ADDRESS: Address = address!("0x2d651e3fe9470db52d211569a0ab7266c5180de7");
/// XLayer 默认部署：引擎合约（update 事件、价格提交）
pub const BINARYFI_ENGINE_ADDRESS: Address = address!("0xeacf260a16a4e16a758fc1bd126d49d8e02f9996");
/// XLayer 默认部署：金库合约（持有资产）
pub const BINARYFI_VAULT_ADDRESS: Address = address!("0x9b169052Ee1569Ec5bDF51DbF48D2962526cF6D9");
/// XLayer 默认部署：quote 的 recipient（Router，从真实交易获取）
pub const BINARYFI_ROUTER_ADDRESS: Address = address!("0xa1945aa291a99ea996b9c41cc645e30c1c01d190");
/// 池子 Swap 事件签名
pub const BINARYFI_SWAP_EVENT: B256 =
    b256!("cd3829a3813dc3cdd188fd3d01dcf3268c16be2fdd2dd21d0665418816e46062");
/// 引擎 Update 事件签名
pub const BINARYFI_UPDATE_EVENT: B256 =
    b256!("af186e2e77ac28f0c051cdd1e2b3b92924e34b314650186bbc14742e373751c8");
/// 引擎 update 函数选择器
pub const BINARYFI_UPDATE_SELECTOR: [u8; 4] = [0x02, 0x4b, 0x94, 0xf6];
/// update calldata 总长度（实测 452B）= selector(4) + head 6 words(192) +
/// data0..2(96) + data_len(32) + sig_len(32) + sig(96)
pub const BINARYFI_UPDATE_CALLDATA_LEN: usize = 4 + 32 * 6 + 96 + 32 + 32 + 96;
/// 单笔输出截断比例（96% 池子余额，本地近似）
pub const BINARYFI_MAX_OUTPUT_BPS: u64 = 9600;
/// USDT0 的引擎报价默认值（2 位小数定点）
pub const BINARYFI_PRICE0_DEFAULT: u64 = 100;
/// 价格未知资产的默认点差（ask-bid，价格单位；SKHYx 实测 = 8）
pub const BINARYFI_DEFAULT_SPREAD: u64 = 8;
/// 每次 swap 模拟消耗的默认 gas
pub const DEFAULT_SWAP_GAS: u64 = 250_000;

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
    /// 每资产卖方向偏移（bid = price - bid_offset；独立于 ask，可与 ask 不对称）
    #[serde(default)]
    pub bid_offsets: Vec<u64>,
    /// 每资产买方向偏移（ask = price + ask_offset）
    #[serde(default)]
    pub ask_offsets: Vec<u64>,
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
    /// 池子各资产余额（输出截断基准）
    pub reserves: Vec<U256>,
    /// 有向费率 num/den，index = i * N + j（对角为空）
    pub rates: Vec<Rate>,
    /// 待批量刷新 pair（index 同上）
    pub stale_pairs: Vec<usize>,
    /// 每资产最近一次价格更新块号（0 = 未知）
    pub price_updated_block: Vec<u64>,
    /// price0 是否已通过 Swap 锚定标定
    pub price0_calibrated: bool,
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

    /// bid = price - bid_offset（引擎卖方向偏移独立于买方向，见 apply_l2_update）
    pub fn bid_price(&self, i: usize) -> Option<U256> {
        // SELL 容量权威为 0（快照观测 j→0 quote=0）时，卖出价视为不可用，
        // 防止残留价格继续驱动该方向费率/报价（engine_quote 由 maxIn=0 兜底）
        if self.max_inputs.get(i).copied().flatten() == Some(U256::ZERO) {
            return None;
        }
        let p = self.prices.get(i).copied().filter(|p| !p.is_zero())?;
        let off = self
            .bid_offsets
            .get(i)
            .copied()
            .filter(|&o| o != 0)
            .unwrap_or_else(|| self.spread(i) / 2);
        Some(p.saturating_sub(U256::from(off)))
    }

    /// ask = price + ask_offset（引擎买方向偏移）
    pub fn ask_price(&self, i: usize) -> Option<U256> {
        let p = self.prices.get(i).copied().filter(|p| !p.is_zero())?;
        let off = self
            .ask_offsets
            .get(i)
            .copied()
            .filter(|&o| o != 0)
            .unwrap_or_else(|| (self.spread(i) + 1) / 2);
        Some(p + U256::from(off))
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
    /// 卖方向（data0）与买方向（data1）偏移独立：`bid = price - bid_offset`、
    /// `ask = price + ask_offset`（不对称时 price 非算术中点，仅作参考价）。
    /// 模块同时维护全点差 `spread = ask_offset + bid_offset`（兼容/诊断用）。
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
        let bid_off = bid_offset_raw.saturating_mul(factor);
        if let Some(o) = self.bid_offsets.get_mut(asset_idx) {
            *o = bid_off;
        }
        if let Some(o) = self.ask_offsets.get_mut(asset_idx) {
            *o = ask_off;
        }
        if let Some(s) = self.spreads.get_mut(asset_idx) {
            *s = ask_off.saturating_add(bid_off);
        }
        // update 只携带价格与 ladder 点差，不携带容量；SELL 0-cap（max_inputs=Some(0)）
        // 来自快照权威观测，不能被价格更新复活；容量恢复由下一次快照重新观测。
        self.apply_price_update(asset_idx, price, block_number);
    }

    /// 由 bid/ask 推导 rate(i→j)，与引擎线性区公式一致（约分后的大整数分数）
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
        if j == 0 {
            let bid = self.bid_price(i)?;
            if di >= k0 {
                Some(Rate {
                    num: bid,
                    den: p10(di - k0),
                })
            } else {
                Some(Rate {
                    num: bid.checked_mul(p10(k0 - di))?,
                    den: U256::from(1),
                })
            }
        } else if i == 0 {
            let ask = self.ask_price(j)?;
            if dj >= k0 {
                Some(Rate {
                    num: p10(dj - k0),
                    den: ask,
                })
            } else {
                Some(Rate {
                    num: U256::from(1),
                    den: ask.checked_mul(p10(k0 - dj))?,
                })
            }
        } else {
            let bid = self.bid_price(i)?;
            let ask = self.ask_price(j)?;
            if dj >= di {
                Some(Rate {
                    num: bid.checked_mul(p10(dj - di))?,
                    den: ask,
                })
            } else {
                Some(Rate {
                    num: bid,
                    den: ask.checked_mul(p10(di - dj))?,
                })
            }
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

    /// Swap 事件锚定：余额增减 + price0 标定。
    ///
    /// 费率以引擎价格精确推导为准；仅当方向两侧价格未知时才用 Swap 的
    /// `out/in` 锚定（保证池子在未收到 update 时可用，不作为精度来源）。
    pub fn anchor_rate(&mut self, i: usize, j: usize, amount_in: U256, amount_out: U256) {
        let price_known = self.prices.get(i).map(|p| !p.is_zero()).unwrap_or(false)
            && self.prices.get(j).map(|p| !p.is_zero()).unwrap_or(false);
        if !price_known {
            self.set_rate(i, j, amount_in, amount_out);
        }
        if let Some(r) = self.reserves.get_mut(i) {
            *r = r.saturating_add(amount_in);
        }
        if let Some(r) = self.reserves.get_mut(j) {
            *r = r.saturating_sub(amount_out);
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

    /// 从 `(0→j, amountIn=10^d0)` 的 quote 反推 ask：
    /// `out = floor(10^(dj+2) / ask)`，在 `base±4` 候选区间内逐点校验（唯一解）。
    pub fn recover_ask(out: U256, dj: u32) -> Option<U256> {
        if out.is_zero() || dj > 30 {
            return None;
        }
        let scale = U256::from(10u64).pow(U256::from(dj + 2));
        let base = (scale / out).saturating_sub(U256::from(4));
        for delta in 0..9u64 {
            let cand = base + U256::from(delta);
            if cand.is_zero() {
                continue;
            }
            if scale / cand == out {
                return Some(cand);
            }
        }
        None
    }

    /// 用 `(0→j)` 大额 quote（amountIn=10^(d0+4)，q_big）联合小额 quote
    /// （amountIn=10^d0，q_small）精确锁定 ask：
    /// q_small = floor(10^(dj+2)/ask)，q_big = floor(10^(dj+6)/ask)。
    ///
    /// 引擎对每资产存在不可观测的 cap；若 q_big 被截断（q_big < q_small·10^4）
    /// 则返回 None，调用方回退到 `recover_ask`（高小数位资产本身无歧义）。
    fn pin_ask(q_small: U256, q_big: U256, dj: u32) -> Option<U256> {
        if q_small.is_zero() || q_big.is_zero() || dj > 30 {
            return None;
        }
        let p10 = |e: u32| U256::from(10u64).pow(U256::from(e));
        let s6 = p10(dj + 6);
        let lin_lo = q_small.checked_mul(p10(4))?;
        if q_big < lin_lo {
            // 大额 quote 已被引擎 cap 截断，无法用于锁定
            return None;
        }
        // ask = ceil(s6 / (q_big + 1))
        let ask = (s6 + q_big) / (q_big + U256::from(1));
        if ask.is_zero() {
            return None;
        }
        // 两个 quote 都必须被精确复现
        if s6 / ask != q_big || p10(dj + 2) / ask != q_small {
            return None;
        }
        Some(ask)
    }

    /// 与链上引擎 quote 完全一致的计算（含两段式截断与阶梯上限）：
    ///   - SELL（j==0）：`out = min(in, maxIn_i) · bid_i · 10^(d0-2) / 10^di`
    ///   - BUY（i==0）：线性 `linear = in · 10^(dj-d0+2) / ask_j`，再按
    ///     `buy_capped` 截断（饱和型 `min(linear, maxOut)` / 归零型
    ///     `linear > 金库余额 → 0` / 未知不截断）
    ///   - 跨资产：先按 maxIn_i 截断输入走 i→0，再按 buy_capped 规则走 0→j
    /// 价格未知或资产缺失时返回 `None`。
    /// 引擎报价核心：`i→j` 全方向（含 SELL 截断、BUY 阶梯上限/归零、跨资产两段式）
    pub fn engine_quote(&self, i: usize, j: usize, amount_in: U256) -> Option<U256> {
        if i == j || amount_in.is_zero() {
            return Some(U256::ZERO);
        }
        if i >= self.assets.len() || j >= self.assets.len() {
            return None;
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
        let k0 = d0.saturating_sub(2);
        if j == 0 {
            let bid = self.bid_price(i)?;
            // SELL 输入上限：in ≤ maxIn（阶梯容量；未知时不截断）
            let eff_in = match self.max_inputs.get(i).copied().flatten() {
                Some(m) => amount_in.min(m),
                None => amount_in,
            };
            let out = eff_in.checked_mul(bid)?.checked_mul(p10(k0))? / p10(di);
            Some(out)
        } else if i == 0 {
            let ask = self.ask_price(j)?;
            // BUY：linear = in * 10^(dj-d0+2) / ask；指数为负（dj < d0-2）时
            // 等价于 in / (ask * 10^(d0-dj-2))（与 derive_rate 的 dj<k0 分支一致）
            let linear = if dj + 2 >= d0 {
                amount_in.checked_mul(p10(dj + 2 - d0))? / ask
            } else {
                let den = ask.checked_mul(p10(d0 - dj - 2))?;
                amount_in / den
            };
            Some(self.buy_capped(j, linear))
        } else {
            let bid = self.bid_price(i)?;
            let ask = self.ask_price(j)?;
            // 两段式：先 i→USDT0 截断，再 USDT0→j 截断（与引擎一致）；
            // 输入侧受 maxIn_i 约束，输出侧受 maxOut_j 约束
            let eff_in = match self.max_inputs.get(i).copied().flatten() {
                Some(m) => amount_in.min(m),
                None => amount_in,
            };
            let value = eff_in.checked_mul(bid)?.checked_mul(p10(k0))? / p10(di);
            // 0→j 段：value * 10^(dj-d0+2) / ask；负指数分支同上
            let linear = if dj + 2 >= d0 {
                value.checked_mul(p10(dj + 2 - d0))? / ask
            } else {
                let den = ask.checked_mul(p10(d0 - dj - 2))?;
                value / den
            };
            Some(self.buy_capped(j, linear))
        }
    }

    /// BUY（0→j）输出应用引擎阶梯上限：
    ///   - 超阈值归零型（大额 probe = 0，阶梯容量 > 金库余额）：
    ///     linear ≤ 金库余额才返回，否则 0
    ///   - 饱和型（maxOut 已知）：`min(linear, maxOut)`
    ///   - 未知：不截断（保持线性）
    fn buy_capped(&self, j: usize, linear: U256) -> U256 {
        if self.buy_zero_over_vault.get(j).copied().unwrap_or(false) {
            let vault = self.reserves.get(j).copied().unwrap_or(U256::ZERO);
            return if linear <= vault { linear } else { U256::ZERO };
        }
        match self.max_outputs.get(j).copied().flatten() {
            Some(m) => linear.min(m),
            None => linear,
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
            self.max_outputs.get(k).copied().flatten().is_some()
                || self.buy_zero_over_vault.get(k).copied().unwrap_or(false)
        };
        if j == 0 {
            max_in(i)
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
        let out_ceiling = if self.buy_zero_over_vault.get(j).copied().unwrap_or(false) {
            Some(self.reserves.get(j).copied().unwrap_or(U256::ZERO))
        } else {
            self.max_outputs.get(j).copied().flatten()
        };
        if j == 0 {
            return match self.max_inputs.get(i).copied().flatten() {
                Some(m) => self.engine_quote(i, 0, m),
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
    /// bid/ask（含点差），再由 bid/ask 推导全部费率（不做 quote 锚定，避免
    /// 1-ulp 放大误差）；同时从大额 probe 恢复 BUY maxOut（饱和型直接取 q_big，
    /// 归零型标记 `buy_zero_over_vault`）与 SELL maxIn（100 整枚 probe 反推）。
    fn apply_snapshot(&mut self, snap: &Snapshot, snap_block: u64) -> usize {
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
        self.price_scales.resize(n, 10_000);
        self.buy_disabled.resize(n, false);
        self.buy_zero_over_vault.resize(n, false);
        self.max_outputs.resize(n, None);
        self.max_inputs.resize(n, None);
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

        // 余额以引擎 getAssetReserves()（= 金库余额）为准
        if snap.vaultReserves.len() == n {
            self.reserves = snap.vaultReserves.clone();
        } else if snap.poolBalances.len() == n {
            self.reserves = snap.poolBalances.clone();
        }

        // 1) 从同块 quote 恢复每资产 bid/ask：
        //    - `j→0`（amountIn=10^(dj-4)）无舍入：out = bid 精确值
        //    - `0→j`（amountIn=10^d0）：out = floor(10^(dj+2) / ask)，候选扫描
        //    - `0→j` 大额（amountIn=10^(d0+4)，pair >= n*n）：与小额联合锁定 ask
        let mut j0_out: Vec<Option<U256>> = vec![None; n];
        let mut zj_out: Vec<Option<U256>> = vec![None; n];
        let mut big_out: Vec<Option<U256>> = vec![None; n];
        let mut big_sell_out: Vec<Option<U256>> = vec![None; n];
        for (k, pair) in snap.quotePairs.iter().enumerate() {
            if k >= snap.quotes.len() || !snap.quotes[k].success {
                continue;
            }
            let p = pair.to::<usize>();
            let nn = n * n;
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
        for j in 1..n {
            let dj = self.assets[j].decimals as u32;
            // j→0（amountIn=10^(dj-4)）：out = bid 精确值
            let bid = j0_out[j].filter(|out| !out.is_zero());
            let zj = zj_out[j];
            // 0→j quote 成功返回 0：引擎禁用该资产买入（ask=0）
            let disabled = zj == Some(U256::ZERO);
            let ask = if disabled {
                None
            } else if let Some(qs) = zj {
                let base = Self::recover_ask(qs, dj);
                match big_out[j] {
                    Some(qb) => Self::pin_ask(qs, qb, dj).or(base),
                    None => base,
                }
            } else {
                None
            };
            // 保鲜判断（日志优先、快照补缺）：该资产 update 日志已给出 >= 快照块的
            // 价格时，快照 quote 不覆盖本地（防止旧块 quote 回退新日志价格）；
            // 容量/禁用状态仍以快照为准（见 1.5 节）
            let log_fresh = snap_block != 0
                && self.price_updated_block.get(j).copied().unwrap_or(0) >= snap_block;
            if disabled {
                self.buy_disabled[j] = true;
                if let Some(b) = bid {
                    if !log_fresh {
                        self.prices[j] = b;
                        self.spreads[j] = 0;
                        self.bid_offsets[j] = 0;
                        self.ask_offsets[j] = 0;
                    }
                } else if !log_fresh {
                    // 卖出同不可用（j→0 quote 为 0/缺失）：清掉残留 bid，
                    // 避免旧价格 + 无上限继续产生幻影卖出报价
                    self.prices[j] = U256::ZERO;
                    self.spreads[j] = 0;
                    self.bid_offsets[j] = 0;
                    self.ask_offsets[j] = 0;
                }
                continue;
            }
            match (bid, ask) {
                (Some(b), Some(a)) => {
                    self.buy_disabled[j] = false;
                    if !log_fresh && a > b && (a - b) <= U256::from(u64::MAX) {
                        let sh = (a - b).to::<u64>();
                        self.spreads[j] = sh;
                        // 参考价 = bid + spread/2（floor），bid/ask 偏移独立保存，
                        // 保证 `bid = price - bid_offset`、`ask = price + ask_offset` 恒成立
                        self.prices[j] = b + U256::from(sh / 2);
                        self.bid_offsets[j] = sh / 2;
                        self.ask_offsets[j] = sh - sh / 2;
                    }
                }
                (Some(b), None) => {
                    if !log_fresh {
                        // 仅 bid 可用（0→j 缺失/恢复失败）：默认点差
                        self.spreads[j] = BINARYFI_DEFAULT_SPREAD;
                        self.prices[j] = b + U256::from(BINARYFI_DEFAULT_SPREAD / 2);
                        self.bid_offsets[j] = BINARYFI_DEFAULT_SPREAD / 2;
                        self.ask_offsets[j] = BINARYFI_DEFAULT_SPREAD / 2;
                    }
                }
                (None, Some(a)) => {
                    if !log_fresh {
                        self.spreads[j] = BINARYFI_DEFAULT_SPREAD;
                        self.prices[j] =
                            a.saturating_sub(U256::from((BINARYFI_DEFAULT_SPREAD + 1) / 2));
                        self.bid_offsets[j] = (BINARYFI_DEFAULT_SPREAD + 1) / 2;
                        self.ask_offsets[j] = (BINARYFI_DEFAULT_SPREAD + 1) / 2;
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
                    } else if qb < qs.checked_mul(U256::from(10_000)).unwrap_or(U256::MAX) {
                        self.max_outputs[j] = Some(qb);
                        self.buy_zero_over_vault[j] = false;
                    } else {
                        self.max_outputs[j] = None;
                        self.buy_zero_over_vault[j] = false;
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
        vec![BINARYFI_SWAP_EVENT, BINARYFI_UPDATE_EVENT]
    }

    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError> {
        let topics = log.topics();

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
            // 虚拟子池只处理涉及自身暴露 pair 的 Swap（解析层已按此扇出，此处防御性校验）
            if let Some((a, b)) = self.exposed_pair {
                if a != i && b != i && a != j && b != j {
                    return Ok(SyncAction::None);
                }
            }
            self.anchor_rate(i, j, amount_in, amount_out);
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
                self.apply_l2_update(
                    asset_idx,
                    price,
                    block_number,
                    ask_offset_raw,
                    bid_offset_raw,
                );
                return Ok(SyncAction::None);
            }
            // canonical 路径无 raw bytes：标记 stale，交由 update() 批量刷新
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
        let mut big_quote_pairs = Vec::with_capacity(BINARYFI_ASSET_COUNT - 1);
        for j in 1..BINARYFI_ASSET_COUNT {
            big_quote_pairs.push(U256::from(BINARYFI_ASSET_COUNT * BINARYFI_ASSET_COUNT + j));
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
        let assets: Vec<Address> = self.assets.iter().map(|t| t.address).collect();
        // 固定到具体块号：快照 quote 与保鲜判断（日志价格 >= snap_block 不覆盖）同块一致，
        // 避免 BlockId::latest() 的隐式取数块与本地日志推进产生竞态
        let snap_block: u64 = match block {
            BlockId::Number(alloy::eips::BlockNumberOrTag::Number(num)) => num,
            _ => provider.get_block_number().await?,
        };
        let block = BlockId::Number(alloy::eips::BlockNumberOrTag::Number(snap_block));
        let quote_pairs: Vec<U256> = self.stale_pairs.iter().map(|&p| U256::from(p)).collect();
        // stale 资产涉及的 (0→j) 方向附带大额报价，锁定 ask + BUY maxOut
        let mut big_quote_pairs: Vec<U256> = Vec::new();
        // stale 资产涉及的 (j→0) 100 整枚报价，恢复 SELL maxIn
        let mut big_sell_pairs: Vec<U256> = Vec::new();
        for &p in &self.stale_pairs {
            let (i, j) = self.pair_indices(p);
            if i == 0 && j != 0 {
                big_quote_pairs.push(U256::from(n * n + j));
            }
            if i != 0 && j == 0 {
                big_sell_pairs.push(U256::from(2 * n * n + i));
            }
        }
        let snap = Self::fetch_snapshot(
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
        .await?;
        self.apply_snapshot(&snap, snap_block);
        if !snap.quotePairs.is_empty() {
            let refreshed: Vec<usize> = snap.quotePairs.iter().map(|p| p.to::<usize>()).collect();
            self.clear_stale_pairs(&refreshed);
        }
        Ok(())
    }

}

impl BinaryFiPropPool {
    /// 输出截断兜底：out ≤ 96% * 金库余额(tokenOut)；余额未知时不截断。
    ///
    /// 仅当该方向的阶梯上限（maxIn/maxOut）未知时启用（见 `ladder_cap_known`），
    /// 保守近似防止超大额输入的虚假报价；已知精确上限时不叠加，避免低估。
    fn capped_out(&self, j: usize, out: U256) -> U256 {
        match self.reserves.get(j).copied().filter(|r| !r.is_zero()) {
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
            chain_id: BINARYFI_CHAIN_ID,
            created_block: 0,
            last_synced_block: 0,
            assets: Vec::new(),
            prices: Vec::new(),
            spreads: Vec::new(),
            bid_offsets: Vec::new(),
            ask_offsets: Vec::new(),
            price_scales: Vec::new(),
            buy_disabled: Vec::new(),
            buy_zero_over_vault: Vec::new(),
            max_outputs: Vec::new(),
            max_inputs: Vec::new(),
            reserves: Vec::new(),
            rates: Vec::new(),
            stale_pairs: Vec::new(),
            price_updated_block: Vec::new(),
            price0_calibrated: false,
        }
    }
}

// ============================================================================
// Flashblocks 原始交易增强
// ============================================================================

/// 从 flashblocks 的原始交易字节中定位引擎 `update` 交易并解析 calldata，
/// 返回注入 7 个 word（price / blockNumber / data0..2 / askOffsetRaw /
/// bidOffsetRaw）后的日志 data。其中：
///   - `data0` = sellLadder、`data1` = buyLadder（左对齐 256 位字段），前 16 位为
///     点差偏移字段；`askOffsetRaw = (data1 >> 240) / 16`、
///     `bidOffsetRaw = (data0 >> 240) / 16`（ladder 空间单位，scale 由池子应用）
///
/// 找不到对应 raw bytes、RLP 解码失败或目标/选择器不匹配时返回 `None`，
/// 调用方应保留原始日志原样。
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
    if input.len() < BINARYFI_UPDATE_CALLDATA_LEN {
        return None;
    }
    if input[..4] != BINARYFI_UPDATE_SELECTOR {
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

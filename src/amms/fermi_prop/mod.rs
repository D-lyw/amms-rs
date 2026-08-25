//! # Fermi propAMM（Ethereum 主网）
//!
//! 架构、数据流、同步与模拟逻辑详见 `docs/fermi_prop_internal.md`（长期维护必读）。
//!
//! Fermi 是 Ethereum 主网 PropAMM 中交易量最大的 venue：链下做市引擎经 Titan
//! 私有流高频更新 registry lane 报价（链上看不到），链上只保留"有吃单时才落块"
//! 的稀疏快照。本地实时同步采用**双数据源**：
//!
//! - Titan 流（`state_space::titan_stream`）：lane 报价 + 余额 override 实时快照，
//!   最新者胜（slot 单调守卫）；
//! - 链上事件（newHeads → logs）：pair 生命周期（PairRegistered/Unregistered/ActiveSet）、
//!   wrapper Swapped 成交对账、vault ERC20 Transfer 余额对账、registry 上链校准。
//!
//! 池实例粒度为 **per-pair**：一个 `FermiPropPool` = 一个有序 (tokenA, tokenB)，
//! `virtual_address` 为 StateSpace key；同一 engine 部署下多实例共享全局状态
//! （lane 报价、PairParams、vault 余额）。

use alloy::{
    eips::{BlockId, BlockNumberOrTag},
    network::Network,
    primitives::{Address, B256, U256},
    providers::Provider,
    rpc::types::eth::Log,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::instrument;

use crate::amms::{
    amm::{AutomatedMarketMaker, SyncAction, AMM},
    error::AMMError,
};

pub mod factory;
pub mod titan;
pub mod types;

pub use types::{
    fermi_engine_last_trade_slot, fermi_lane_index, fermi_max_output_slot,
    fermi_registry_lane_slot, fermi_virtual_address, sorted_tokens, FermiCurveSegment,
    FermiLane, FermiPairParams, IFermiERC20, IFermiEngine, IFermiRegistry, ERC20_TRANSFER_EVENT,
    FERMI_CHAIN_ID, FERMI_ENGINE_ADDRESS, FERMI_PAIR_ACTIVE_SET_EVENT, FERMI_PAIR_REGISTERED_EVENT,
    FERMI_PAIR_UNREGISTERED_EVENT, FERMI_REGISTRY_ADDRESS, FERMI_SWAPPED_EVENT,
    FERMI_SWAPPER_ADDRESS, FERMI_VAULT_ADDRESS, FERMI_WRAPPER_ADDRESS,
};

// ============================================================================
// 常量
// ============================================================================

/// 默认 swap gas 估计。
pub const DEFAULT_SWAP_GAS: u64 = 400_000;
/// lane 价格 E8 定点缩放基数。
pub const FERMI_PRICE_E8_SCALE: u128 = 100_000_000;

// ============================================================================
// FermiPropPool
// ============================================================================

/// Fermi propAMM 池子（per-pair 实例）。
///
/// 对外以"具体 token pair"呈现：StateSpace 中一个实例 = 一个可交易对，
/// `virtual_address` 为 StateSpace key（与 Caliber/BinaryFi 虚拟子池同款）。
/// 实例内部保存该 pair 的 lane 报价、曲线参数与共享金库余额。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FermiPropPool {
    /// engine 合约地址（quote/swap 核心，Pair 生命周期事件源）
    pub engine_address: Address,
    /// swapper 合约地址（执行层）
    pub swapper_address: Address,
    /// IPropAMM wrapper 地址（标准 quote/swap 入口，Swapped 事件源）
    pub wrapper_address: Address,
    /// registry（PrioUpdateRegistry）地址（lane 存储）
    pub registry_address: Address,
    /// trader vault 地址（全部 pair 共享流动性金库）
    pub vault_address: Address,
    /// 报价方向基准资产 token A（baseAsset；engine 报价方向，价格高的资产在前）
    pub token_a: Address,
    /// 报价方向计价资产 token B（quoteAsset；1 token_a = lane.fair_price_e8/1e8 token_b）
    pub token_b: Address,
    /// token A 精度（decimals）
    pub decimals_a: u8,
    /// token B 精度
    pub decimals_b: u8,
    /// StateSpace key（虚拟子池地址）
    pub virtual_address: Address,
    /// registry lane 索引：`keccak256(abi.encode(tokenA, tokenB))`
    pub lane_index: B256,
    /// pair 曲线参数（engine `getPairParams`）
    pub pair_params: FermiPairParams,
    /// lane 报价状态（fair_price_e8 / update_timestamp / flag）
    pub lane: FermiLane,
    /// pair 是否活跃（engine isActive / PairActiveSet 事件）
    pub active: bool,
    /// 共享金库余额（token 地址 → 余额；全部 pair 共用同一金库）
    pub vault_balances: HashMap<Address, U256>,
    /// 引擎全局输出上限 `maxOutput[token_a]`（storage slot `keccak256(abi.encode(token_a, 8))`）。
    /// IL 检查：`vault(token_a) + amountIn > max_output` → revert `IL`；0 = 未初始化/跳过。
    pub max_output: U256,
    /// engine 全局"最后成交"记录（sub_key=0，正向路径；M4.5 trace 实证，2026-08-25）。
    /// 布局 `(last_trade_x << 64) | last_trade_block`；当 `last_trade_block ==
    /// last_synced_block`（同块成交）时，正向 `engine_quote` 把 `last_trade_x`
    /// 加进 div1。
    pub last_trade_word: U256,
    /// engine 全局"最后成交"记录（sub_key=1，反向路径；M4.5 trace 实证，2026-08-25）。
    /// 布局同上；当 `last_trade_block == last_synced_block`（同块成交）时，
    /// 反向 `engine_quote` 用 `a_norm = (A + last_trade_x)*1e18/D` 选档，
    /// 但 `a_eff` 保持原始 `amount_in`（只影响分档、不影响金额）。
    pub last_trade_rev_word: U256,
    /// 链 ID
    pub chain_id: u64,
    /// 创建区块号（StateSpace 扫描起点）
    pub created_block: u64,
    /// 最后同步区块号
    pub last_synced_block: u64,
}

impl FermiPropPool {
    /// 批量初始化（供 Variant::init_batch 调用）。
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

    /// 部署分组 key（同 engine/wrapper/registry/vault = 同一部署，共享链上状态）。
    pub fn deployment_key(&self) -> (Address, Address, Address, Address) {
        (
            self.engine_address,
            self.wrapper_address,
            self.registry_address,
            self.vault_address,
        )
    }

    /// token 是否属于本 pair。
    pub fn is_token(&self, token: Address) -> bool {
        token == self.token_a || token == self.token_b
    }

    /// 返回 token 精度（未知 token 返回 0）。
    pub fn token_decimals(&self, token: Address) -> u8 {
        if token == self.token_a {
            self.decimals_a
        } else if token == self.token_b {
            self.decimals_b
        } else {
            0
        }
    }

    /// 应用 Titan 流快照中的 lane 更新（M4 实时同步入口）。
    ///
    /// 版本守卫：update_timestamp 不回卷（Titan 最新者胜，slot 守卫在上游完成，
    /// 这里兜底）。调用方（M4 的 Titan 流消费者）已按 venue 过滤。
    pub fn apply_titan_lane(&mut self, lane: FermiLane) -> bool {
        if lane.update_timestamp == 0 {
            return false;
        }
        if lane.update_timestamp < self.lane.update_timestamp {
            return false;
        }
        // 同值不触发下游（同批重复消息/轮询幂等）。
        if lane == self.lane {
            return false;
        }
        self.lane = lane;
        true
    }

    /// 应用 vault 余额 override（防御性；M4 实测 Titan 快照不含 ERC20 余额，
    /// `stateOverride.balance` 为原生 ETH 余额。权威 ERC20 账本 = init balanceOf
    /// + 链上事件对账 + reconcile `eth_getStorageAt` 校准，见 `docs/fermi_prop_internal.md` §6.4）。
    pub fn apply_vault_balances(&mut self, balances: &HashMap<Address, U256>) {
        for (token, balance) in balances {
            if self.is_token(*token) || balance.is_zero() {
                self.vault_balances.insert(*token, *balance);
            }
        }
    }

    /// 应用 engine 全局 last-trade 槽原始值（init / 对账 / 漂移测试共用入口）。
    ///
    /// 由调用方从 `eth_getStorageAt(engine, FERMI_ENGINE_LAST_TRADE_SLOT, block)`
    /// 读取；本地 `engine_quote` 仅在 `last_trade_block == last_synced_block`
    /// 时使用其 `last_trade_x`（同块成交校正）。
    pub fn apply_last_trade_word(&mut self, word: U256) {
        self.last_trade_word = word;
    }

    /// 应用 engine 全局 last-trade 槽原始值（sub_key=1，反向路径；init / 对账 / 漂移测试共用）。
    pub fn apply_last_trade_rev_word(&mut self, word: U256) {
        self.last_trade_rev_word = word;
    }

    /// 输出截断兜底：out ≤ min(金库余额(tokenOut), pair 容量上限)。
    /// 余额未知时不截断。
    fn capped_out(&self, token_out: Address, out: U256) -> U256 {
        match self
            .vault_balances
            .get(&token_out)
            .copied()
            .filter(|b| !b.is_zero())
        {
            Some(balance) => out.min(balance),
            None => out,
        }
    }
}

impl Default for FermiPropPool {
    fn default() -> Self {
        Self {
            engine_address: FERMI_ENGINE_ADDRESS,
            swapper_address: FERMI_SWAPPER_ADDRESS,
            wrapper_address: FERMI_WRAPPER_ADDRESS,
            registry_address: FERMI_REGISTRY_ADDRESS,
            vault_address: FERMI_VAULT_ADDRESS,
            token_a: Address::ZERO,
            token_b: Address::ZERO,
            decimals_a: 18,
            decimals_b: 18,
            virtual_address: Address::ZERO,
            lane_index: B256::ZERO,
            pair_params: FermiPairParams::default(),
            lane: FermiLane::default(),
            active: false,
            vault_balances: HashMap::new(),
            max_output: U256::ZERO,
            last_trade_word: U256::ZERO,
            last_trade_rev_word: U256::ZERO,
            chain_id: FERMI_CHAIN_ID,
            created_block: 0,
            last_synced_block: 0,
        }
    }
}

// ============================================================================
// 链上 quote 复刻（M3.1：trace/生产 eth_call 级逆向，与 engine 字节码逐位对齐）
// ============================================================================

impl FermiPropPool {
    /// 本地 quote 复刻（token_in → token_out，exact-in），与 engine 字节码逐位对齐。
    ///
    /// 数学（trace 级验证 @block 0x18a0d7b，lane=246288406772，WETH/USDC；M3.1 修正版）：
    ///
    /// 前置（vault 余额 → 失衡度 → 基准价）：
    /// ```text
    /// scale  = 1e(8 + dec_diff)                    # WETH/USDC = 1e20
    /// L1     = vault(token_a)
    /// L2     = vault(token_b) * lane / scale        # 折合 token_a 计价
    /// S      = L1 + L2
    /// M      = (L2 - S*b/1e4) * 1e18 / S             # b = pair_params.b（M4.6 实证；WETH 系 5000 ≡ S/2）
    /// delta_c2 = c2 曲线对 M 的插值（段：y < M <= x，p1 = (M-y)*1e18/(x-y)，
    ///            delta = c + p1*d/1e18；M>0 时 = -2M，M<0 时 = -0.4M，M 超 ±1e18 无法报价）
    /// P0     = lane * (1e22 + delta_c2) / 1e22      # ★ 非 lane*K/1e22（旧 K 常量已废弃）
    /// ```
    ///
    /// 正向（token_in == token_a，WETH→USDC 例）：
    /// ```text
    /// div1   = A * lane / scale
    /// a_norm = div1 * 1e18 / D                      # D = pair_params.c（WETH 系 = 3e12）
    /// 找段 i：c1[i].y < a_norm <= c1[i].x
    /// p1     = (a_norm - y) * 1e18 / (x - y)
    /// delta2 = c + p1 * d / 1e18 + a                # a = pair_params.a（M4.6 实证；非 c2[0].c）
    /// price1 = P0 * (1e22 - delta2) / 1e22
    /// out    = A * price1 / scale
    /// ```
    /// 反向（token_in == token_b，USDC→WETH 例；A 为 quote 原生单位）：
    /// ```text
    /// a_norm = A * 1e18 / D
    /// a_norm > c1 末段.x → 封顶：a_norm = 末段.x，A_eff = a_norm * D / 1e18（out 持平）
    /// price1 = P0 * (1e22 + delta2) / 1e22
    /// out    = A_eff * scale / price1
    /// ```
    /// 边界（trace/eth_call 实证）：
    /// - COR：`a_norm <= c1[0].y` → revert `COR`（正向 @ A < ~4.06e12；反向 @ A <= 10000）；
    /// - IL：仅正向路径，`max_output[token_a] > 0 && vault(token_a) + A > max_output` → revert `IL`
    ///   （maxOutput 为引擎槽位 8 的 mapping，按 base 资产索引；WETH = 1.8e21，
    ///   故正向 IL 边界 = 1.8e21 - vault(WETH) = 253005317793142188091 @block 0x18a0d7b）；
    /// - 输出截断：`out = min(out, vault(token_out))`（反向大额精确等于 vault 余额）。
    ///
    /// 返回 `None` 表示链上会 revert（COR/IL）或本地无法精确模拟（防御性：缺 vault 余额/
    /// c2 曲线、M 超 ±1e18、delta2 为负等）。
    pub fn engine_quote(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Option<U256> {
        if !self.is_token(token_in) || !self.is_token(token_out) || token_in == token_out {
            return None;
        }
        if amount_in.is_zero() {
            return Some(U256::ZERO);
        }
        if !self.active
            || self.lane.fair_price_e8 == 0
            || self.pair_params.c1.is_empty()
            || self.pair_params.c2.is_empty()
        {
            return None;
        }
        let d = U256::from(self.pair_params.c);
        if d.is_zero() {
            return None;
        }
        let lane = U256::from(self.lane.fair_price_e8);
        // 全部 Fermi 对满足 dec_a >= dec_b（WETH 18/USDC 6、WBTC 8/USDC 6、6/6、8/8）。
        let dec_diff = self.decimals_a.saturating_sub(self.decimals_b) as u32;
        let scale = U256::from(10u128.pow(8 + dec_diff)); // 1e(8+dec_diff)
        let one_e18 = U256::from(1_000_000_000_000_000_000u128);
        let one_e22 = U256::from(10_000_000_000_000_000_000_000u128);
        let forward = token_in == self.token_a;
        let (c1, c2) = (&self.pair_params.c1, &self.pair_params.c2);

        // IL：仅正向路径执行（trace 实证：反向不经过 IL 检查，超大金额恒封顶）。
        if forward && !self.max_output.is_zero() {
            if let Some(vault_in) = self.vault_balances.get(&token_in).copied() {
                if vault_in
                    .checked_add(amount_in)
                    .map_or(true, |s| s > self.max_output)
                {
                    return None;
                }
            }
        }

        // P0 = lane * (1e22 + delta_c2) / 1e22；delta_c2 来自 vault 失衡度 M 经 c2 曲线插值。
        // ★ M 以 quote 资产（token_b）为单位（trace 实证）：L1 = vault(token_b)，
        //   L2 = vault(token_a) * lane / scale（base 资产折合 quote 计价）。
        let l1 = self.vault_balances.get(&self.token_b).copied()?;
        let l2_raw = self.vault_balances.get(&self.token_a).copied()?;
        let l2 = l2_raw.checked_mul(lane)? / scale;
        let s = l1.checked_add(l2)?;
        // M = (L2 - S*b/1e4) * 1e18 / S（M4.6 trace 实证 @25827361 WBTC/USDC：
        //   引擎用 `S * pair_params.b / 10000` 作失衡基准，非 S/2。
        //   WETH 系 b=5000 → S*b/1e4 ≡ S/2，与旧公式一致；WBTC 系 b=3333 必须用 b）。
        let m = imbalance_m(l1, l2, s, self.pair_params.b as u64, one_e18)?;
        let delta_c2 = c2_delta(m, c2)?;
        let p0_num = 10_000_000_000_000_000_000_000i128 + delta_c2; // 1e22 + delta_c2 恒 > 0
        let p0 = lane.checked_mul(U256::from(p0_num as u128))? / one_e22;

        // a_norm 归一化；反向超曲线上限时封顶（out 持平，trace 实证）。
        let last = c1.last()?;
        let last_x = U256::from(last.x as u128);
        let (a_eff, a_norm) = if forward {
            // 同块成交校正（M4.5 trace 实证）：engine 全局 last-trade 槽
            // `(last_trade_x << 64) | last_trade_block`，当 `last_trade_block ==
            // 当前同步块` 时把 `last_trade_x` 加进 div1（仅在正向路径，反向无）。
            let mut div1 = amount_in.checked_mul(lane)? / scale;
            if !self.last_trade_word.is_zero() {
                // low 32 位 = 成交区块号（布局实证，bits 32-63 恒 0）
                let trade_block = (self.last_trade_word & U256::from(0xffff_ffffu64)).to::<u64>();
                if trade_block == self.last_synced_block {
                    let x = self.last_trade_word >> U256::from(64);
                    div1 = div1.checked_add(x)?;
                }
            }
            (amount_in, div1.checked_mul(one_e18)? / d)
        } else {
            // 反向：无同块成交时 `a_norm = A*1e18/D`，超曲线上限（> last_x）封顶
            // （a_eff = last_x*D/1e18，out 持平，eth_call 实证）。
            let a_norm_raw = amount_in.checked_mul(one_e18)? / d;
            if a_norm_raw > last_x {
                (last_x.checked_mul(d)? / one_e18, last_x)
            } else {
                // 同块成交校正（M4.5 trace 实证 @25828239）：`a_norm = (A + X')*1e18/D`
                // 重新选档；`a_eff` 保持原始 `amount_in`（校正只影响分档、不影响金额）。
                // 校正后 a_norm 若超过 last_x（如 A=1e12 同块成交 @25828950），
                // 引擎无段可匹配 → revert COR（本地返回 None，与链上 COR 对齐）。
                let mut a_norm = a_norm_raw;
                if !self.last_trade_rev_word.is_zero() {
                    let trade_block =
                        (self.last_trade_rev_word & U256::from(0xffff_ffffu64)).to::<u64>();
                    if trade_block == self.last_synced_block {
                        let x = self.last_trade_rev_word >> U256::from(64);
                        let a_base = amount_in.checked_add(x)?;
                        a_norm = a_base.checked_mul(one_e18)? / d;
                    }
                }
                (amount_in, a_norm)
            }
        };

        // COR：a_norm <= c1[0].y（下界）→ revert COR。
        if a_norm <= U256::from(c1[0].y as u128) {
            return None;
        }

        // 找档 i：y < a_norm <= x（x 为上界、y 为下界，M3.1 修正的 ABI 顺序）。
        let mut seg = None;
        for s in c1 {
            if a_norm > U256::from(s.y as u128) && a_norm <= U256::from(s.x as u128) {
                seg = Some(s);
                break;
            }
        }
        let seg = seg?;
        let x1 = U256::from(seg.y as u128);
        let x2 = U256::from(seg.x as u128);

        let p1 = (a_norm - x1).checked_mul(one_e18)? / (x2 - x1); // <= 1e18
        let p1i = i128::try_from(p1.to::<u128>()).ok()?;
        let one_e18i: i128 = 1_000_000_000_000_000_000;
        // delta2 = c + p1*d/1e18 + a（a = pair_params.a，M4.6 trace 实证 @25827458
        // cbBTC/USDC：c2[0].c=0 时链上仍加 2e17，来自打包参数槽的 a 字段；
        // WETH/WBTC 系 a 与 c2[0].c 恰同为 2e17，故早期分析误判为 c2[0].c）。
        let delta2i = seg.c + p1i.checked_mul(seg.d)? / one_e18i + self.pair_params.a as i128;
        if delta2i < 0 {
            return None; // Fermi 曲线恒正；防御性
        }
        let delta2 = U256::from(delta2i as u128);

        let price1 = if forward {
            p0.checked_mul(one_e22 - delta2)? / one_e22
        } else {
            p0.checked_mul(one_e22 + delta2)? / one_e22
        };
        let out = if forward {
            a_eff.checked_mul(price1)? / scale
        } else {
            a_eff.checked_mul(scale)? / price1
        };
        Some(self.capped_out(token_out, out))
    }
}

/// 失衡度 `M = (L2 - S*b/1e4) * 1e18 / S`（int256 截断向零，trace 实证：`0x5953 SDIV`；
/// 失衡基准 = `S * pair_params.b / 10000`，M4.6 修正，非 S/2）。
/// `|M| <= 5e17` 恒成立（`0 <= L2 <= S`），返回 i128；超出 c2 曲线 x 范围（±1e18）
/// 由调用方经 `c2_delta` 判 None。
fn imbalance_m(l1: U256, l2: U256, s: U256, b: u64, one_e18: U256) -> Option<i128> {
    // 失衡基准 = S * b / 10000（b = pair_params.b；WETH 系 5000 = S/2，WBTC 系 3333）。
    let half = s.checked_mul(U256::from(b))? / U256::from(10_000u64);
    let (abs_m, negative) = if l2 >= half {
        ((l2 - half).checked_mul(one_e18)? / s, false)
    } else {
        ((half - l2).checked_mul(one_e18)? / s, true)
    };
    let abs_m = u128::try_from(abs_m).ok()?;
    Some(if negative {
        -(abs_m as i128)
    } else {
        abs_m as i128
    })
}

/// c2 曲线插值：段 `y < m <= x`，`p1 = (m - y) * 1e18 / (x - y)`，
/// `delta = c + p1 * d / 1e18`（WETH/USDC：M>0 → -2M；M<0 → -0.4M；恒在 ±1e18 内）。
/// M 不在任何段（|M| > 1e18）时返回 None（引擎行为未知，防御性不报价）。
fn c2_delta(m: i128, c2: &[FermiCurveSegment]) -> Option<i128> {
    let one_e18: i128 = 1_000_000_000_000_000_000;
    for s in c2 {
        if s.y < m && m <= s.x {
            let p1 = (m - s.y).checked_mul(one_e18)? / (s.x - s.y);
            return Some(s.c + p1.checked_mul(s.d)? / one_e18);
        }
    }
    None
}

// ============================================================================
// AutomatedMarketMaker
// ============================================================================

impl AutomatedMarketMaker for FermiPropPool {
    fn address(&self) -> Address {
        self.virtual_address
    }

    fn supported_chains(&self) -> Option<Vec<u64>> {
        Some(vec![FERMI_CHAIN_ID])
    }

    fn last_synced_block(&self) -> u64 {
        self.last_synced_block
    }

    fn set_last_synced_block(&mut self, block_number: u64) {
        self.last_synced_block = block_number;
    }

    fn sync_events(&self) -> Vec<B256> {
        vec![
            FERMI_PAIR_ACTIVE_SET_EVENT,
            FERMI_SWAPPED_EVENT,
            ERC20_TRANSFER_EVENT,
        ]
    }

    fn sync(&mut self, log: &Log) -> Result<SyncAction, AMMError> {
        let topics = log.topics();

        // L0: engine PairActiveSet → pair 启停
        if log.address() == self.engine_address
            && topics.len() == 3
            && topics[0] == FERMI_PAIR_ACTIVE_SET_EVENT
        {
            let base = Address::from_word(topics[1]);
            let quote = Address::from_word(topics[2]);
            if (base == self.token_a && quote == self.token_b)
                || (base == self.token_b && quote == self.token_a)
            {
                let data = log.data().data.as_ref();
                let active = data.len() >= 32 && U256::from_be_slice(&data[..32]) == U256::from(1);
                self.active = active;
                tracing::debug!(
                    pair = %self.virtual_address,
                    active,
                    block = log.block_number,
                    "fermi pair active set"
                );
            }
            return Ok(SyncAction::None);
        }

        // L1: wrapper Swapped → 成交信号（余额以 ERC20 Transfer 为权威账本）。
        // 2026-08-24 漂移测试实证：同一笔成交会同时 emit wrapper Swapped 与
        // token 合约 Transfer（tokenIn→vault / vault→tokenOut），若两者都增减
        // vault 余额会造成重复记账（余额随事件数线性漂移）。故此处不再变更
        // vault_balances，仅作为本 pair 成交信号返回；权威余额由 L2 的
        // Transfer 账本维护（含跨 pair 成交与其它 vault 收付）。
        if log.address() == self.wrapper_address
            && topics.len() == 4
            && topics[0] == FERMI_SWAPPED_EVENT
        {
            return Ok(SyncAction::None);
        }

        // L2: ERC20 Transfer 涉及 vault → 余额对账
        if topics.len() == 3 && topics[0] == ERC20_TRANSFER_EVENT {
            let from = Address::from_word(topics[1]);
            let to = Address::from_word(topics[2]);
            if from == self.vault_address || to == self.vault_address {
                let data = log.data().data.as_ref();
                if data.len() < 32 {
                    return Ok(SyncAction::Resync);
                }
                let amount = U256::from_be_slice(&data[..32]);
                let token = log.address();
                if from == self.vault_address {
                    let bal = self.vault_balances.get(&token).copied().unwrap_or_default();
                    self.vault_balances
                        .insert(token, bal.saturating_sub(amount));
                } else {
                    let bal = self.vault_balances.get(&token).copied().unwrap_or_default();
                    self.vault_balances
                        .insert(token, bal.saturating_add(amount));
                }
            }
            return Ok(SyncAction::None);
        }

        Ok(SyncAction::None)
    }

    fn tokens(&self) -> Vec<Address> {
        vec![self.token_a, self.token_b]
    }

    fn decimals(&self, token: Address) -> u8 {
        self.token_decimals(token)
    }

    fn calculate_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
        if !self.is_token(base_token) || !self.is_token(quote_token) {
            return Err(AMMError::TokenNotFound(base_token));
        }
        if base_token == quote_token {
            return Ok(1.0);
        }
        let price_e8 = self.lane.fair_price_e8 as f64 / FERMI_PRICE_E8_SCALE as f64;
        let (p, dec_base, dec_quote) = if base_token == self.token_a {
            (price_e8, self.decimals_a, self.decimals_b)
        } else {
            (1.0 / price_e8, self.decimals_b, self.decimals_a)
        };
        if !p.is_finite() || p <= 0.0 {
            return Ok(0.0);
        }
        let scale = 10f64.powi(dec_base as i32 - dec_quote as i32);
        Ok(p * scale)
    }

    fn spot_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
        self.calculate_price(base_token, quote_token)
    }

    fn has_sufficient_liquidity(&self) -> bool {
        self.active && self.lane.fair_price_e8 != 0
    }

    fn simulate_swap(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        self.engine_quote(token_in, token_out, amount_in)
            .ok_or_else(|| AMMError::Msg("fermi: no quote for pair".to_string()))
    }

    fn simulate_swap_mut(
        &mut self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let out = self.simulate_swap(token_in, token_out, amount_in)?;
        if !out.is_zero() {
            self.vault_balances
                .entry(token_in)
                .and_modify(|b| *b = b.saturating_add(amount_in))
                .or_insert(amount_in);
            let bal = self
                .vault_balances
                .get(&token_out)
                .copied()
                .unwrap_or_default();
            self.vault_balances
                .insert(token_out, bal.saturating_sub(out));
        }
        Ok(out)
    }

    #[instrument(skip_all, fields(pool = %self.virtual_address))]
    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let block = match block_number {
            BlockId::Number(n) => BlockId::Number(n),
            _ => BlockId::latest(),
        };
        let engine = IFermiEngine::new(self.engine_address, provider.clone());

        // 1. 报价方向探测 + 曲线参数。
        // getPairParams 方向敏感（2026-08-23 实测：getPairParams(WETH, USDC)
        // 正常返回，getPairParams(USDC, WETH) 返回空），而 engine getPairs 返回
        // 的是 IPropAMM 标准地址排序（token0 < token1），与报价方向可能相反。
        // 探测失败则交换 token_a/token_b 并重算 virtual_address/lane_index。
        let mut swapped = false;
        let params = match engine
            .getPairParams(self.token_a, self.token_b)
            .block(block)
            .call()
            .await
        {
            Ok(p) => p,
            Err(_) => {
                tracing::debug!(
                    target: "amms::fermi_prop",
                    token_a = %self.token_a,
                    token_b = %self.token_b,
                    "fermi: getPairParams direction mismatch, swapping"
                );
                std::mem::swap(&mut self.token_a, &mut self.token_b);
                std::mem::swap(&mut self.decimals_a, &mut self.decimals_b);
                self.virtual_address =
                    fermi_virtual_address(self.engine_address, self.token_a, self.token_b);
                self.lane_index = fermi_lane_index(self.token_a, self.token_b);
                swapped = true;
                engine
                    .getPairParams(self.token_a, self.token_b)
                    .block(block)
                    .call()
                    .await
                    .map_err(|e| AMMError::Msg(format!("fermi: getPairParams failed: {e}")))?
            }
        };
        self.pair_params = FermiPairParams::from_sol(params);
        let _ = swapped;

        // 2. pair 活跃状态
        match engine
            .isActive(self.token_a, self.token_b)
            .block(block)
            .call()
            .await
        {
            Ok(active) => self.active = active,
            Err(e) => {
                tracing::warn!(target: "amms::fermi_prop", pool = %self.virtual_address, error = %e, "fermi: isActive failed");
                self.active = false;
            }
        }

        // 3. lane 状态。
        // 首选：registry 存储槽直读（M3.1 已破解映射，2026-08-24 8/8 pair 验证）：
        //   slot = keccak256(abi.encode(engine, laneIndex))，对所有 pair 有效，
        //   无 getState 的新鲜度/调用方限制；Titan 流接管后由 M4 实时覆盖。
        let registry = IFermiRegistry::new(self.registry_address, provider.clone());
        let lane_slot = U256::from_be_bytes(
            fermi_registry_lane_slot(self.engine_address, self.token_a, self.token_b).0,
        );
        match provider
            .get_storage_at(self.registry_address, lane_slot)
            .block_id(block)
            .await
        {
            Ok(word) => {
                if let Some(lane) = FermiLane::from_slot_word(word) {
                    self.lane = lane;
                }
            }
            Err(_) => {
                // 兜底：registry getState（受调用方与新鲜度限制，可能失败）。
                if let Ok(r) = registry
                    .getState(self.lane_index.into(), 0, u32::MAX)
                    .block(block)
                    .call()
                    .await
                {
                    self.lane = FermiLane {
                        update_timestamp: r.updateTimestamp,
                        flag: r.flag,
                        fair_price_e8: r.fairPriceE8.to::<u64>(),
                    };
                }
            }
        }

        // 4. decimals + vault 余额（本 pair 涉及的两个 token）
        for token in [self.token_a, self.token_b] {
            let erc20 = IFermiERC20::new(token, provider.clone());
            if let Ok(dec) = erc20.decimals().block(block).call().await {
                if token == self.token_a {
                    self.decimals_a = dec;
                } else {
                    self.decimals_b = dec;
                }
            }
            match erc20
                .balanceOf(self.vault_address)
                .block(block)
                .call()
                .await
            {
                Ok(balance) => {
                    self.vault_balances.insert(token, balance);
                }
                Err(e) => {
                    tracing::warn!(target: "amms::fermi_prop", token = %token, error = %e, "fermi: balanceOf failed");
                }
            }
        }

        // 5. 引擎全局输出上限 maxOutput[token_a]（IL 检查用；槽位 8 的 mapping）。
        //    读取失败不阻断 init（max_output = 0 时本地跳过 IL 检查，与引擎
        //    `maxOutput == 0 → 跳过` 的分支一致）。
        let max_out_slot = U256::from_be_bytes(fermi_max_output_slot(self.token_a).0);
        if let Ok(max_out) = provider
            .get_storage_at(self.engine_address, max_out_slot)
            .block_id(block)
            .await
        {
            self.max_output = max_out;
        } else {
            tracing::warn!(
                target: "amms::fermi_prop",
                pool = %self.virtual_address,
                "fermi: max_output storage read failed, IL check disabled"
            );
        }

        // 6. engine 全局 last-trade 槽（同块成交校正用，见 engine_quote 注释）。
        //    正向读 sub_key=0、反向读 sub_key=1；读取失败不阻断 init
        //    （word = 0 时校正自然跳过）。
        let last_trade_slot = U256::from_be_bytes(
            fermi_engine_last_trade_slot(self.token_a, self.token_b, 0).0,
        );
        if let Ok(word) = provider
            .get_storage_at(self.engine_address, last_trade_slot)
            .block_id(block)
            .await
        {
            self.last_trade_word = word;
        }
        let last_trade_rev_slot = U256::from_be_bytes(
            fermi_engine_last_trade_slot(self.token_a, self.token_b, 1).0,
        );
        if let Ok(word) = provider
            .get_storage_at(self.engine_address, last_trade_rev_slot)
            .block_id(block)
            .await
        {
            self.last_trade_rev_word = word;
        }

        // 记录 init 区块（供同块成交校正判断）。
        if let BlockId::Number(BlockNumberOrTag::Number(n)) = block {
            self.last_synced_block = n;
        }

        Ok(self)
    }

    async fn update<N, P>(&mut self, _provider: P) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        // M4: 周期对账（vault 余额、pair 活跃状态、registry lane 校准）在此接入。
        Ok(())
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, b256};

    // ---- WETH/USDC @block 25817758 对拍基准（trace 级实测） ----

    /// lane：1 WETH = 2462.88406772 USDC（E8 定点 @block 0x18a0d7b，生产对拍基准）。
    const TEST_LANE: u64 = 246_288_406_772;
    /// engine maxOutput[WETH]（slot keccak(abi.encode(WETH, 8))，= 1.8e21）。
    const TEST_MAX_OUTPUT: u128 = 1_800_000_000_000_000_000_000;
    /// vault WETH 余额（@block 0x18a0d7b；正向 IL 边界 = maxOutput - vault = 253005317793142188091）。
    const TEST_VAULT_WETH: u128 = 1_546_994_682_206_857_811_909;
    /// vault USDC 余额（@block 0x18a0d7b）。
    const TEST_VAULT_USDC: u128 = 1_705_359_318_173;
    /// 反向大额封顶输出（a_norm 封顶在 c1 末段.x 时，A >= 3e12 USDC 恒为此值，eth_call 实证）。
    const TEST_REV_CAP_OUT: u128 = 1_214_462_937_530_800_654_669;

    /// WETH/USDC 8 档正区间曲线（getPairParams @block 0x18a0d7b；(x,y,a,b,c,d)，
    /// x=上界、y=下界、c=截距、d=斜率——M3.1 修正的真实 ABI 顺序，不再交换）。
    fn weth_usdc_segments() -> Vec<FermiCurveSegment> {
        let raw: [(i128, i128, i128, i128, i128, i128); 8] = [
            (
                1_666_666_666_666_667,
                3_333_333_333,
                0,
                0,
                500_000_000_000_000_000,
                100_000_000_000_000_000,
            ),
            (
                16_666_666_666_666_666,
                1_666_666_666_666_667,
                0,
                0,
                600_000_000_000_000_000,
                400_000_000_000_000_000,
            ),
            (
                33_333_333_333_333_332,
                16_666_666_666_666_666,
                0,
                0,
                1_000_000_000_000_000_000,
                500_000_000_000_000_000,
            ),
            (
                100_000_000_000_000_000,
                33_333_333_333_333_332,
                0,
                0,
                1_500_000_000_000_000_000,
                1_000_000_000_000_000_000,
            ),
            (
                166_666_666_666_666_656,
                100_000_000_000_000_000,
                0,
                0,
                2_500_000_000_000_000_000,
                2_500_000_000_000_000_000,
            ),
            (
                333_333_333_333_333_312,
                166_666_666_666_666_656,
                0,
                0,
                5_000_000_000_000_000_000,
                3_000_000_000_000_000_000,
            ),
            (
                666_666_666_666_666_624,
                333_333_333_333_333_312,
                0,
                0,
                8_000_000_000_000_000_000,
                7_000_000_000_000_000_000,
            ),
            (
                1_000_000_000_000_000_000,
                666_666_666_666_666_624,
                0,
                0,
                15_000_000_000_000_000_000,
                15_000_000_000_000_000_000,
            ),
        ];
        raw.iter()
            .map(|&(x, y, a, b, c, d)| FermiCurveSegment { x, y, a, b, c, d })
            .collect()
    }

    /// WETH/USDC 4 档负区间曲线（getPairParams @block 0x18a0d7b；M 失衡度插值用）。
    /// P0 修正：M ∈ (0,5e17] → delta = -2M；M ∈ (-5e17,0) → delta = -0.4M。
    fn weth_usdc_c2_segments() -> Vec<FermiCurveSegment> {
        let raw: [(i128, i128, i128, i128, i128, i128); 4] = [
            (
                -500_000_000_000_000_000,
                -1_000_000_000_000_000_000,
                0,
                0,
                200_000_000_000_000_000,
                0,
            ),
            (
                0,
                -500_000_000_000_000_000,
                0,
                0,
                200_000_000_000_000_000,
                -200_000_000_000_000_000,
            ),
            (
                500_000_000_000_000_000,
                0,
                0,
                0,
                0,
                -1_000_000_000_000_000_000,
            ),
            (
                1_000_000_000_000_000_000,
                500_000_000_000_000_000,
                0,
                0,
                -1_000_000_000_000_000_000,
                0,
            ),
        ];
        raw.iter()
            .map(|&(x, y, a, b, c, d)| FermiCurveSegment { x, y, a, b, c, d })
            .collect()
    }

    /// WETH/USDC 池子（报价方向 token_a=WETH 基准资产，token_b=USDC 计价资产）。
    fn weth_usdc_pool() -> FermiPropPool {
        let weth = address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let usdc = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let mut pool = FermiPropPool {
            token_a: weth,
            token_b: usdc,
            decimals_a: 18,
            decimals_b: 6,
            lane_index: fermi_lane_index(weth, usdc),
            virtual_address: fermi_virtual_address(FERMI_ENGINE_ADDRESS, weth, usdc),
            active: true,
            max_output: U256::from(TEST_MAX_OUTPUT),
            ..Default::default()
        };
        pool.lane = FermiLane {
            update_timestamp: 1,
            flag: 1,
            fair_price_e8: TEST_LANE,
        };
        pool.pair_params.a = 500_000_000_000_000_000;
        pool.pair_params.b = 5000;
        pool.pair_params.c = 3_000_000_000_000;
        pool.pair_params.d = 3_000_000_000_000;
        pool.pair_params.c1 = weth_usdc_segments();
        pool.pair_params.c2 = weth_usdc_c2_segments();
        pool.vault_balances
            .insert(weth, U256::from(TEST_VAULT_WETH));
        pool.vault_balances
            .insert(usdc, U256::from(TEST_VAULT_USDC));
        pool
    }

    #[test]
    fn engine_quote_forward_matches_chain() {
        let pool = weth_usdc_pool();
        let (weth, usdc) = (pool.token_a, pool.token_b);
        // (amount_in, amount_out) — 生产 RPC eth_call(engine.quote) @block 0x18a0d7b，
        // fresh lane override（真实链上 lane 已过期），lane=246288406772，vault USDC=1705359318173，
        // vault WETH=1546994682206857811909（正向输出均低于 vault 上限，未触发截断）。
        let cases: [(u128, u128); 8] = [
            (10_000_000_000_000, 24_626),
            (100_000_000_000_000, 246_261),
            (1_000_000_000_000_000, 2_462_617),
            (10_000_000_000_000_000, 24_626_175),
            (100_000_000_000_000_000, 246_261_647),
            (1_000_000_000_000_000_000, 2_462_605_556),
            (10_000_000_000_000_000_000, 24_625_500_894),
            (50_000_000_000_000_000_000, 123_117_145_455),
        ];
        for (a, expected) in cases {
            let out = pool
                .engine_quote(weth, usdc, U256::from(a))
                .unwrap_or_else(|| panic!("fwd A={a} returned None"));
            assert_eq!(out, U256::from(expected), "fwd A={a}");
        }
    }

    #[test]
    fn engine_quote_reverse_matches_chain() {
        let pool = weth_usdc_pool();
        let (weth, usdc) = (pool.token_a, pool.token_b);
        // (amount_in USDC 1e6, amount_out WETH wei) — 生产 RPC eth_call(engine.quote)
        // @block 0x18a0d7b（同上 fresh lane override）。
        let cases: [(u128, u128); 8] = [
            (100_000_000, 40_601_503_628_722_709),
            (1_000_000_000, 406_014_305_511_483_392),
            (10_000_000_000, 4_060_092_533_081_223_139),
            (100_000_000_000, 40_597_452_286_569_227_071),
            (1_000_000_000_000, 405_710_855_633_618_704_143),
            (3_000_000_000_000, 1_214_462_937_530_800_654_669),
            (10_000_000_000_000, TEST_REV_CAP_OUT),
            (1_000_000_000_000_000, TEST_REV_CAP_OUT),
        ];
        for (a, expected) in cases {
            let out = pool
                .engine_quote(usdc, weth, U256::from(a))
                .unwrap_or_else(|| panic!("rev A={a} returned None"));
            assert_eq!(out, U256::from(expected), "rev A={a}");
        }
    }

    #[test]
    fn engine_quote_cor_boundary() {
        let pool = weth_usdc_pool();
        let (weth, usdc) = (pool.token_a, pool.token_b);
        // 正向 COR：a_norm <= c1[0].y（div1=10000 时 a_norm=3333333333 = 下界）→ revert COR。
        // div1 = A*lane//scale，A≈4.06e12 处 div1 跨过 10000。
        assert!(pool
            .engine_quote(weth, usdc, U256::from(4_060_000_000_000u128))
            .is_none());
        assert!(pool
            .engine_quote(weth, usdc, U256::from(4_061_000_000_000u128))
            .is_some());
        // 反向 COR：a_norm <= c1[0].y（A <= 10000 USDC）→ revert COR。
        assert!(pool
            .engine_quote(usdc, weth, U256::from(9_999u128))
            .is_none());
        assert!(pool
            .engine_quote(usdc, weth, U256::from(10_000u128))
            .is_none());
        assert!(pool
            .engine_quote(usdc, weth, U256::from(10_001u128))
            .is_some());
    }

    #[test]
    fn engine_quote_il_boundary() {
        let pool = weth_usdc_pool();
        let (weth, usdc) = (pool.token_a, pool.token_b);
        // 正向 IL：vault(WETH) + A > maxOutput[WETH]（边界 = 1.8e21 - vault = 253005317793142188091）。
        assert!(pool
            .engine_quote(weth, usdc, U256::from(253_005_317_793_142_188_092u128))
            .is_none());
        assert!(pool
            .engine_quote(weth, usdc, U256::from(200_000_000_000_000_000_000u128))
            .is_some());
        // 反向无 IL：超大金额恒返回封顶值（trace 实证）。
        assert_eq!(
            pool.engine_quote(usdc, weth, U256::from(10u128.pow(30)))
                .unwrap(),
            U256::from(TEST_REV_CAP_OUT)
        );
    }

    #[test]
    fn engine_quote_imbalance_sweep_matches_chain() {
        // 生产 RPC eth_call @block 0x18a0d7b（fresh lane override + 合成 vault 余额）逐位对拍，
        // 覆盖 M<0（r=0.05/0.1/0.25）、M=0（r=0.5）、M>0（r=0.75）与反向 vault 余额封顶。
        let base = weth_usdc_pool();
        let (weth, usdc) = (base.token_a, base.token_b);
        let lane = base.lane.fair_price_e8 as u128;
        let scale = 10u128.pow(8 + (base.decimals_a - base.decimals_b) as u32);
        let l1 = 10u128.pow(12); // vault USDC（quote 资产）
                                 // (r_percent, [(fwd A, fwd out)], [(rev A, rev out)])
        let cases: [(u128, &[(u128, u128)], &[(u128, u128)]); 5] = [
            (
                5,
                &[(10u128.pow(18), 2_462_743_862)],
                &[(10u128.pow(11), 21_369_897_039_337_042_465)],
            ),
            (
                10,
                &[(10u128.pow(13), 24_627), (10u128.pow(18), 2_462_738_937)],
                &[
                    (10u128.pow(8), 40_599_304_670_398_550),
                    (10u128.pow(11), 40_595_253_547_538_641_164),
                ],
            ),
            (25, &[(10u128.pow(18), 2_462_724_161)], &[]),
            (
                50,
                &[(10u128.pow(18), 2_462_699_534)],
                &[(10u128.pow(8), 40_599_954_259_085_464)],
            ),
            (
                75,
                &[(10u128.pow(18), 2_462_576_399)],
                &[
                    (10u128.pow(8), 40_601_984_358_499_446),
                    (10u128.pow(11), 40_597_932_968_375_201_464),
                ],
            ),
        ];
        for (r_pct, fwd, rev) in cases {
            let mut pool = base.clone();
            let l2 = r_pct * l1 / (100 - r_pct); // USDC 计价，折合 quote 资产
            let v_weth = l2 * scale / lane;
            pool.vault_balances.insert(weth, U256::from(v_weth));
            pool.vault_balances.insert(usdc, U256::from(l1));
            for &(a, expected) in fwd {
                let out = pool
                    .engine_quote(weth, usdc, U256::from(a))
                    .unwrap_or_else(|| panic!("r={r_pct} fwd A={a} returned None"));
                assert_eq!(out, U256::from(expected), "r={r_pct} fwd A={a}");
            }
            for &(a, expected) in rev {
                let out = pool
                    .engine_quote(usdc, weth, U256::from(a))
                    .unwrap_or_else(|| panic!("r={r_pct} rev A={a} returned None"));
                assert_eq!(out, U256::from(expected), "r={r_pct} rev A={a}");
            }
        }
    }

    #[test]
    fn engine_quote_caps_at_vault_balance() {
        let mut pool = weth_usdc_pool();
        let (weth, usdc) = (pool.token_a, pool.token_b);
        // 金库只有 1000 USDC：1 WETH 的链上输出（2462605556）被截断为 1000e6。
        pool.vault_balances
            .insert(usdc, U256::from(1_000_000_000u128));
        let out = pool
            .engine_quote(weth, usdc, U256::from(10u128.pow(18)))
            .unwrap();
        assert_eq!(out, U256::from(1_000_000_000u128));
    }

    #[test]
    fn lane_apply_guard() {
        let mut pool = weth_usdc_pool();
        let newer = FermiLane {
            update_timestamp: 200,
            flag: 1,
            fair_price_e8: 123,
        };
        let older = FermiLane {
            update_timestamp: 100,
            flag: 1,
            fair_price_e8: 456,
        };
        assert!(pool.apply_titan_lane(newer));
        assert!(!pool.apply_titan_lane(older));
        assert_eq!(pool.lane.fair_price_e8, 123);
    }

    #[test]
    fn deployment_key_and_virtual_address() {
        let pool = weth_usdc_pool();
        assert_eq!(
            pool.deployment_key(),
            (
                FERMI_ENGINE_ADDRESS,
                FERMI_WRAPPER_ADDRESS,
                FERMI_REGISTRY_ADDRESS,
                FERMI_VAULT_ADDRESS
            )
        );
        assert_ne!(pool.virtual_address, Address::ZERO);
    }

    #[test]
    fn max_output_slot_matches_trace() {
        let weth = address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        // trace 实证的 SLOAD key（IL 检查 @block 25817758）。
        assert_eq!(
            fermi_max_output_slot(weth),
            b256!("5cc08dfcef394bb3e1501dd9c602b313a910bc96e6e3b9f14c10c5608560cb26")
        );
    }
}

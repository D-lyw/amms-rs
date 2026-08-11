//! Fee-on-transfer (FoT) token 支持
//!
//! # 扣税语义（链上取证确认）
//!
//! 存在三类扣税语义（[`FotTaxType`] 变体）。已注册 token 的扣税模型见下方
//! **扣税档案**——每节对应一个 token，取证方式与判定规则完整记录，
//! 是长期迭代维护的唯一事实来源；**修改扣税语义必须同步更新对应档案**。
//!
//! # 扣税档案 · XLS（0x64af27d3...）= BuySell{buy:0, sell:300}，XlayerSwapV2 池白名单，无 swapBack
//!
//! **取证**：2026-08-09 曾按 Transfer 事件逐笔金额 + balanceOf 差值误判为
//! `both_sides`（**已推翻**，2026-08-10）；以 **XLS 开源合约源码 + proxy 反汇编
//! 为硬标准**定案，行为测试（eth_call 差分）与链上交易（debug_trace）仅作佐证。
//!
//! ## 合约架构
//!   - `XLS` = 标准 ERC20（2,633 B 代码，开源）；`c` = proxy 0x15e98f9e...
//!     存于 slot 5（`_transfer` 内 `IXLSC(c).getSlippage(...)`）
//!   - proxy → impl 0xbf7bbe66...（23,397 B）：`getSlippage`（税判定）+
//!     `slippage`（分红分发）
//!
//! ## 扣税判定（硬标准，selector 已用 keccak 与源码逐一验证）
//!
//! `_transfer(from, to, amount)` 先调 `IXLSC(c).getSlippage(from, to, amount)`
//! （selector `0x6473e7d6`），按 **参数 from/to** 判定（与 msg.sender 无关，
//! 差分测试三次一致）：
//!
//! | from ∈ XlayerSwapV2 池（factory 0x717ab5de...）| to ∈ XlayerSwapV2 池 | 税率 |
//! |:--|:--|:--|
//! | 是 | 任意 | **0%**（池子转出豁免）|
//! | 否 | 是 | **3%**（卖进池子，池子实收 97%，K 按净额记账）|
//! | 否 | 否 | 0%（EOA↔EOA、合约↔EOA、其他 DEX 池均不扣）|
//!
//! 即：买出池（from=池）免税、卖进池（to=池）扣 3% → 与 RTX 方向同构，
//! 映射为 `BuySell{buy_fee_bps:0, sell_fee_bps:300}`。
//! 触发池 = **XlayerSwapV2 factory 下所有含 XLS 的池**（非单个 pair），
//! 经 `pairs` 白名单集合表达；graph 生成阶段从 poolindex 池数据动态展开，
//! 新池被索引后自动生效，无需改代码。
//!
//! ## 拆账与附加机制
//!   - 税 = `floor(amount × 300 / 10000)`，从 from 转给 proxy，
//!     proxy 再转 1/30（≈0.1%）给 0xdead 销毁，净留 2.9%；net 转给 to
//!   - 随后 `IXLSC(c).slippage(from, to, amount, slippageValue)`
//!     （selector `0x7783387d`）触发 DividenTracker 分红分发，单次 >588k gas
//!     （实测 ~1.03M）：**含 XLS 的路径结构性不可执行**
//!     （引擎 gas 估算需为含该 token 的 hop 加 ~1.15M 预算）
//!   - **无 swapBack**：分红在 proxy 侧独立分发，注册时
//!     `swap_back_threshold` 传 [`U256::MAX`] 永不触发
//!
//! ## 注册 JSON（fot_tokens 表 / ndjson token 条目）
//!
//! ```json
//! {"type":"buy_sell","buy_fee_bps":0,"sell_fee_bps":300,
//!  "pairs":["0xa70e64138f1c70f0aa5ce7a5ddde78ecdb49a144",
//!           "0x3d49cdd23bf689510ece56dd90f4b739c309ef05"],
//!  "swap_back_threshold":"115792089237316195423570985008687907853269984665640564039457584007913129639935"}
//! ```
//!
//! # 扣税档案 · RTX（0x18a4f9d4...）= BuySell{buy:300, sell:300}，单主池，swapBack 1250e18
//!
//! **取证**：2026-08-09，TaxDividendToken 类合约源码 + 交易 trace。
//!   - `automatedMarketMakerPairs` 白名单**只有主池 0xb8960e3b... 一个地址**
//!     （`graduate()` 一次性写入，无管理函数）：只有与主池交互的 transfer 扣税，
//!     其他池（如 V4 PoolManager）**不扣税**
//!   - 卖进主池（to=主池）：扣 `sell_fee_bps=300`，池子实收 net（K 按 balanceOf 差值记账）
//!   - 买出主池（from=主池）：扣 `buy_fee_bps=300`，接收方实收 net
//!   - **swapBack**：卖出到主池时若合约自身余额（`balanceOf(address(this))`，
//!     [`swap_back_balance`]）>= `swap_back_threshold`（1250e18），
//!     先以合约全部累积余额砸入主池（`swapping` 豁免不扣税，池子实收全额），
//!     换出的另一侧全额给分红分发器（不参与用户路径），再算用户主 swap
//!
//! ## swapBack 余额同步方式（2026-08-10 起：事件驱动，替代 1s 轮询）
//!
//! 自持余额 = `balanceOf(RTX 合约自身)`，变化谱系只有 3 种，全部通过标准
//! ERC20 Transfer 事件表达（RTX impl 无 mint/burn，Transfer emit 全合约唯一）：
//!   - 卖出税：`Transfer(用户 → RTX合约, 名义×3%)` → `+= v`
//!   - 买入税：`Transfer(主池 → RTX合约, gross×3%)` → `+= v`
//!   - swapBack dump：`Transfer(RTX合约 → 主池, 全部自持)` → **置 0**
//!     （dump 转出全部，置零而非减法可清除此前任何事件缺失导致的漂移，
//!     是天然强制对齐点；24h 窗口 440 次 dump，已对账 460/460 采样点）
//! 同步链路：启动时 [`init_swap_back_balance_snapshot`] 一次快照（sync chain_tip），
//! 事件流（实时 flashblocks + 断流 backfill 同源覆盖）按
//! `(block, txIndex, logIndex)` 序调 [`apply_swap_back_transfer`] 增量累加，
//! 水位 `(block, txIndex, logIndex)` 三元组防重复（同块内多事件全部应用）。
//! 旧的 1s 周期 RPC 轮询已移除——
//! 它无法识别机会区块内的增量（事故 67598082：竞争者 idx14 买入推高自持
//! 32,071→117,250，模拟仍用 block-1 快照 → swapBack dump 低估 3.66 倍 → revert）。
//!
//! # 通用语义（变体定义，与具体 token 无关）
//!
//! ## [`FotTaxType::FlatRate`]（单侧，输出侧）
//!
//! 扣税发生在 **swap 之后、token transfer hook 内部**，且 **只在 from = pool（池子）
//! 转出时扣税**（即用户从池子买入该 token 时）：
//!   - from = pool（买）：池子 swap math 输出 gross，transfer hook 扣税，
//!     接收方到手 net = gross × (10000 - fee_bps) / 10000（向下取整）
//!   - to = pool（卖）：不扣税，池子收到全额
//!   - EOA → EOA、合约 → pool：不扣税
//!
//! ## [`FotTaxType::BothSides`]（双向，输入侧 + 输出侧）
//!
//! **每次 transfer（无论方向）都扣税**：发送者付出名义 gross，
//! 接收者实收 net = gross × (10000 - fee_bps) / 10000。
//! 输入侧（user→pool）池子实收 net，K 检查按 net 记账；
//! 输出侧（pool→user）接收者实收 net。
//!
//! 模拟时（`amount_in` 语义 = **名义金额**，池子实收为扣税后净额）：
//!   - 输入侧（user→pool）：pool 模拟内部按税率扣税后以净额参与 math。
//!     hop 链中 hop N+1 的输入 = hop N 输出（`fot_net` 后实收）作为**名义**
//!     再被输入侧扣一次——链上引擎中转每 hop 一次 transfer 各扣一次税，
//!     `0.97 × 0.97` 正是链上真实语义，不是双重扣税
//!   - 输出侧（pool→user）：math 输出 gross，返回给调用方的是扣税后 net
//!
//! ## [`FotTaxType::BuySell`]（买卖分离 + swapBack，仅白名单池生效）
//!
//! 模拟层语义（触发判定见 [`FotTaxType::applies_to_pool`]）：
//!   - 输入侧（token 进白名单池）：扣 `sell_fee_bps`，且模拟前先做
//!     swapBack 预交易（余额来自 [`swap_back_balance`] 缓存，事件驱动增量
//!     维护；未读到按 0 = 不触发处理）
//!   - 输出侧（token 出白名单池）：扣 `buy_fee_bps`
//!   - pool ∉ pairs 时任何方向都不扣税
//!
//! # 注册方式
//!
//! FoT 参数 **不会自动检测**，必须在初始化阶段通过 [`register_fot_token`]
//! 显式注册（或直接在 Token 序列化数据中标注 `fot_tax` 字段）。
//! 优先级：Token 上显式标注的 `fot_tax` > registry 运行时注入。
//! **维护提醒**：新增 FoT token 时在 `crates/pools_index/src/fot_tokens.rs`
//! 注册（写入 fot_tokens 表），graph 生成阶段自动展开 `pairs` 并写入 ndjson。

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use alloy::primitives::{Address, B256, U256};
use std::time::{Duration, Instant};
use serde::{Deserialize, Serialize};

use super::Token;

/// FoT 扣税类型
///
/// 不同的扣税 token 逻辑差异很大（买卖分离税率、仅卖出扣税、反射分红等），
/// 注册时必须指定具体类型，方便后续扩展支持多种扣税逻辑。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FotTaxType {
    /// 池子转出时扣税（输出侧 FoT，单侧税）
    ///
    /// 扣税只发生在 **from = pair（池子）转出该 token 时**（用户从池子买入），
    /// 即 swap math 输出 gross 后、transfer hook 内扣税，接收方到手
    /// `net = gross - floor(gross × fee_bps / 10000)`。
    ///
    /// **不**表示每次 transfer 都扣税：to = pool（卖出）、EOA→EOA、
    /// 合约→pool 均不扣税（链上取证确认，见模块文档）。
    ///
    /// `fee_bps` 以 basis points 计（1 bps = 0.01%），例如 3% 税 = 300。
    FlatRate { fee_bps: u64 },

    /// 双向扣税（输入侧 + 输出侧）
    ///
    /// **每次 transfer（无论方向）都扣税**（XLS 实测，见模块文档）：
    ///   - 输入侧（user→pool）：池子实收 net，K 检查按 net 记账
    ///   - 输出侧（pool→user）：接收者实收 net，同 [`FotTaxType::FlatRate`]
    ///
    /// 模拟层语义：hop 链中一次 transfer 的税由 hop N 输出侧 `fot_net` 捕获，
    /// 输入侧不重复扣税；仅引擎层起点转账（flash→池）需用
    /// `Token::fot_input_net` 将名义金额折算为池子实收净额入池。
    ///
    /// `fee_bps` 以 basis points 计（1 bps = 0.01%），例如 3% 税 = 300。
    BothSides { fee_bps: u64 },

    /// 买卖分离税率 + swapBack 分红（TaxDividendToken 类合约）
    ///
    /// 已注册实例：RTX（单主池白名单 + swapBack，2026-08-09 取证）、
    /// XLS（XlayerSwapV2 池集合白名单 + 无 swapBack，2026-08-10 取证），
    /// 完整模型见模块文档扣税档案。
    ///
    /// 通用方向语义（触发池 = `pairs` 白名单集合）：
    ///   - to ∈ pairs（用户卖该 token 进池）：扣 `sell_fee_bps`，
    ///     池子实收 net（K 检查按 balanceOf 差值 = net 记账）
    ///   - from ∈ pairs（用户买该 token 出池）：扣 `buy_fee_bps`，
    ///     接收方实收 net
    ///   - **swapBack**：卖出到白名单池时，若合约自身持有余额
    ///     （`balanceOf(address(this))`）>= `swap_back_threshold`，
    ///     在本次 transfer 扣税**之前**先执行一次预交易：把合约全部
    ///     累积 token 卖入池子（`swapping` 豁免扣税，池子实收全额），
    ///     换出的另一侧全额给分红分发器（不参与用户路径），
    ///     池子 reserve 被砸后再执行用户的主 swap
    ///
    /// 模拟层语义：
    ///   - 输入侧（token 进白名单池）：扣 `sell_fee_bps`，且模拟前先做
    ///     swapBack 预交易（余额来自 [`swap_back_balance`] 缓存，
    ///     由周期任务 1s 刷新；未读到按 0 = 不触发处理）
    ///   - 输出侧（token 出白名单池）：扣 `buy_fee_bps`
    ///   - pool ∉ pairs 时任何方向都不扣税（[`FotTaxType::applies_to_pool`]）
    BuySell {
        /// 池子转出该 token（用户买）时扣税，basis points（1 bps = 0.01%）
        buy_fee_bps: u64,
        /// 该 token 进入池子（用户卖）时扣税，basis points（1 bps = 0.01%）
        sell_fee_bps: u64,
        /// 白名单池集合（RTX：唯一主池；XLS：XlayerSwapV2 factory 下所有含该
        /// token 的池，graph 生成阶段从 poolindex 池数据动态展开；
        /// 反序列化缺省为空 = 任何池都不扣税）
        #[serde(default)]
        pairs: Vec<Address>,
        /// 合约自身持有余额 >= 该值触发 swapBack 预交易（含精度，如 1250e18；
        /// XLS 类无 swapBack，注册时传 [`U256::MAX`] 永不触发）
        swap_back_threshold: U256,
    },
    // 未来扩展（示例，变体命名用 snake_case 与 serde rename_all 保持一致）：
    // /// 仅卖出扣税
    // SellOnly { sell_fee_bps: u64 },
    // /// 反射型：每笔 transfer 按比例向持有者分红
    // Reflection { fee_bps: u64 },
}

impl FotTaxType {
    /// 税率分母（万分之一）
    pub const BASIS: u64 = 10_000;

    /// 输入侧（user→pool 方向）是否扣税。
    ///
    /// [`FotTaxType::BothSides`] 与 [`FotTaxType::BuySell`]（卖出方向）返回
    /// `true`（每次 transfer 都扣税）；[`FotTaxType::FlatRate`] 只在池子
    /// 转出（输出侧）时扣税，输入侧不扣。
    pub fn input_taxed(&self) -> bool {
        matches!(
            self,
            FotTaxType::BothSides { .. } | FotTaxType::BuySell { .. }
        )
    }

    /// 该税种是否对指定池生效。
    ///
    /// [`FotTaxType::BuySell`] 仅白名单池（`pairs` 集合）扣税，其他池子
    /// （如 V4 PoolManager）与该 token 的 transfer 不扣税；其余税种
    /// 对所有池子生效。
    pub fn applies_to_pool(&self, pool: Address) -> bool {
        match self {
            FotTaxType::BuySell { pairs, .. } => pairs.contains(&pool),
            _ => true,
        }
    }

    /// 输出侧（该 token 从池子转出）扣税后到手 net。
    ///
    /// [`FotTaxType::FlatRate`]/[`FotTaxType::BothSides`] 扣统一 fee，
    /// [`FotTaxType::BuySell`] 扣 `buy_fee_bps`（from = pair = 用户买）。
    /// 调用方需先用 [`FotTaxType::applies_to_pool`] 判断池子是否生效。
    pub fn output_net(&self, gross: U256) -> U256 {
        let fee_bps = match self {
            FotTaxType::FlatRate { fee_bps } | FotTaxType::BothSides { fee_bps } => *fee_bps,
            FotTaxType::BuySell { buy_fee_bps, .. } => *buy_fee_bps,
        };
        Self::apply_fee(gross, fee_bps)
    }

    /// 输入侧（该 token 进入池子）池子实收净额。
    ///
    /// [`FotTaxType::FlatRate`] 输入侧不扣税（返回原值）；
    /// [`FotTaxType::BothSides`] 扣统一 fee；[`FotTaxType::BuySell`]
    /// 扣 `sell_fee_bps`（to = pair = 用户卖，池子实收 net）。
    /// 调用方需先用 [`FotTaxType::applies_to_pool`] 判断池子是否生效。
    pub fn input_net(&self, gross: U256) -> U256 {
        match self {
            FotTaxType::FlatRate { .. } => gross,
            FotTaxType::BothSides { fee_bps } => Self::apply_fee(gross, *fee_bps),
            FotTaxType::BuySell { sell_fee_bps, .. } => Self::apply_fee(gross, *sell_fee_bps),
        }
    }

    /// 输出侧反算：接收方到手 `net` 所需的最小 gross 金额。
    ///
    /// 方向语义与 [`FotTaxType::output_net`] 一致。
    pub fn output_gross_up(&self, net: U256) -> U256 {
        let fee_bps = match self {
            FotTaxType::FlatRate { fee_bps } | FotTaxType::BothSides { fee_bps } => *fee_bps,
            FotTaxType::BuySell { buy_fee_bps, .. } => *buy_fee_bps,
        };
        Self::gross_up_for(net, fee_bps)
    }

    /// 输入侧反算：池子需实收 `net` 时用户需转的名义金额。
    ///
    /// 方向语义与 [`FotTaxType::input_net`] 一致（FlatRate 不 gross-up）。
    pub fn input_gross_up(&self, net: U256) -> U256 {
        match self {
            FotTaxType::FlatRate { .. } => net,
            FotTaxType::BothSides { fee_bps } => Self::gross_up_for(net, *fee_bps),
            FotTaxType::BuySell { sell_fee_bps, .. } => Self::gross_up_for(net, *sell_fee_bps),
        }
    }

    fn apply_fee(gross: U256, fee_bps: u64) -> U256 {
        if fee_bps >= Self::BASIS {
            return U256::ZERO;
        }
        let tax = gross * U256::from(fee_bps) / U256::from(Self::BASIS);
        gross - tax
    }

    fn gross_up_for(net: U256, fee_bps: u64) -> U256 {
        if fee_bps >= Self::BASIS || net.is_zero() {
            return U256::ZERO;
        }
        let numerator = (net - U256::from(1u8)) * U256::from(Self::BASIS);
        let denominator = U256::from(Self::BASIS - fee_bps);
        numerator / denominator + U256::from(1u8)
    }

    /// 该税种下，gross 金额实际到手 net
    ///
    /// 链上语义（取证确认）：合约先算 `tax = floor(gross × fee_bps / 10000)`，
    /// 再转出 `net = gross - tax`。等价于 `ceil(gross × (10000-fee) / 10000)`，
    /// 与直接 `floor(gross × (10000-fee) / 10000)` 可能差 1 wei。
    ///
    /// 输出侧与输入侧共用同一折扣数学（BothSides 输入侧同样调用本方法）。
    pub fn net_of_gross(&self, gross: U256) -> U256 {
        // 保留（兼容/测试）：BuySell 取 buy 侧（输出侧方向），等价 output_net
        self.output_net(gross)
    }

    /// 反算：接收方到手 net 所需的最小 gross 金额
    ///
    /// 满足 `net_of_gross(gross_up(net)) >= net`，且是满足该条件的最小值：
    /// `gross_up(net) = floor((net-1) × 10000 / (10000 - fee_bps)) + 1`
    /// （net = 0 时返回 0）。
    ///
    /// 用于 exact-out 场景：池子 math 必须先输出 gross，transfer 扣税后
    /// 接收方才能拿到 net，因此 amount_out 需先 gross-up 再进 `get_amount_in`。
    /// BothSides 输入侧同样调用本方法（池子需实收 net，用户需转 gross-up 名义）。
    pub fn gross_up(&self, net: U256) -> U256 {
        // 保留（兼容/测试）：BuySell 取 buy 侧（输出侧方向），等价 output_gross_up
        self.output_gross_up(net)
    }
}

/// 全局 FoT token 注册表（初始化阶段写入，运行期只读）
static FOT_REGISTRY: OnceLock<RwLock<HashMap<Address, FotTaxType>>> = OnceLock::new();

fn registry() -> &'static RwLock<HashMap<Address, FotTaxType>> {
    FOT_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 注册一个 FoT token（**必须在初始化阶段调用**，amms 不会自动检测）
pub fn register_fot_token(token: Address, tax: FotTaxType) {
    registry().write().unwrap().insert(token, tax);
}

/// 查询 token 的 FoT 税率（未注册返回 `None`）
pub fn fot_tax(token: Address) -> Option<FotTaxType> {
    registry().read().unwrap().get(&token).cloned()
}

/// 初始化阶段应用到 Token：若已注册，注入 `fot_tax` 字段
///
/// 若 Token 上已有显式标注的 `fot_tax`（如反序列化数据），则保持不变。
pub fn apply_to_token(token: &mut Token) {
    if token.fot_tax.is_none() {
        if let Some(tax) = fot_tax(token.address) {
            token.fot_tax = Some(tax);
        }
    }
}

/// BuySell token 合约自身持有余额缓存（swapBack 触发判定用）
///
/// 链上语义：`balanceOf(address(this))`，即 token 合约自持 token 余额。
///
/// 同步方式（2026-08-10 重构，替代旧 1s 周期 RPC 轮询）——事件驱动：
///   1. 启动时 [`init_swap_back_balance_snapshot`] 读取一次快照
///      （block = sync chain_tip，与事件流 backfill 起点对齐）
///   2. 事件流（实时 flashblocks + 断流 backfill 同源覆盖）按
///      `(block, txIndex, logIndex)` 序调 [`apply_swap_back_transfer`]：
///        to   == token → balance += v（sell/buy 3% 税进自持）
///        from == token → balance = 0（swapBack dump 转出全部，置零清漂移）
///   3. 水位 `(block, txIndex, logIndex)` 三元组防重复（backfill/实时重叠、
///      断线重连重放的事件丢弃；**同块内多事件按位置序全部应用**——
///      块粒度水位会把同块第二条及以后事件误丢，导致 dump 归零失效）
///
/// 旧轮询无法识别机会区块内的增量（事故 67598082：竞争者块内买入推高自持，
/// 模拟仍用旧快照 → swapBack dump 量低估 3.66 倍 → "V4: unprofitable" revert）。
///
/// 缓存携带刷新时刻（`Instant`），供模拟层判断数据新鲜度。
#[derive(Debug, Clone, Copy)]
struct SwapBackBalanceState {
    balance: U256,
    refreshed_at: Instant,
    /// 已应用事件的最高链上位置 `(block, txIndex, logIndex)`
    /// （<= 该位置的事件视为已含在状态中，丢弃）
    last_applied: (u64, u64, u64),
}

static FOT_SWAP_BACK_BALANCES: OnceLock<RwLock<HashMap<Address, SwapBackBalanceState>>> =
    OnceLock::new();

fn swap_back_balances() -> &'static RwLock<HashMap<Address, SwapBackBalanceState>> {
    FOT_SWAP_BACK_BALANCES.get_or_init(|| RwLock::new(HashMap::new()))
}

/// ERC20 Transfer 事件签名 keccak256("Transfer(address,address,uint256)")
///
/// = 0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef
/// （硬编码避免非 const 的 keccak256 调用；alloy 宏不便在此使用）
pub const ERC20_TRANSFER_SIG: B256 = B256::new([
    0xdd, 0xf2, 0x52, 0xad, 0x1b, 0xe2, 0xc8, 0x9b, 0x69, 0xc2, 0xb0, 0x68, 0xfc, 0x37, 0x8d,
    0xaa, 0x95, 0x2b, 0xa7, 0xf1, 0x63, 0xc4, 0xa1, 0x16, 0x28, 0xf5, 0x5a, 0x4d, 0xf5, 0x23,
    0xb3, 0xef,
]);

/// 判断税率是否需要在事件驱动下监控 swapBack 余额。
///
/// `swap_back_threshold = U256::MAX` 的 BuySell token（如 XLS）永不触发
/// swapBack dump，自持余额不影响任何模拟结果，无需监控。
fn is_swap_back_monitored_tax(tax: &FotTaxType) -> bool {
    matches!(
        tax,
        FotTaxType::BuySell {
            swap_back_threshold,
            ..
        } if *swap_back_threshold != U256::MAX
    )
}

/// 需要事件驱动监控 swapBack 余额的 token 地址列表（BuySell 且 threshold != MAX）
pub fn swap_back_monitored_tokens() -> Vec<Address> {
    registry()
        .read()
        .unwrap()
        .iter()
        .filter(|(_, t)| is_swap_back_monitored_tax(t))
        .map(|(a, _)| *a)
        .collect()
}

/// 判断地址是否为受监控的 swapBack token（事件流防御性过滤用）
pub fn is_swap_back_monitored(token: Address) -> bool {
    registry()
        .read()
        .unwrap()
        .get(&token)
        .map(is_swap_back_monitored_tax)
        .unwrap_or(false)
}

/// 初始化 swapBack 余额快照（启动时一次，block = 事件流增量起点）。
///
/// block 必须与 state_space `realtime_head` 初始值（sync 的 chain_tip）一致：
/// 事件流 backfill 从 chain_tip+1 开始，事件全部 > 本水位 → 无重叠无缺口。
/// 快照水位取块末位置 `(block, u64::MAX, u64::MAX)`：块内 tx/log 索引
/// 无意义，但任何 block+1 起的事件位置必然 > 块末水位。
pub fn init_swap_back_balance_snapshot(token: Address, balance: U256, block: u64) {
    swap_back_balances().write().unwrap().insert(
        token,
        SwapBackBalanceState {
            balance,
            refreshed_at: Instant::now(),
            last_applied: (block, u64::MAX, u64::MAX),
        },
    );
}

/// 应用一条监控 token 合约的 Transfer 事件（事件流按
/// `(block, txIndex, logIndex)` 序调用）。
///
/// - `(block, tx_index, log_index) <= last_applied` → 丢弃（水位防重复：
///   backfill/实时重叠、断线重连重放；**只丢弃完全相同位置的重放**，
///   同块内多事件按位置序全部应用——块粒度水位会把同块第二条及以后
///   事件误丢（如竞争者块：税收入账占用水位后，dump 归零事件被丢弃，
///   余额停在错误值污染后续块模拟））
/// - `to == token` → `balance += value`（sell/buy 方向 3% 税进自持）
/// - `from == token` → `balance = 0`（swapBack dump 转出全部自持；**置零而非
///   减法**——清除此前任何事件缺失导致的漂移，dump 是天然强制对齐点）
pub fn apply_swap_back_transfer(
    token: Address,
    from: Address,
    to: Address,
    value: U256,
    block: u64,
    tx_index: u64,
    log_index: u64,
) {
    let mut guard = swap_back_balances().write().unwrap();
    let state = guard.entry(token).or_insert(SwapBackBalanceState {
        balance: U256::ZERO,
        refreshed_at: Instant::now(),
        last_applied: (0, 0, 0),
    });
    if (block, tx_index, log_index) <= state.last_applied {
        return;
    }
    // 语义与 verify_rtx_swapback_balance 取证脚本一致：from==self 优先归零
    // （自转账 from==to==token 在链上不可能发生，防御性取归零分支）
    match (from == token, to == token) {
        (false, true) => state.balance = state.balance.saturating_add(value),
        (true, _) => state.balance = U256::ZERO,
        _ => {}
    }
    state.last_applied = (block, tx_index, log_index);
    state.refreshed_at = Instant::now();
}

/// 判断 log 是否为受监控 swapBack token 的 ERC20 Transfer 事件
///
/// 生产事件流（state_space `apply_logs_for_block_timed` FoT 分支）与验证/
/// 回放脚本统一经此过滤：topic0 == Transfer 签名且地址在受监控注册表
/// （BuySell 且 threshold != MAX）。避免各脚本手动复制提取逻辑产生漂移
/// （曾因脚本自研累加器未覆盖生产水位逻辑导致盲区，2026-08-11 已修复）。
pub fn is_swap_back_transfer_log(log: &alloy::rpc::types::Log) -> bool {
    log.topics().first() == Some(&ERC20_TRANSFER_SIG) && is_swap_back_monitored(log.address())
}

/// 应用一条受监控 token 的 Transfer 日志（生产事件流与回放脚本的统一入口）
///
/// 等价 [`apply_swap_back_transfer`] 的单条语义：从 log 提取
/// `(from, to, value, txIndex, logIndex)` 后调用。日志必须已按
/// `(block, txIndex, logIndex)` 排序。非受监控/非 Transfer/字段缺失的日志
/// 返回 false（不 panic）。
pub fn apply_swap_back_transfer_log(log: &alloy::rpc::types::Log, block: u64) -> bool {
    if !is_swap_back_transfer_log(log) {
        return false;
    }
    let (Some(from_t), Some(to_t)) = (log.topics().get(1), log.topics().get(2)) else {
        return false;
    };
    let from = Address::from_slice(&from_t.0[12..]);
    let to = Address::from_slice(&to_t.0[12..]);
    let value = U256::from_be_slice(&log.data().data);
    apply_swap_back_transfer(
        log.address(),
        from,
        to,
        value,
        block,
        log.transaction_index.unwrap_or(u64::MAX),
        log.log_index.unwrap_or(u64::MAX),
    );
    true
}

/// 更新 token 合约自身持有余额（兼容/测试用途；生产路径已改为事件驱动快照+增量）
pub fn set_swap_back_balance(token: Address, balance: U256) {
    swap_back_balances().write().unwrap().insert(
        token,
        SwapBackBalanceState {
            balance,
            refreshed_at: Instant::now(),
            last_applied: (0, 0, 0),
        },
    );
}

/// 读取 token 合约自身持有余额（模拟层调用）
///
/// 未读到（快照尚未初始化）返回 0 = 视为不触发 swapBack（保守：
/// 宁可高估输出，也避免在余额未知时凭空砸盘）。
pub fn swap_back_balance(token: Address) -> U256 {
    swap_back_balances()
        .read()
        .unwrap()
        .get(&token)
        .map(|s| s.balance)
        .unwrap_or(U256::ZERO)
}

/// 读取 token 合约自身持有余额 + 最后刷新时刻
///
/// 未读到返回 `None`（快照尚未初始化）。
pub fn swap_back_balance_with_refresh(token: Address) -> Option<(U256, Instant)> {
    swap_back_balances()
        .read()
        .unwrap()
        .get(&token)
        .map(|s| (s.balance, s.refreshed_at))
}

/// 缓存数据已 stale 的判定阈值：超过该时长未刷新视为不可信。
///
/// 事件驱动下刷新时刻随事件更新；无事件的平静窗口会 stale，属正常
/// （事件驱动的状态在无事件时天然不变，stale 仅表示"长时间无新事件"）。
pub const SWAP_BACK_BALANCE_STALE_AFTER: Duration = Duration::from_secs(30);

/// 缓存是否 stale（最后刷新距今超过 [`SWAP_BACK_BALANCE_STALE_AFTER`] 或从未刷新）
pub fn swap_back_balance_is_stale(token: Address) -> bool {
    match swap_back_balance_with_refresh(token) {
        Some((_, refreshed_at)) => refreshed_at.elapsed() > SWAP_BACK_BALANCE_STALE_AFTER,
        None => true,
    }
}

/// 所有已注册 BuySell 的 token 地址列表
pub fn buy_sell_tokens() -> Vec<Address> {
    registry()
        .read()
        .unwrap()
        .iter()
        .filter(|(_, t)| matches!(t, FotTaxType::BuySell { .. }))
        .map(|(a, _)| *a)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::uint;

    // 链上取证精确值（XLS token，3% 税，交易 0xf90b9c...）：
    //   gross 656445408742641273262 → net 636752046480362035065
    const GROSS: U256 = uint!(656445408742641273262_U256);
    const NET: U256 = uint!(636752046480362035065_U256);

    #[test]
    fn test_flat_rate_net_matches_onchain_forensics() {
        let tax = FotTaxType::FlatRate { fee_bps: 300 };
        assert_eq!(tax.net_of_gross(GROSS), NET);
        // 链上语义：tax = floor(gross×fee/10000)，与 floor(gross×9700/10000) 可能差 1
        let floor_direct = GROSS * U256::from(9700u16) / U256::from(10000u16);
        assert_eq!(NET, floor_direct + U256::from(1u8));
    }

    #[test]
    fn test_flat_rate_gross_up_inverse() {
        let tax = FotTaxType::FlatRate { fee_bps: 300 };
        // net -> gross_up -> net 应回到原值
        let gross = tax.gross_up(NET);
        assert_eq!(tax.net_of_gross(gross), NET);
        // 且是最小解：gross-1 已不够
        assert!(tax.net_of_gross(gross - U256::from(1u8)) < NET);
    }

    #[test]
    fn test_flat_rate_gross_up_minimal() {
        let tax = FotTaxType::FlatRate { fee_bps: 300 };
        // net=97: gross=99 即满足（net_of_gross(99)=99-floor(2.97)=97），100 不是最小
        let gross = tax.gross_up(U256::from(97u8));
        assert_eq!(gross, U256::from(99u8));
        assert_eq!(tax.net_of_gross(gross), U256::from(97u8));
        assert_eq!(tax.net_of_gross(U256::from(98u8)), U256::from(96u8));
        // net=0 -> 0
        assert_eq!(tax.gross_up(U256::ZERO), U256::ZERO);
    }

    #[test]
    fn test_flat_rate_full_fee_returns_zero() {
        let tax = FotTaxType::FlatRate { fee_bps: 10_000 };
        assert_eq!(tax.net_of_gross(GROSS), U256::ZERO);
        assert_eq!(tax.gross_up(GROSS), U256::ZERO);
    }

    #[test]
    fn test_both_sides_net_matches_onchain_forensics() {
        let tax = FotTaxType::BothSides { fee_bps: 300 };
        // 输入/输出侧同一折扣数学：名义 656445408742641273262 → 实收 636752046480362035065
        assert_eq!(tax.net_of_gross(GROSS), NET);
    }

    #[test]
    fn test_both_sides_gross_up_inverse() {
        let tax = FotTaxType::BothSides { fee_bps: 300 };
        // net -> gross_up -> net 应回到原值，且是最小解
        let gross = tax.gross_up(NET);
        assert_eq!(tax.net_of_gross(gross), NET);
        assert!(tax.net_of_gross(gross - U256::from(1u8)) < NET);
    }

    #[test]
    fn test_input_taxed_semantics() {
        // 双向：输入侧扣税；单侧：输入侧不扣
        assert!(FotTaxType::BothSides { fee_bps: 300 }.input_taxed());
        assert!(!FotTaxType::FlatRate { fee_bps: 300 }.input_taxed());
    }

    #[test]
    fn test_both_sides_full_fee_returns_zero() {
        let tax = FotTaxType::BothSides { fee_bps: 10_000 };
        assert_eq!(tax.net_of_gross(GROSS), U256::ZERO);
        assert_eq!(tax.gross_up(GROSS), U256::ZERO);
    }

    #[test]
    fn test_both_sides_serde_roundtrip() {
        let tax = FotTaxType::BothSides { fee_bps: 300 };
        let json = serde_json::to_string(&tax).unwrap();
        assert_eq!(json, r#"{"type":"both_sides","fee_bps":300}"#);
        let back: FotTaxType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, tax);
    }

    #[test]
    fn test_registry_roundtrip() {
        let token = Address::repeat_byte(0xab);
        register_fot_token(token, FotTaxType::FlatRate { fee_bps: 300 });
        assert_eq!(fot_tax(token), Some(FotTaxType::FlatRate { fee_bps: 300 }));

        let mut t = Token::new_with_decimals(token, 18);
        apply_to_token(&mut t);
        assert_eq!(t.fot_tax, Some(FotTaxType::FlatRate { fee_bps: 300 }));

        // 已有显式标注时 registry 不覆盖
        let mut t2 = Token {
            fot_tax: Some(FotTaxType::FlatRate { fee_bps: 500 }),
            ..Token::new_with_decimals(token, 18)
        };
        apply_to_token(&mut t2);
        assert_eq!(t2.fot_tax, Some(FotTaxType::FlatRate { fee_bps: 500 }));
    }

    #[test]
    fn test_serde_roundtrip() {
        let tax = FotTaxType::FlatRate { fee_bps: 300 };
        let json = serde_json::to_string(&tax).unwrap();
        assert_eq!(json, r#"{"type":"flat_rate","fee_bps":300}"#);
        let back: FotTaxType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, tax);
    }

    #[test]
    fn test_buy_sell_directional_math() {
        // 买卖税率分离：买 2%、卖 5%
        let tax = FotTaxType::BuySell {
            buy_fee_bps: 200,
            sell_fee_bps: 500,
            pairs: vec![Address::repeat_byte(0x01), Address::repeat_byte(0x03)],
            swap_back_threshold: U256::from(1250u64) * U256::from(18),
        };
        // 输出侧（用户买）：扣 buy 2%
        assert_eq!(tax.output_net(GROSS), GROSS * uint!(9800_U256) / uint!(10000_U256) + uint!(1_U256));
        // 输入侧（用户卖）：扣 sell 5%
        assert_eq!(tax.input_net(GROSS), GROSS * uint!(9500_U256) / uint!(10000_U256) + uint!(1_U256));
        // gross-up 往返
        let buy_gross = tax.output_gross_up(NET);
        assert_eq!(tax.output_net(buy_gross), NET);
        assert!(tax.output_net(buy_gross - U256::from(1u8)) < NET);
        let sell_gross = tax.input_gross_up(NET);
        assert_eq!(tax.input_net(sell_gross), NET);
        // 白名单：pairs 集合内生效，其余不生效
        assert!(tax.applies_to_pool(Address::repeat_byte(0x01)));
        assert!(tax.applies_to_pool(Address::repeat_byte(0x03)));
        assert!(!tax.applies_to_pool(Address::repeat_byte(0x02)));
        // net_of_gross 兼容语义 = buy 侧
        assert_eq!(tax.net_of_gross(GROSS), tax.output_net(GROSS));
    }

    #[test]
    fn test_buy_sell_serde_roundtrip() {
        let tax = FotTaxType::BuySell {
            buy_fee_bps: 300,
            sell_fee_bps: 300,
            pairs: vec![Address::repeat_byte(0xb8)],
            swap_back_threshold: U256::from(1250u64) * U256::from(18),
        };
        let json = serde_json::to_string(&tax).unwrap();
        let back: FotTaxType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, tax);
        assert!(back.input_taxed());

        // 旧 pair 格式不再被识别为白名单：未知字段被 serde 忽略，
        // pairs 缺省为空 = 任何池都不扣税（静默失效，勿回退到该格式）
        let old_json = format!(
            r#"{{"type":"buy_sell","buy_fee_bps":300,"sell_fee_bps":300,"pair":"0x{}","swap_back_threshold":"22500"}}"#,
            "b8".repeat(40)
        );
        let old: FotTaxType = serde_json::from_str(&old_json).unwrap();
        match &old {
            FotTaxType::BuySell { pairs, .. } => assert!(pairs.is_empty()),
            _ => panic!("expected BuySell"),
        }
        assert!(!old.applies_to_pool(Address::repeat_byte(0xb8)));
    }

    #[test]
    fn test_swap_back_balance_cache() {
        let token = Address::repeat_byte(0xcd);
        // 未读到返回 0（保守不触发）
        assert_eq!(swap_back_balance(token), U256::ZERO);
        set_swap_back_balance(token, U256::from(1000u32));
        assert_eq!(swap_back_balance(token), U256::from(1000u32));
    }

    // ===== swapBack 事件驱动同步（2026-08-11 起） =====

    /// 注册两个 BuySell token：一个有限阈值（监控），一个 threshold=MAX（不监控，如 XLS）
    fn register_monitor_test_tokens() {
        register_fot_token(
            Address::repeat_byte(0x51),
            FotTaxType::BuySell {
                buy_fee_bps: 300,
                sell_fee_bps: 300,
                pairs: vec![Address::repeat_byte(0x52)],
                swap_back_threshold: U256::from(1250u64)
                    * U256::from(10u64).pow(U256::from(18)),
            },
        );
        register_fot_token(
            Address::repeat_byte(0x53),
            FotTaxType::BuySell {
                buy_fee_bps: 0,
                sell_fee_bps: 300,
                pairs: vec![Address::repeat_byte(0x52)],
                swap_back_threshold: U256::MAX,
            },
        );
    }

    #[test]
    fn test_swap_back_monitored_filter() {
        register_monitor_test_tokens();
        let monitored = swap_back_monitored_tokens();
        assert!(monitored.contains(&Address::repeat_byte(0x51)));
        assert!(!monitored.contains(&Address::repeat_byte(0x53)));
        assert!(is_swap_back_monitored(Address::repeat_byte(0x51)));
        assert!(!is_swap_back_monitored(Address::repeat_byte(0x53)));
        // 未注册地址不监控
        assert!(!is_swap_back_monitored(Address::repeat_byte(0x99)));
    }

    #[test]
    fn test_swap_back_snapshot_plus_events() {
        // 快照 @100 + 序列事件：税收入 +=v、dump 归零
        let token = Address::repeat_byte(0x55);
        let pool = Address::repeat_byte(0x56);
        init_swap_back_balance_snapshot(token, U256::from(1000u32), 100);
        // sell 税：Transfer(用户 → token 合约) → +v
        apply_swap_back_transfer(
            token,
            Address::repeat_byte(0x01),
            token,
            U256::from(50u32),
            101,
            0,
            0,
        );
        assert_eq!(swap_back_balance(token), U256::from(1050u32));
        // buy 税：Transfer(主池 → token 合约) → +v
        apply_swap_back_transfer(token, pool, token, U256::from(30u32), 102, 0, 0);
        assert_eq!(swap_back_balance(token), U256::from(1080u32));
        // swapBack dump：Transfer(token 合约 → 主池) → =0（非减法）
        apply_swap_back_transfer(token, token, pool, U256::from(1080u32), 103, 0, 0);
        assert_eq!(swap_back_balance(token), U256::ZERO);
        // dump 后再进税 → 从 0 重新累加
        apply_swap_back_transfer(
            token,
            Address::repeat_byte(0x02),
            token,
            U256::from(7u32),
            104,
            0,
            0,
        );
        assert_eq!(swap_back_balance(token), U256::from(7u32));
    }

    #[test]
    fn test_swap_back_watermark_drops_replays() {
        let token = Address::repeat_byte(0x57);
        init_swap_back_balance_snapshot(token, U256::from(1000u32), 100);
        // 快照水位 = (100, MAX, MAX)：<= 该位置的事件丢弃（快照块事件重叠 /
        // backfill-实时边界防重）
        apply_swap_back_transfer(token, Address::repeat_byte(0x01), token, U256::from(50u32), 100, 0, 0);
        apply_swap_back_transfer(token, Address::repeat_byte(0x01), token, U256::from(60u32), 99, 0, 0);
        assert_eq!(swap_back_balance(token), U256::from(1000u32));
        // (101,0,0) 应用后，完全相同位置重放被丢弃（断线重连重放同一事件）
        apply_swap_back_transfer(token, Address::repeat_byte(0x01), token, U256::from(40u32), 101, 0, 0);
        assert_eq!(swap_back_balance(token), U256::from(1040u32));
        apply_swap_back_transfer(token, Address::repeat_byte(0x01), token, U256::from(40u32), 101, 0, 0);
        assert_eq!(swap_back_balance(token), U256::from(1040u32));
    }

    #[test]
    fn test_swap_back_same_block_multiple_events_all_applied() {
        // 水位 bug 回归（67617009 取证）：同块内多条 Transfer 必须全部按
        // (block, txIndex, logIndex) 位置序应用；块粒度水位会把同块第二条
        // 及以后事件误丢 → 竞争者的 dump 归零失效，余额停在错误值。
        let token = Address::repeat_byte(0x5a);
        let pool = Address::repeat_byte(0x5b);
        init_swap_back_balance_snapshot(token, U256::from(1000u32), 100);
        // 块 101 内 3 条事件（对应 67617009：tx14 税入 → tx15 dump → tx15 税入）
        // (101, tx14, log0)：sell 税入 +50
        apply_swap_back_transfer(
            token,
            Address::repeat_byte(0x01),
            token,
            U256::from(50u32),
            101,
            14,
            0,
        );
        assert_eq!(swap_back_balance(token), U256::from(1050u32));
        // (101, tx15, log0)：dump 归零（旧块粒度水位下被误丢）
        apply_swap_back_transfer(token, token, pool, U256::from(1050u32), 101, 15, 0);
        assert_eq!(swap_back_balance(token), U256::ZERO);
        // (101, tx15, log1)：dump 后税入 +7（同一 tx 内后续事件）
        apply_swap_back_transfer(token, pool, token, U256::from(7u32), 101, 15, 1);
        assert_eq!(swap_back_balance(token), U256::from(7u32));
        // 完全相同位置重放 → 丢弃
        apply_swap_back_transfer(token, pool, token, U256::from(7u32), 101, 15, 1);
        assert_eq!(swap_back_balance(token), U256::from(7u32));
        // 更早位置乱序重放 → 丢弃（水位单调）
        apply_swap_back_transfer(
            token,
            Address::repeat_byte(0x01),
            token,
            U256::from(50u32),
            101,
            14,
            0,
        );
        assert_eq!(swap_back_balance(token), U256::from(7u32));
        // 下一块事件正常应用
        apply_swap_back_transfer(token, Address::repeat_byte(0x01), token, U256::from(3u32), 102, 0, 0);
        assert_eq!(swap_back_balance(token), U256::from(10u32));
    }

    /// 构造一条监控 token 的 Transfer 日志（RPC JSON 格式，位置参数可配）
    fn mk_transfer_log(
        token: Address,
        from: Address,
        to: Address,
        value: u64,
        block: u64,
        tx_index: u64,
        log_index: u64,
    ) -> alloy::rpc::types::Log {
        // topics 是 32 字节 B256：from/to 地址需左填充到 64 hex
        let pad = |a: Address| format!("0x{:0>64}", format!("{a:x}"));
        serde_json::from_value(serde_json::json!({
            "address": format!("{token:#x}"),
            "topics": [
                format!("{ERC20_TRANSFER_SIG:#x}"),
                pad(from),
                pad(to),
            ],
            "data": format!("0x{:064x}", value),
            "blockNumber": format!("{block:#x}"),
            "transactionIndex": format!("{tx_index:#x}"),
            "logIndex": format!("{log_index:#x}"),
        }))
        .unwrap()
    }

    #[test]
    fn test_apply_swap_back_transfer_log_api() {
        // 统一 Log API（state_space FoT 分支与回放脚本共用）：
        // 过滤 + 提取 + 位置水位，等价手写 7 参调用
        let token = Address::repeat_byte(0x5c);
        let pool = Address::repeat_byte(0x5d);
        let user = Address::repeat_byte(0x5e);
        // 未注册地址的 Transfer → 不过滤不应用
        let unmonitored = mk_transfer_log(user, user, user, 1, 101, 0, 0);
        assert!(!is_swap_back_transfer_log(&unmonitored));
        assert!(!apply_swap_back_transfer_log(&unmonitored, 101));
        // 注册监控后：同块多事件（税入 + dump 归零 + 税入）全部应用
        register_fot_token(
            token,
            FotTaxType::BuySell {
                buy_fee_bps: 300,
                sell_fee_bps: 300,
                pairs: vec![pool],
                swap_back_threshold: U256::from(1250u64),
            },
        );
        init_swap_back_balance_snapshot(token, U256::from(1000u32), 100);
        let tax_in = mk_transfer_log(token, pool, token, 50, 101, 14, 0);
        assert!(is_swap_back_transfer_log(&tax_in));
        assert!(apply_swap_back_transfer_log(&tax_in, 101));
        assert_eq!(swap_back_balance(token), U256::from(1050u32));
        let dump = mk_transfer_log(token, token, pool, 1050, 101, 15, 0);
        assert!(apply_swap_back_transfer_log(&dump, 101));
        assert_eq!(swap_back_balance(token), U256::ZERO);
        let tax_in2 = mk_transfer_log(token, pool, token, 7, 101, 15, 1);
        assert!(apply_swap_back_transfer_log(&tax_in2, 101));
        assert_eq!(swap_back_balance(token), U256::from(7u32));
        // 完全同位置重放 → 丢弃
        assert!(apply_swap_back_transfer_log(&tax_in2, 101));
        assert_eq!(swap_back_balance(token), U256::from(7u32));
    }

    #[test]
    fn test_swap_back_dump_zeroes_drift() {
        // 事故场景（67598082）：1s 轮询 stale 缓存 32000 远小于链上 117250；
        // 事件驱动下，无论此前缓存如何漂移，链上 dump 事件 → 归零 = 强制对齐
        let token = Address::repeat_byte(0x58);
        init_swap_back_balance_snapshot(token, U256::from(32000u32), 100);
        set_swap_back_balance(token, U256::from(32000u32)); // 模拟漂移基线
        apply_swap_back_transfer(
            token,
            token,
            Address::repeat_byte(0x02),
            U256::from(117250u32),
            200,
            0,
            0,
        );
        assert_eq!(swap_back_balance(token), U256::ZERO);
    }

    #[test]
    fn test_swap_back_apply_uninitialized_token() {
        // 未快照 token 的事件：or_insert 0 起点累加（防御性，不应 panic）
        let token = Address::repeat_byte(0x59);
        apply_swap_back_transfer(token, Address::repeat_byte(0x01), token, U256::from(77u32), 5, 0, 0);
        assert_eq!(swap_back_balance(token), U256::from(77u32));
        apply_swap_back_transfer(token, token, Address::repeat_byte(0x02), U256::from(1u32), 6, 0, 0);
        assert_eq!(swap_back_balance(token), U256::ZERO);
    }
}

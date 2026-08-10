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
//!     [`swap_back_balance`] 周期任务 1s 刷新）>= `swap_back_threshold`（1250e18），
//!     先以合约全部累积余额砸入主池（`swapping` 豁免不扣税，池子实收全额），
//!     换出的另一侧全额给分红分发器（不参与用户路径），再算用户主 swap
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
//!     swapBack 预交易（余额来自 [`swap_back_balance`] 缓存，
//!     由周期任务 1s 刷新；未读到按 0 = 不触发处理）
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

use alloy::primitives::{Address, U256};
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
/// 无法从池子事件流同步（与池子无关），由周期任务约 1s 刷新一次。
///
/// 缓存携带刷新时刻（`Instant`），供模拟层判断数据新鲜度：
/// 刷新任务若挂起/连续失败（如 RPC 断连），stale 值会显著放大模拟误差
/// （见 DYOR:K 事故：cache 84s 未刷新，模拟 dump 量 9 倍小于实际）。
static FOT_SWAP_BACK_BALANCES: OnceLock<RwLock<HashMap<Address, (U256, Instant)>>> =
    OnceLock::new();

fn swap_back_balances() -> &'static RwLock<HashMap<Address, (U256, Instant)>> {
    FOT_SWAP_BACK_BALANCES.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 更新 token 合约自身持有余额（周期任务调用）
pub fn set_swap_back_balance(token: Address, balance: U256) {
    swap_back_balances()
        .write()
        .unwrap()
        .insert(token, (balance, Instant::now()));
}

/// 读取 token 合约自身持有余额（模拟层调用）
///
/// 未读到（周期任务尚未首跑）返回 0 = 视为不触发 swapBack（保守：
/// 宁可高估输出，也避免在余额未知时凭空砸盘）。
pub fn swap_back_balance(token: Address) -> U256 {
    swap_back_balances()
        .read()
        .unwrap()
        .get(&token)
        .map(|(b, _)| *b)
        .unwrap_or(U256::ZERO)
}

/// 读取 token 合约自身持有余额 + 最后刷新时刻
///
/// 未读到返回 `None`（周期任务尚未首跑）。
pub fn swap_back_balance_with_refresh(token: Address) -> Option<(U256, Instant)> {
    swap_back_balances()
        .read()
        .unwrap()
        .get(&token)
        .copied()
}

/// 缓存数据已 stale 的判定阈值：超过该时长未刷新视为不可信。
///
/// 刷新任务 1s 周期 + 5s 调用超时，正常场景 staleness 应在秒级；
/// 放宽到 30s 以容忍偶发重试窗口，超过即视为刷新链路中断。
pub const SWAP_BACK_BALANCE_STALE_AFTER: Duration = Duration::from_secs(30);

/// 缓存是否 stale（最后刷新距今超过 [`SWAP_BACK_BALANCE_STALE_AFTER`] 或从未刷新）
pub fn swap_back_balance_is_stale(token: Address) -> bool {
    match swap_back_balance_with_refresh(token) {
        Some((_, refreshed_at)) => refreshed_at.elapsed() > SWAP_BACK_BALANCE_STALE_AFTER,
        None => true,
    }
}

/// 所有已注册 BuySell 的 token 地址列表（周期任务遍历用）
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
}

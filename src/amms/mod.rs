use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::Arc,
};

use alloy::{
    dyn_abi::DynSolType,
    network::Network,
    primitives::{Address, U256},
    providers::Provider,
    sol,
};
use error::{AMMError, BatchContractError};
use futures::{stream::FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};

pub mod aerodrome_slipstream;
pub mod aerodrome_v2;
pub mod algebra_integral;
pub mod amm;
pub mod balancer_v2;
pub mod balancer_v3;
pub mod binaryfi_prop;
pub mod caliber_prop;
pub mod consts;
pub mod curve_legacy;
pub mod curve_ng;
pub mod ekubo;
pub mod elfomo_prop;
pub mod erc_4626;
pub mod error;
pub mod factory;
pub mod fermi_prop;
pub mod float;
pub mod fluid_dex;
pub mod fot;
pub mod pancake_infinity;
pub mod pancake_v2;
pub mod pancake_v3;
pub mod pendle;
pub mod rocketpool;
pub mod sky;
pub mod sushi_v2;
pub mod tick_math_cache;
pub mod uniswap_v2;
pub mod uniswap_v3;
pub mod uniswap_v4;

sol! {
    #[sol(rpc)]
    GetTokenDecimalsBatchRequest,
    "src/amms/abi/GetTokenDecimalsBatchRequest.json",
}

sol!(
#[derive(Debug, PartialEq, Eq)]
#[sol(rpc)]
contract IERC20 {
    function decimals() external view returns (uint8);
    function symbol() external view returns (string memory);
});

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Token {
    pub address: Address,
    pub decimals: u8,
    pub symbol: String,
    pub chain_id: u64,
    /// FoT（fee-on-transfer）税率，`None` 表示普通 token。
    ///
    /// 不会自动检测，需在初始化阶段通过 `fot::register_fot_token` 注入，
    /// 或直接在序列化数据中标注（显式标注优先级更高）。
    ///
    /// 变体语义（链上取证，见 `fot` 模块文档扣税档案）：
    /// - `FotTaxType::FlatRate`：**池子转出该 token 时扣税**（单侧，输出侧），
    ///   模拟输出返回 net、exact-out 输入需 gross-up；to = pool（卖出）方向不扣税。
    /// - `FotTaxType::BothSides`：**每次 transfer 都扣税**（输入侧 + 输出侧），
    ///   模拟层 hop 链中一次 transfer 的税由 hop N 输出侧 `fot_net` 捕获，
    ///   输入侧不重复扣税；仅引擎层起点转账（flash→池）用 `fot_input_net`
    ///   折算池子实收净额，输出侧同 FlatRate。
    /// - `FotTaxType::BuySell`：**仅白名单池（`pairs` 集合）生效**的买卖分离税，
    ///   卖进池扣 `sell_fee_bps`、买出池扣 `buy_fee_bps`（RTX/XLS 实例见档案）。
    #[serde(default)]
    pub fot_tax: Option<fot::FotTaxType>,
}

impl Token {
    pub async fn new<N, P>(address: Address, provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let token = Arc::new(IERC20::new(address, provider.clone()));
        let decimals = token.decimals().call().await?;
        let symbol = token.symbol().call().await?;

        Ok(Self {
            address,
            decimals,
            chain_id: provider.get_chain_id().await?,
            symbol,
            fot_tax: None,
        })
    }

    pub const fn new_with_decimals(address: Address, decimals: u8) -> Self {
        Self {
            address,
            decimals,
            symbol: String::new(),
            chain_id: 0,
            fot_tax: None,
        }
    }

    pub const fn address(&self) -> &Address {
        &self.address
    }

    /// 扣除 FoT 税后实际到手金额（net）。无 FoT 时返回原值。
    ///
    /// 链上语义：池子 swap math 输出 gross，transfer hook 扣税后接收方到手 net。
    /// 输出侧方向：[`fot::FotTaxType::BuySell`] 扣 `buy_fee_bps`。
    /// 仅池子对该 token 的税种生效时扣税，见 [`Token::fot_net_for`]。
    pub fn fot_net(&self, gross: U256) -> U256 {
        match &self.fot_tax {
            Some(tax) => tax.output_net(gross),
            None => gross,
        }
    }

    /// 反算：接收方到手 `net` 所需的 gross 金额（向上取整）。无 FoT 时返回原值。
    ///
    /// 用于 exact-out 场景：池子 math 必须先输出 gross，transfer 扣税后
    /// 接收方才能拿到 net。
    pub fn fot_gross_up(&self, net: U256) -> U256 {
        match &self.fot_tax {
            Some(tax) => tax.output_gross_up(net),
            None => net,
        }
    }

    /// 输入侧（user→pool）实收净额。仅双向/买卖分离 FoT 按税率扣，
    /// 单侧 FoT 与普通 token 全额入池。
    ///
    /// 链上语义：池子实收 net，K 检查按 balanceOf 差值（net）记账，
    /// swap math 的 amountIn 也是 net。
    ///
    /// amms 模拟层 pool 内部使用带池过滤的 [`Token::fot_input_net_for`] 执行
    /// 输入侧扣税（hop 链中 hop N+1 的输入 = hop N 输出 `fot_net` 实收值，
    /// 作为名义再被扣一次——链上引擎中转每 hop 一次 transfer 各扣一次税，
    /// 0.97 × 0.97 是链上真实语义，不是双重扣税）。
    /// 本方法（无池过滤，BuySell 不检查池白名单）仅供引擎层起点转账场景
    /// （flash→池）使用：用户需转名义 gross，池子实收本方法返回值后按此
    /// 净额入池并参与 math。
    pub fn fot_input_net(&self, gross: U256) -> U256 {
        match &self.fot_tax {
            Some(tax) => tax.input_net(gross),
            None => gross,
        }
    }

    /// 输入侧反算：池子需实收 `net` 时，用户需转的名义金额（向上取整）。
    ///
    /// 仅双向/买卖分离 FoT 时 gross-up，单侧 FoT 与普通 token 返回原值。
    /// 用于 exact-out 起点转账场景：`get_amount_in` 返回池子实收需求，
    /// 双向 FoT 下用户转账需多转以覆盖输入侧扣税。
    /// 模拟层 pool 内部使用带池过滤的 [`Token::fot_input_gross_up_for`]。
    pub fn fot_input_gross_up(&self, net: U256) -> U256 {
        match &self.fot_tax {
            Some(tax) => tax.input_gross_up(net),
            None => net,
        }
    }

    /// 输出侧净额，**仅当税种对该池生效时扣税**。
    ///
    /// [`fot::FotTaxType::BuySell`] 只在白名单池（`pairs` 集合）扣税：
    /// 其他池子（如 V4 PoolManager）与该 token 的 transfer 不扣税，
    /// 返回原值。其余税种对所有池生效，等价 [`Token::fot_net`]。
    pub fn fot_net_for(&self, pool: Address, gross: U256) -> U256 {
        match &self.fot_tax {
            Some(tax) if tax.applies_to_pool(pool) => tax.output_net(gross),
            _ => gross,
        }
    }

    /// 输出侧反算（池子过滤版），语义同 [`Token::fot_net_for`]。
    pub fn fot_gross_up_for(&self, pool: Address, net: U256) -> U256 {
        match &self.fot_tax {
            Some(tax) if tax.applies_to_pool(pool) => tax.output_gross_up(net),
            _ => net,
        }
    }

    /// 输入侧实收净额（池子过滤版），语义同 [`Token::fot_net_for`]。
    pub fn fot_input_net_for(&self, pool: Address, gross: U256) -> U256 {
        match &self.fot_tax {
            Some(tax) if tax.applies_to_pool(pool) => tax.input_net(gross),
            _ => gross,
        }
    }

    /// 输入侧反算（池子过滤版），语义同 [`Token::fot_net_for`]。
    pub fn fot_input_gross_up_for(&self, pool: Address, net: U256) -> U256 {
        match &self.fot_tax {
            Some(tax) if tax.applies_to_pool(pool) => tax.input_gross_up(net),
            _ => net,
        }
    }

    pub const fn decimals(&self) -> u8 {
        self.decimals
    }

    /// Checks if the provided reserve amount is considered sufficient liquidity for this token.
    /// This helps filter out "dust" or "zombie" pools.
    pub fn has_sufficient_liquidity(&self, reserve: u128) -> bool {
        let symbol = self.symbol.to_uppercase();

        // 1. Check for known high-value base assets
        if symbol == "WETH" || symbol == "WBNB" || symbol == "ETH" || symbol == "BNB" {
            // Require at least 0.1 ETH/BNB (~$300)
            // 0.1 * 10^18 = 100_000_000_000_000_000
            return reserve >= 100_000_000_000_000_000;
        }
        if symbol == "WBTC" || symbol == "BTC" || symbol == "CBTC" {
            // Require at least 0.005 BTC (~$300)
            // 0.005 * 10^8 = 500_000
            return reserve >= 500_000;
        }
        if symbol == "USDC" || symbol == "USDT" || symbol == "DAI" {
            // Require at least 300 USD
            // 300 * 10^decimals
            // For 6 decimals: 300 * 10^6 = 300_000_000
            // For 18 decimals: 300 * 10^18
            let threshold = 300u128.saturating_mul(10u128.pow(self.decimals as u32));
            return reserve >= threshold;
        }

        // 2. Generic check for other tokens based on decimals
        if self.decimals >= 18 {
            // 0.0001 unit (e.g. 10^14 wei)
            reserve >= 10u128.pow(self.decimals as u32 - 4)
        } else if self.decimals >= 6 {
            // 100 units (e.g. 100 * 10^6 = 10^8)
            let threshold = 100u128.saturating_mul(10u128.pow(self.decimals as u32));
            reserve >= threshold
        } else {
            // Fallback for very low decimals
            reserve >= 100_000
        }
    }
}

impl From<Address> for Token {
    fn from(address: Address) -> Self {
        Self {
            address,
            decimals: 0,
            ..Default::default()
        }
    }
}

impl Hash for Token {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.address.hash(state);
    }
}

/// Fetches the decimal precision for a list of ERC-20 tokens.
///
/// # Returns
/// A map of token addresses to their decimal precision.
pub async fn get_token_decimals<N, P>(
    tokens: Vec<Address>,
    provider: P,
) -> Result<HashMap<Address, u8>, BatchContractError>
where
    N: Network,
    P: Provider<N> + Clone + Clone,
{
    let mut token_decimals = HashMap::new();

    // Filter out Address::ZERO (Native ETH) and set decimals to 18
    let eth_placeholder: Address = "0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE"
        .parse()
        .unwrap();
    let tokens_to_fetch: Vec<Address> = tokens
        .into_iter()
        .filter(|&t| {
            if t.is_zero() || t == eth_placeholder {
                token_decimals.insert(t, 18);
                false
            } else {
                true
            }
        })
        .collect();

    let step = 765;

    let mut futures = FuturesUnordered::new();
    tokens_to_fetch.chunks(step).for_each(|group| {
        let provider = provider.clone();

        futures.push(async move {
            (
                group,
                GetTokenDecimalsBatchRequest::deploy_builder(provider, group.to_vec())
                    .call_raw()
                    .await,
            )
        });
    });

    let return_type = DynSolType::Array(Box::new(DynSolType::Uint(8)));

    while let Some(res) = futures.next().await {
        let (token_addresses, return_data) = res;

        let return_data = return_type.abi_decode_sequence(&return_data?)?;

        if let Some(tokens_arr) = return_data.as_array() {
            for (decimals, token_address) in tokens_arr.iter().zip(token_addresses.iter()) {
                token_decimals.insert(
                    *token_address,
                    decimals.as_uint().expect("Could not get uint").0.to::<u8>(),
                );
            }
        }
    }
    Ok(token_decimals)
}

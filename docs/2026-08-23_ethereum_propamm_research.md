# Ethereum PropAMM 协议调研(2026-08-23)

> 目的:评估在 amms-rs 中集成 Ethereum PropAMM(Proprietary AMM)的协议格局、接口与实施方案。
> 所有主网地址均已通过 `eth_getCode` 核验在线;Titan state-overrides RPC 已验证有实时返回。

## 1. 什么是 PropAMM

PropAMM(Proprietary AMM,专业做市商 AMM)由单一专业做市商用自有资金做市,把 CEX 价格/私有
定价模型高频写入链上合约存储,替代传统 `x*y=k` 等被动定价曲线。

- 起源:Solana(Lifinity → SolFi/ZeroFi/Obric → HumidiFi/BisonFi),Solana 上 PropAMM 一度占
  DEX 成交量 ~70%。
- 2026-05 起经 Titan Builder 的 ACE(Application-Controlled Execution)机制移植到 Ethereum 主网。
- 核心价值:报价新鲜(每区块多次更新)、深度集中在"下一笔交易发生处"、对聚合器/求解器可组合
  (仍是普通链上池子,可进 swap path 与套利 bundle)。

## 2. Ethereum 主网现状(截至 2026-08-23)

| 时间 | 数据 |
|---|---|
| 2026-08-20 | PropAMM 日交易量首次破 $1 亿,约占 Ethereum 现货 DEX 量 10%;FermiSwap + Metric 占主要份额(吴说/Titan 数据) |
| 2026-07 | LambdaClass 统计累计 $5.46 亿+/6.8 万+ swaps;主流对(BTC/ETH/stables)部分报价优于 Binance |
| 2026-06-01 | KyberSwap 成为首家接入的 DEX 聚合器(WETH/USDT、WETH/USDC) |
| 持续 | Titan 构建 >50% 区块;Quasar 亦提供同类服务;FOCIL(Heogota)为未来协议级包含保证 |

## 3. 主网上线协议清单

统一入口(Titan/LambdaClass PropAMMRouter,开源、零手续费,自动选最优 + Uniswap V3 兜底):

- `0x4DdF368080CD7946db5b459aD591c350158175e1`(proxy,已验证在线)

Titan blocks 内 live 的 venue:

| 协议 | Router / 报价目标 | Oracle / stream 地址 | 备注 |
|---|---|---|---|
| FermiSwap | `0x5979458912F80B96d30D4220af8E2e4925A33320` | `0x26e5A56f807d4C937B0b815266B135F09B4Bf312`;流:`0xb1076fe3ab5e28005c7c323bac5ac06a680d452e` | 当前最大份额;实现 IPropAMM |
| Metric | — | `0x1e266e7bD2CD8597171Df7f3d36EbdbC7EE53E91` | 份额第二梯队;Fynd 已路由 |
| Kipseli | `0x71e790dd841c8A9061487cb3E78C288E75cE0B3d` | `0xFe3D12b21D2602868223E83149BdbbFb5d11e185` | 无链上 quote 函数,需 revert 模拟(`simulateKipseliSwap`)+ balance override |
| bopAMM(Bebop) | `0xB09AaA5614916d7AEb59C295C52c92ca82aDdD76` | `0xB0999914B3DE1be58Ef2416af09Bd2E7F8AaD03C` | 报价来自单一 registry slot;空快照需清零默认 slot |
| Tempest | `0x00000003f1EC2379e79F58E12eC6c4F51ee92149` | 同左(proxy,130B) | 流可能需权限 |
| TaurusFi | `0x217D58931a8549Ca539426aA8152E33dAFc3D95a` | 同左 | 已上线 |

值得关注但不在 Ethereum 主网:`LunarBase`(Base,Tycho 已支持)、`Tessera V`(Wintermute,Base)、
`LFJ POE`(Monad)、`GeniusFi`(BNB)、`Spire/BaiBai`(Base,PropAMM+聚合器)。

标准层面:ERC-8324 Priority Update Registry(PUR,草案)以单一共享合约承载 top-of-block 更新,
是未来的通用接口方向。

## 4. 链上架构与接口

组件:`Oracle`(报价 + 有效区块号)→ `Router`(入口)→ `Swapper`(新鲜度/规模调价/优先费反抢跑)
→ `PriceAdjuster`(按规模修正,含上限)→ `Vault`(Safe 多签持币)。
调用流:`Taker → Router → Swapper → Oracle → PriceAdjuster → Safe`。

统一接口 `IPropAMM`(lambdaclass/propamm-router-contracts):

- `isActive(tokenIn, tokenOut) -> bool`
- `getPairs() -> TokenPair[]`(token0 < token1 规范排序)
- `quote(tokenIn, tokenOut, amountIn) -> amountOut`(必须是 fresh 状态)
- `swap(tokenIn, tokenOut, amountIn, minAmountOut, recipient, deadline)`(push-payment:先转账后 swap)
- 事件:`Swapped(sender, tokenIn, tokenOut, amountIn, amountOut, recipient)`

安全语义(对 searcher 重要):

- 报价有新鲜度窗口,仅在其指定 slot 内有效;过期报价 revert。
- Swapper 比较 `tx.gasprice - block.basefee` 与阈值,异常高优先费交易的输出置零(链上防抢跑)。

## 5. Titan 数据流(拿到"新鲜状态"的关键)

裸链上状态是过时的(做市商报价走私有通道,不落公共内存池),必须消费 Titan 提供的 taker 流:

### 5.1 State overrides 流

- WS:`wss://{eu|ap|us}.rpc.titanbuilder.xyz/ws/pamm_quote_stream`
- JSON-RPC:`titan_getPammStateOverrides` @ `https://{region}.rpc.titanbuilder.xyz/data`
- 格式:扁平化 `stateOverride`(balance/nonce/stateDiff),可直接作为 `eth_call`/`eth_simulateV1`
  第三参数;模拟需按 beacon slot 覆盖 block/timestamp(`1606824023 + slot*12`)。
- 公开无鉴权流目前含 Fermi/Kipseli/bopAMM;Tempest/TaurusFi 可能需权限。

### 5.2 Price levels 流

- WS:`wss://{region}.rpc.titanbuilder.xyz/ws/pamm_price_levels` / `titan_getPammPriceLevels`
- 格式:每 pAMM/pair 一个 `orderBook` 梯子,`amountIn → amountOut`;`Simulated`(EVM 实测,
  几何级数分布尺寸)+ `Interpolated`(线性样条插值)。与 binaryfi_prop 的 BUY/SELL ladder 同构。
- 每条消息是全量快照,消费方只保留最新。

### 5.3 多候选 Bundle 提交

- 同一笔交易可提交 N 个候选 bundle(纯 CFMM 路径 / 含 pAMM hop 的路径)。
- Titan 在构建时对每个候选按最新 pAMM 状态评估,并在每次报价更新时重评估,直到入块前一刻,
  选最优者包含。

## 6. Taker/Searcher 接入方式

Taker 侧标准工作流(全部基于 Titan 公开接口,与具体做市商无关):

1. 订阅 `pamm_quote_stream`(state overrides)与 `pamm_price_levels`,本地维护"最新可成交价";
2. 模拟器用最新 overrides 评估 pAMM 路径(`eth_call`/`eth_simulateV1` 第三参数),
   同时覆盖 block/timestamp(beacon slot 推导);
3. 同一笔交易构造多个候选 bundle(pAMM-only、CFMM-only、混合 hop),提交 Titan;
4. Titan 在构建时对每个候选按最新 pAMM 状态重评估,每次报价更新都会触发重评估直到入块,
   择优包含;报价更新交易由 ACE 保证排在吃单交易之前。

关键约束(合约层,searcher 必须遵守):

- 报价仅在指定 slot 内有效,过期即 revert;
- 高 priority fee 交易输出置零(反抢跑),不能用加价插队;
- 做市商可配置 freshness protection,只允许对足够新的报价成交。

## 7. amms-rs 集成建议

**结论:必须逐家逆向 + 实现,与 XLayer BinaryFi/Caliber 同款模式。**
PropAMMRouter 只是"执行/路由入口"(统一调用面 + 入块时择优 + Uniswap V3 兜底),
它不提供本地建模所需的余额、梯度、参数与新鲜状态;本地精确模拟仍须逐家完成。
Ethereum 上采用**双数据源**模型:

### 数据源 A:Titan 流(实时报价状态)

- `pamm_quote_stream`(state overrides:价格槽位 + 余额)+ `pamm_price_levels`(离散报价梯子);
- 每条消息是**完整快照,最新者胜**;断线只能回到"当前最新快照",**无历史、不可回放**;
- 流是 maker opt-in + 节流/延迟配置,不一定能看到每次 sub-block 更新;部分 venue
  (Tempest/TaurusFi)可能需要权限接入;
- 消息带 `slot`/`blockNumber`,本地状态须以版本号标记并按 slot 判定**新鲜度窗口**(报价仅在其
  所属 slot 内有效,过期 revert)。

### 数据源 B:链上事件(可回放、对账、校准)

- `Swapped`(成交对账)、`updateState`/`batchUpdateStateWithSignature`(报价上链提交,
  仅在有吃单交易时发生,稀疏但可确认)、`PairRegistered`(pair 发现);
- 这部分与 XLayer 事件驱动逻辑完全一致,可用于断线后的状态校准与历史重建(部分)。

### 混合同步模型

```
Titan 流消息 → 覆盖本地新鲜状态(带 slot 版本) → 重算受影响 pool 的 spot/quote → 触发下游变化检测
链上事件     → 余额对账 / pair 发现 / updateState 交叉校准
```

### 实施要点(vs XLayer 差异)

- 精确本地模拟所需:新鲜价格槽位 + 余额来自 Titan 流,PriceAdjuster 等静态参数读链上,
  定价数学本地复刻(逆向/开源);price-levels 梯子(含 Interpolated 插值)仅可作近似;
- 无历史回放 → 本地缓存带版本号,链上 `updateState` 事件做交叉校准;
- 新鲜度窗口语义 → 按 slot 判定有效性(可迁移 Caliber flashblocks 的 slot 经验);
- 模拟入口需支持 `state overrides + block/timestamp override`(现有 simulate_swap 是纯链上状态);
- `spot_price` 语义改为"oracle 中间价 + 新鲜度检查";
- 优先级:Fermi(最大份额)→ bopAMM(Bebop)→ Metric → Kipseli(revert 模拟最特殊)→ 新 venue 观察。

### Router 快速通道(可选,交易/路由层)

把 PropAMMRouter 作为单一 venue 接入,消费 overrides 流(Arc 共享缓存),`simulate_swap` 走
`quoteV1` + state overrides,可快速覆盖全部 venue + Uniswap V3 兜底;但仅适合路由/机会发现,
不满足 AMMS 标准本地精确建模。

风险提示:

- ACE 是 builder 级保证(非协议级),仅在 Titan/Quasar 区块内有效;FOCIL 尚未上线。
- 0x 2026-03 报告:Base 上部分 PropAMM 存在 Flashblock 末尾 ~200ms "诱饵报价"欺诈
  (下单后立即调价,单笔 5-10bps 额外损失)。
- 做市商闭源、不透明;普通用户/LP 无法直接参与。
- searcher:高优先费会被 PropAMM 置零输出,竞拍/套利路径需避开。

## 8. 参考资料

- https://docs.titanbuilder.xyz/propamms(Titan 官方文档,含 takers.md/makers.md)
- https://github.com/lambdaclass/propamm-router-contracts(合约 + Rust/TS/Python SDK)
- https://github.com/propeller-heads/tycho(protocols/substreams: fermiswap/bopamm/lunarbase)
- https://pamm.wtf(实时数据面板)
- https://ethresear.ch/t/proprietary-amms-and-ethereum/25543
- https://webflow.internal.0x.org/post/propamm-shenanigans(0x 报价欺诈报告)
- https://ethereum-magicians.org/t/erc-8324-priority-update-registry-pur/28921(ERC-8324 PUR)

//! 通用 EVM storage 批量读取（不依赖 `eth_getStorageAt`）。
//!
//! ## 背景
//!
//! 官方 XLayer WS 网关（`wss://ws.xlayer.tech` / `wss://xlayerws.okx.com`）对
//! `eth_getStorageAt`、`eth_getProof`、`debug_traceCall` 做**方法白名单**
//! （`-32601 rpc method is not whitelisted`），但一定放行 `eth_call`。而
//! caliber / elfomo 这类 propAMM 的关键状态（ladder 曲线、位置 `pos`、
//! fee/window 等）没有任何 view getter，只能直读合约 storage 槽位——此前只能
//! 绕道硬编码的公共 HTTP 端点，并被其限流（`-32016 over rate limit`）逼出
//! 固定 200ms/批节流与逐槽回退兜底。
//!
//! ## 方案
//!
//! `eth_call` 支持第三个参数 **state override**：只替换地址上的 `code`、
//! **保留 storage**。把下面这段 bulk-SLOAD 运行时注入到目标合约地址、并让
//! `to` 也指向该地址，EVM 就会在它的 storage 上执行，一次调用读回任意多个槽。
//! 语义与逐个 `eth_getStorageAt` **逐位一致**（同一块、同一槽），且走调用方
//! 注入的 provider——官方 WS 即可，无需任何额外端点或节流。
//!
//! 实测（官方 WS `wss://ws.xlayer.tech`）：2 槽 0.07s、200 槽 0.16s、
//! 755 槽 0.29s；取值与 `eth_getStorageAt` 在 latest 与历史块下完全一致。

use alloy::{
    eips::BlockId,
    network::{Network, TransactionBuilder},
    primitives::{Address, Bytes, B256, U256},
    providers::Provider,
    rpc::types::state::{AccountOverride, StateOverride},
};

use super::error::AMMError;

/// bulk-SLOAD 运行时：`calldata` = N×32 字节槽号序列，返回 N×32 字节槽值序列。
///
/// ```text
/// PUSH1 0x00            ; o = 0
/// JUMPDEST              ; loop:
/// DUP1 CALLDATALOAD     ;   slot = calldata[o]
/// SLOAD                 ;   val  = sload(slot)
/// DUP2 MSTORE           ;   memory[o] = val
/// PUSH1 0x20 ADD        ;   o += 32
/// DUP1 CALLDATASIZE GT  ;   o < calldatasize ?
/// PUSH1 0x02 JUMPI      ;   -> loop
/// POP CALLDATASIZE      ; return memory[0..calldatasize]
/// PUSH1 0x00 RETURN
/// ```
const BULK_SLOAD_RUNTIME: &[u8] = &[
    0x60, 0x00, 0x5b, 0x80, 0x35, 0x54, 0x81, 0x52, 0x60, 0x20, 0x01, 0x80, 0x36, 0x11, 0x60, 0x02,
    0x57, 0x50, 0x36, 0x60, 0x00, 0xf3,
];

/// 单次 `eth_call` 读取的槽位上限（控制 gas 与响应体大小）。
pub(crate) const MAX_SLOTS_PER_CALL: usize = 512;

/// 每个槽位预留的 gas（cold SLOAD = 2100，留余量）。
const GAS_PER_SLOT: u64 = 2_500;
/// 调用基础 gas（calldata/内存/RETURN 开销）。
const GAS_BASE: u64 = 100_000;

/// 一次 `eth_call`（code state-override + bulk-SLOAD）读取 `slots` 在 `block`
/// 末的值，返回顺序与 `slots` 一致。
pub(crate) async fn storage_slots_at<N, P>(
    provider: &P,
    address: Address,
    slots: &[B256],
    block: BlockId,
) -> Result<Vec<U256>, AMMError>
where
    N: Network,
    P: Provider<N>,
{
    if slots.is_empty() {
        return Ok(Vec::new());
    }

    let mut data = Vec::with_capacity(slots.len() * 32);
    for slot in slots {
        data.extend_from_slice(slot.as_slice());
    }

    let mut overrides = StateOverride::default();
    overrides.insert(
        address,
        AccountOverride {
            code: Some(Bytes::from_static(BULK_SLOAD_RUNTIME)),
            ..Default::default()
        },
    );

    let gas = GAS_BASE.saturating_add(GAS_PER_SLOT.saturating_mul(slots.len() as u64));
    let tx = N::TransactionRequest::default()
        .with_to(address)
        .with_input(Bytes::from(data))
        .with_gas_limit(gas);

    let out: Bytes = provider
        .call(tx)
        .overrides(overrides)
        .block(block)
        .await
        .map_err(|e| {
            AMMError::Msg(format!(
                "evm_storage: eth_call bulk-SLOAD failed ({address:?} @ {block:?}): {e}"
            ))
        })?;

    if out.len() != slots.len() * 32 {
        return Err(AMMError::Msg(format!(
            "evm_storage: bulk-SLOAD returned {} bytes, expected {}",
            out.len(),
            slots.len() * 32
        )));
    }

    Ok(out.chunks_exact(32).map(U256::from_be_slice).collect())
}

/// 分片版 [`storage_slots_at`]：超过 [`MAX_SLOTS_PER_CALL`] 时拆成多次调用。
pub(crate) async fn storage_slots_at_chunked<N, P>(
    provider: &P,
    address: Address,
    slots: &[B256],
    block: BlockId,
) -> Result<Vec<U256>, AMMError>
where
    N: Network,
    P: Provider<N>,
{
    let mut out = Vec::with_capacity(slots.len());
    for part in slots.chunks(MAX_SLOTS_PER_CALL) {
        out.extend(storage_slots_at::<N, P>(provider, address, part, block).await?);
    }
    Ok(out)
}

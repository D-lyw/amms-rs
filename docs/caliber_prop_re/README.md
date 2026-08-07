# Caliber propAMM 逆向工具链（离线可复现）

目标合约：`0x154586b2479b9a11e3d4db90024dc0e26f097312`（XLayer）。
完整报价逻辑说明见 `../caliber_prop_internal.md`。本目录固化逆向过程中使用的
**字节码、状态快照、EVM 解释器与模型原型**，避免下次重新做耗时的逆向。

## 文件清单

| 文件 | 作用 |
|---|---|
| `caliber_code.bin` | 合约运行时代码（22042 字节，块 67325524 附近抓取，未升级则稳定） |
| `caliber_state_66309105.json` | 4 个 pair 在块 66309105 的真实存储状态（含 pos），模型/调试共用 |
| `fetch_state.py` | 从任意 RPC 抓取任意区块的状态（`cfg+0..7`、`data`、ladder、dec、pos） |
| `verify_pair1_reverse.py` | EVM 解释器：用真实状态跑 pair1 反向 `amount=1`，输出 `526122805`（与链上一致） |
| `evm_gen.py` | 参数化 EVM 解释器：可换 pair/金额，打印 SLOAD / 算术序列 / FULLSTACK 断点，用于追链上每一步 |
| `model.py` | Python 正向/反向报价模型原型：与链上 `eth_call` 逐金额对照，输出 DIFF |

## 依赖

- Python 3 + `pycryptodome`（`pip install pycryptodome`）
- 在线对照（`model.py` 查链）需直连 `https://rpc.xlayer.tech`（脚本内已处理证书）

## 快速复现

```bash
# 1) 解释器级验证：pair1 反向 amount=1 应输出 526122805（离线，读 caliber_code.bin）
python3 verify_pair1_reverse.py

# 2) 模型级对照：4 pair × 双向 × 14 个金额 vs 链上 eth_call（联网）
python3 model.py

# 3) 重抓任意区块状态（联网），替换 model.py 中的块号/状态文件
python3 fetch_state.py --block 66309105 --rpc https://rpc.xlayer.tech --out /tmp/state.pkl

# 4) 追某 pair 某金额的链上算术序列（离线解释器，断点见脚本内 FULLSTACK pc 列表）
python3 evm_gen.py <pairId 64hex>
```

## 当前已知未对齐边界（详见 ../caliber_prop_internal.md §7）

1. pair1（335c…）反向 `w=1e9`：差 ~1.5e12。
2. pair2（d81a…）反向 `w≥1e4`：差 5 → 47421 递增，疑似段内剩余量 + 跨段处理。

这两个是唯一剩余差异，定位方法：`evm_gen.py` 换 pair2 + 金额，对照 `model.py` 的
公式分段输出，找到链上额外/差异算术步骤。

## 合约字节码更新

若合约升级，重新抓取并替换 `caliber_code.bin`（`eth_getCode`），
再跑 `verify_pair1_reverse.py` 确认解释器与链上仍一致。

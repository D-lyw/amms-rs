#!/usr/bin/env python3
"""抓取 Caliber propAMM 在任意区块的状态，供本地模型/解释器复现链上报价。

用法:
    python3 fetch_state.py --block 66309105 \
        --rpc https://rpc.xlayer.tech --out /tmp/caliber_state_66309105.pkl
    python3 fetch_state.py --block 66309105 --rpc http://127.0.0.1:8557 --out /tmp/x.json

输出 pkl（默认）或 json（后缀 .json），结构与 docs/caliber_prop_internal.md 一致：
    每 pair 一个 dict: cfg0..cfg7, data0, data1, n, win, rx, ry,
    fee, field0, field1, pos, dec_x, dec_y, scale, ladder=[(x_i,y_i),...]

存储布局（详见 docs/caliber_prop_internal.md）：
    cfg  = keccak256(pairId || uint256(6))
    data = keccak256(pairId || uint256(7))
    cfg+0 token0, cfg+1 token1+dec, cfg+2 n, cfg+3 window,
    cfg+4 reserveX, cfg+5 reserveY, cfg+6 fee+paused,
    cfg+7 [block:32][0:64][pos:96][0:96]
    data+0 [tsY:32][tsX:32][field1:32][field0:64]
    ladder[i] = keccak256(uint256(cfg+2)) + i, 每槽 [x:128][y:128]
"""
import argparse, json, pickle, ssl, urllib.request

PAIRS = {
    "335c400406e84be9c8026ae2b9f8ab07fad4d26bcb8a4c8aede0c9b463618258": "xSOL / wNVDAx",
    "d81a7adf81bba96b8b4dd9bc544761315048650d93a0935e8ec08e27da0ef232": "xSOL / wCRCLx",
    "55c40a68abf347da3a1b6b5130e6b201a27108c3984d2b55e050281199c566d8": "xSOL / wSPCXx",
    "5dda42efa9e87d91f30e065b6e9fa431d6ac6c29603d09e952e0c191af29d8ec": "xSOL / wSKHYx",
}

CT = "0x154586b2479b9a11e3d4db90024dc0e26f097312"


def keccak256(b: bytes) -> bytes:
    from Crypto.Hash import keccak as _k
    h = _k.new(digest_bits=256)
    h.update(b)
    return h.digest()


def p32(x: int) -> bytes:
    return x.to_bytes(32, "big")


def pow10(e: int) -> int:
    return 10 ** e


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--block", required=True, help="区块号（十进制）")
    ap.add_argument("--rpc", required=True, help="RPC URL")
    ap.add_argument("--out", required=True, help="输出 pkl/json 路径")
    args = ap.parse_args()
    block = int(args.block)

    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE  # rpc.xlayer.tech 证书问题

    def rpc(method, params):
        req = urllib.request.Request(
            args.rpc,
            data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode(),
            headers={"content-type": "application/json"},
        )
        with urllib.request.urlopen(req, timeout=60, context=ctx) as r:
            return json.load(r)["result"]

    state = {}
    for pid, label in PAIRS.items():
        pair = bytes.fromhex(pid)
        cfg = int.from_bytes(keccak256(pair + p32(6)), "big")
        data = int.from_bytes(keccak256(pair + p32(7)), "big")

        def get(slot: int) -> int:
            return int(rpc("eth_getStorageAt", [CT, hex(slot), hex(block)]), 16)

        cfg0, cfg1 = get(cfg), get(cfg + 1)
        n, win = get(cfg + 2), get(cfg + 3)
        rx, ry = get(cfg + 4), get(cfg + 5)
        cfg6, cfg7 = get(cfg + 6), get(cfg + 7)
        data0 = get(data)

        dec_x = (cfg1 >> 0xA0) & 0xFF
        dec_y = (cfg1 >> 0xA8) & 0xFF
        fee = cfg6 & (2 ** 64 - 1)
        pos = (cfg7 >> 96) & (2 ** 96 - 1)
        field0 = data0 & (2 ** 64 - 1)
        field1 = (data0 >> 64) & (2 ** 32 - 1)
        lb = int.from_bytes(keccak256(p32(cfg + 2)), "big")
        ladder = []
        for i in range(n):
            v = get(lb + i)
            ladder.append((v >> 128, v & (2 ** 128 - 1)))

        state[pid] = {
            "cfg0": cfg0, "cfg1": cfg1, "n": n, "win": win,
            "rx": rx, "ry": ry, "cfg6": cfg6, "cfg7": cfg7,
            "data0": data0, "data1": get(data + 1),
            "ladder": ladder,
            "dec_x": dec_x, "dec_y": dec_y,
            "field0": field0, "field1": field1, "fee": fee,
            "pos": pos, "scale": pow10(dec_x) // pow10(dec_y),
        }
        print(f"{pid[:14]} {label}: n={n} pos={pos} field0={field0} field1={field1} "
              f"fee={fee} dec=({dec_x},{dec_y}) ladder={ladder}")

    with open(args.out, "wb" if args.out.endswith(".pkl") else "w") as f:
        if args.out.endswith(".pkl"):
            pickle.dump(state, f)
        else:
            json.dump(state, f, indent=1)
    print("saved ->", args.out)


if __name__ == "__main__":
    main()

import json, os, urllib.request, sys
from Crypto.Hash import keccak as _keccak

def keccak256(b):
    h = _keccak.new(digest_bits=256); h.update(b); return h.digest()

RPC = "http://127.0.0.1:8557"
CONTRACT = "0x154586b2479b9a11e3d4db90024dc0e26f097312"
PAIR = bytes.fromhex("335c400406e84be9c8026ae2b9f8ab07fad4d26bcb8a4c8aede0c9b463618258")

def rpc(method, params):
    req = urllib.request.Request(RPC, data=json.dumps({"jsonrpc":"2.0","id":1,"method":method,"params":params}).encode(), headers={"content-type":"application/json"})
    with urllib.request.urlopen(req, timeout=120) as r:
        return json.load(r)["result"]

code = None
_HERE = os.path.dirname(os.path.abspath(__file__))
_code_bin = os.path.join(_HERE, "caliber_code.bin")
if os.path.exists(_code_bin):
    code = open(_code_bin, "rb").read()
else:
    code = bytes.fromhex(rpc("eth_getCode", [CONTRACT, "latest"])[2:])

def p32(x): return x.to_bytes(32, "big")
cfg = int.from_bytes(keccak256(PAIR + p32(6)), "big")
data = int.from_bytes(keccak256(PAIR + p32(7)), "big")
storage = {}
def S(a, v): storage[a] = v
S(cfg+0, 0xe7b000003a45145decf8a28fc755ad5ec5ea025a)
S(cfg+1, 0x612779ded0c9e1022225f8e0630b35a9b54be713736)
S(cfg+2, 3); S(cfg+3, 500); S(cfg+4, 4035636208082082157); S(cfg+5, 13990178462); S(cfg+6, 0x1d1a94a20000000018bcfe5680001010000000000000000c8)
S(cfg+7, 0x03f3cbf100000000000000000f962ef7000000000000000000000000)
S(data+0, 0x6a66217a00000069000001ba76e1f900)
S(2, 0xa)
lb = int.from_bytes(keccak256(p32(cfg+2)), "big")
for i,(x,y) in enumerate([(10,200000000),(50,900000000),(300,1000000000)]):
    S(lb+i, (x<<128)|y)

calldata = bytes.fromhex("4fcb3aa6" + PAIR.hex() + "0"*24 + "779ded0c9e1022225f8e0630b35a9b54be713736" + "0"*24 + "e7b000003a45145decf8a28fc755ad5ec5ea025a" + "0"*63 + "1")
W = 2**256
stack, mem = [], bytearray()
recent = []
pc = 0
arith = []
sloads = []
steps = 0
BLOCK_TS = 1785078141

def push(v): stack.append(v % W)
def pop(): return stack.pop()
def grow(n):
    if n > 4_000_000: raise SystemExit(f"mem huge {n}")
    while len(mem) < n: mem.append(0)

def do_arith(opname, pc_):
    a = pop(); b = pop()
    if opname == "ADD": r = a + b
    elif opname == "MUL": r = a * b
    elif opname == "SUB": r = a - b
    elif opname == "DIV": r = 0 if b == 0 else a // b
    elif opname == "SDIV":
        sa = a if a < 2**255 else a - W; sb = b if b < 2**255 else b - W
        r = 0 if sb == 0 else (abs(sa)//abs(sb))*(-1 if (sa<0)!=(sb<0) else 1)
    elif opname == "MOD": r = 0 if b == 0 else a % b
    elif opname == "SMOD":
        sa = a if a<2**255 else a-W; sb = b if b<2**255 else b-W
        r = 0 if sb == 0 else (abs(sa)%abs(sb))*(-1 if sa<0 else 1)
    elif opname == "EXP":
        if b > 10**6: raise SystemExit(f"EXP huge {b} at {pc_}")
        r = pow(a, b, W)
    push(r % W)
    if opname in ("ADD","MUL","SUB","DIV","MOD"):
        arith.append((pc_, opname, a, b, r % W))

def do_cmp(opname):
    a = pop(); b = pop()
    if opname=="LT": r = a<b
    elif opname=="GT": r = a>b
    elif opname=="SLT":
        sa=a if a<2**255 else a-W; sb=b if b<2**255 else b-W; r=sa<sb
    elif opname=="SGT":
        sa=a if a<2**255 else a-W; sb=b if b<2**255 else b-W; r=sa>sb
    else: r = a==b
    push(1 if r else 0)

OPS = {
    0x00:"STOP",0x01:"ADD",0x02:"MUL",0x03:"SUB",0x04:"DIV",0x05:"SDIV",0x06:"MOD",0x07:"SMOD",0x08:"ADDMOD",0x09:"MULMOD",0x0a:"EXP",0x0b:"SIGNEXTEND",
    0x10:"LT",0x11:"GT",0x12:"SLT",0x13:"SGT",0x14:"EQ",0x15:"ISZERO",0x16:"AND",0x17:"OR",0x18:"XOR",0x19:"NOT",0x1a:"BYTE",0x1b:"SHL",0x1c:"SHR",0x1d:"SAR",
    0x20:"SHA3",
    0x30:"ADDRESS",0x31:"BALANCE",0x32:"ORIGIN",0x33:"CALLER",0x34:"CALLVALUE",0x35:"CALLDATALOAD",0x36:"CALLDATASIZE",0x37:"CALLDATACOPY",0x38:"CODESIZE",0x39:"CODECOPY",0x3a:"GASPRICE",0x3b:"EXTCODESIZE",0x3c:"EXTCODECOPY",0x3d:"RETURNDATASIZE",0x3e:"RETURNDATACOPY",0x3f:"EXTCODEHASH",
    0x40:"BLOCKHASH",0x41:"COINBASE",0x42:"TIMESTAMP",0x43:"NUMBER",0x44:"PREVRANDAO",0x45:"GASLIMIT",0x46:"CHAINID",0x47:"SELFBALANCE",0x48:"BASEFEE",0x49:"BLOBHASH",0x4a:"BLOBBASEFEE",
    0x50:"POP",0x51:"MLOAD",0x52:"MSTORE",0x53:"MSTORE8",0x54:"SLOAD",0x55:"SSTORE",0x56:"JUMP",0x57:"JUMPI",0x58:"PC",0x59:"MSIZE",0x5a:"GAS",0x5b:"JUMPDEST",0x5f:"PUSH0",
    0xa0:"LOG0",0xa1:"LOG1",0xa2:"LOG2",0xa3:"LOG3",0xa4:"LOG4",
    0xf0:"CREATE",0xf1:"CALL",0xf2:"CALLCODE",0xf3:"RETURN",0xf4:"DELEGATECALL",0xfa:"STATICCALL",0xfd:"REVERT",0xfe:"INVALID",0xff:"SELFDESTRUCT",
}

while pc < len(code):
    steps += 1
    if steps % 50000 == 0: print("... step", steps, "pc", pc, "stack", len(stack), "mem", len(mem))
    if steps > 400000: print("step limit"); break
    op = code[pc]
    opname = OPS.get(op)
    if opname is None:
        if 0x60 <= op <= 0x7f: opname = f"PUSH{op-0x5f}"
        elif 0x80 <= op <= 0x8f: opname = f"DUP{op-0x7f}"
        elif 0x90 <= op <= 0x9f: opname = f"SWAP{op-0x8f}"
        else: print("unknown op", hex(op), "at", pc); break
    start_pc = pc
    pc += 1
    recent.append(f"{start_pc:5d} {opname} stack={[hex(x) for x in stack[-6:]]}")
    recent = recent[-200:]
    if 0x3b40 <= start_pc <= 0x3e80 or 0x51c0 <= start_pc <= 0x5370:
        with open("/tmp/exec_trace2.txt","a") as f:
            f.write(f"{start_pc:6d}(0x{start_pc:x}) {opname} stack={[hex(x) for x in stack[-8:]]}\n")
    if opname == "PUSH0": push(0)
    elif opname.startswith("PUSH"):
        n = int(opname[4:])
        push(int.from_bytes(code[pc:pc+n], "big")); pc += n
    elif opname == "POP": pop()
    elif opname.startswith("DUP"): stack.append(stack[-int(opname[3:])])
    elif opname.startswith("SWAP"):
        n = int(opname[4:]); stack[-1], stack[-1-n] = stack[-1-n], stack[-1]
    elif opname in ("ADD","MUL","SUB","DIV","SDIV","MOD","SMOD","EXP"): do_arith(opname, start_pc)
    elif opname == "ADDMOD":
        a=pop(); b=pop(); c=pop(); push(0 if c==0 else (a+b)%c)
    elif opname == "MULMOD":
        a=pop(); b=pop(); c=pop(); push(0 if c==0 else (a*b)%c)
    elif opname == "SIGNEXTEND":
        b=pop(); a=pop()
        if b<32:
            t=8*b+7; m=(1<<(t+1))-1; r=a&m
            if (a>>t)&1: r-=1<<(t+1)
            push(r)
    elif opname in ("LT","GT","SLT","SGT","EQ"): do_cmp(opname)
    elif opname == "ISZERO": a=pop(); push(1 if a==0 else 0)
    elif opname in ("AND","OR","XOR"):
        a=pop(); b=pop(); push(a&b if opname=="AND" else a|b if opname=="OR" else a^b)
    elif opname == "NOT": a=pop(); push(W-1-a)
    elif opname == "BYTE":
        i=pop(); x=pop(); push(0 if i>=32 else (x>>(8*(31-i)))&0xff)
    elif opname in ("SHL","SHR","SAR"):
        s=pop(); x=pop()
        if opname=="SHL": r = x<<s if s<256 else 0
        elif opname=="SHR": r = x>>s if s<256 else 0
        else:
            if s>=256: r = 0 if x < 2**255 else W-1
            else:
                r = x>>s
                if (x & (1<<255)) and s>0: r |= W - (1<<(256-s))
        push(r % W)
    elif opname == "SHA3":
        off=pop(); ln=pop()
        if off+ln > 4_000_000: raise SystemExit(f"sha3 huge {off} {ln}")
        push(int.from_bytes(keccak256(bytes(mem[off:off+ln])), "big"))
    elif opname == "ADDRESS": push(int(CONTRACT,16))
    elif opname == "ORIGIN": push(1)
    elif opname == "CALLER": push(1)
    elif opname == "CALLVALUE": push(0)
    elif opname == "CALLDATALOAD":
        off=pop()
        push(int.from_bytes(calldata[off:off+32].ljust(32,b'\0'), "big"))
    elif opname == "CALLDATASIZE": push(len(calldata))
    elif opname == "CALLDATACOPY":
        off1=pop(); off2=pop(); ln=pop()
        if off1+ln > 4_000_000 or off2+ln > len(calldata)+33: raise SystemExit(f"calldatacopy {off1} {off2} {ln}")
        grow(off1+ln)
        mem[off1:off1+ln] = calldata[off2:off2+ln]
    elif opname == "CODESIZE": push(len(code))
    elif opname == "CODECOPY":
        off1=pop(); off2=pop(); ln=pop()
        grow(off1+ln)
        mem[off1:off1+ln] = code[off2:off2+ln]
    elif opname == "GASPRICE": push(0)
    elif opname == "RETURNDATASIZE": push(0)
    elif opname == "RETURNDATACOPY":
        off1=pop(); off2=pop(); ln=pop(); grow(off1+ln)
    elif opname == "MLOAD":
        off=pop(); grow(off+32); push(int.from_bytes(mem[off:off+32],"big"))
        if 0x3b00 <= start_pc <= 0x3e80 or 0x5000 <= start_pc <= 0x5600:
            print(f"MLOAD pc={start_pc:#06x} off={off} val={int.from_bytes(mem[off:off+32],'big')}")
    elif opname == "MSTORE":
        off=pop(); v=pop(); grow(off+32); mem[off:off+32] = v.to_bytes(32,"big")
    elif opname == "MSTORE8":
        off=pop(); v=pop(); grow(off+1); mem[off] = v & 0xff
    elif opname == "SLOAD":
        k=pop(); push(storage.get(k,0))
        sloads.append((start_pc, k, storage.get(k,0)))
        if 0x3c00 <= start_pc <= 0x3d60:
            print(f"SLOAD pc={start_pc:#06x} slot={k:#x} = {storage.get(k,0)}")
    elif opname == "SSTORE":
        v=pop(); k=pop(); storage[k]=v
    elif opname == "JUMP":
        pc=pop()
    elif opname == "JUMPI":
        dest=pop(); c=pop()
        if 0x3c00 <= start_pc <= 0x3d60:
            print(f"JUMPI pc={start_pc:#06x} dest={dest:#x} cond={c}")
        if c: pc=dest
    elif opname == "PC": push(start_pc)
    elif opname == "MSIZE": push(len(mem))
    elif opname == "GAS": push(10**18)
    elif opname == "JUMPDEST": pass
    elif opname == "TIMESTAMP": push(BLOCK_TS)
    elif opname == "NUMBER": push(66309105)
    elif opname in ("BLOCKHASH","COINBASE","PREVRANDAO","GASLIMIT","CHAINID","SELFBALANCE","BASEFEE","BLOBHASH","BLOBBASEFEE"):
        pop() if opname=="BLOCKHASH" else None
        push(0)
    elif opname == "RETURN":
        off=pop(); ln=pop()
        ret = bytes(mem[off:off+ln])
        print("RETURN:", ret.hex(), "=", int.from_bytes(ret,"big"))
        print("final stack:", [hex(x) for x in stack[-6:]])
        break
    elif opname == "REVERT":
        off=pop(); ln=pop()
        print("REVERT:", bytes(mem[off:off+ln]).hex(), "pc=", start_pc, "stack:", [hex(x) for x in stack[-6:]])
        print("=== last steps ===")
        with open("/tmp/last_steps.txt","w") as f:
            for st in recent[-90:]:
                f.write(st + "\n")
        break
    else:
        print("unhandled", opname, "at", start_pc); break

print("arith count:", len(arith))
print("=== SLOADs ===")
for pc_, k, v in sloads:
    print(f"pc={pc_:5d} slot={k:#066x} = {v}")

for pc_,op,a,b,r in arith:
    print(f"pc={pc_:5d} {op:3s} {a} {op.lower()} {b} = {r}")

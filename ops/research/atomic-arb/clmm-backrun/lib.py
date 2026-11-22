"""Decoders for SOL/USDC pool events and accounts (Orca Whirlpool, Raydium CLMM, Meteora DLMM)."""
import base64, struct, hashlib

B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
def b58e(b):
    n = int.from_bytes(b, "big"); s = ""
    while n: n, r = divmod(n, 58); s = B58[r] + s
    return "1" * (len(b) - len(b.lstrip(b"\0"))) + s
def b58d(s):
    n = 0
    for c in s: n = n * 58 + B58.index(c)
    b = n.to_bytes((n.bit_length() + 7) // 8, "big")
    return b"\0" * (len(s) - len(s.lstrip("1"))) + b

WHIRL = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc"
RCLMM = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK"
DLMM = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo"
RAMM4 = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8"
JUP6 = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUJZi8Ds6vVi5j"
SOL = "So11111111111111111111111111111111111111112"
USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
USDT = "Es9vMFrzaCERmJfrF4H7YVTSNVUvDDQZ3hqFxiuKMQ8"
DEC = {SOL: 9, USDC: 6, USDT: 6}
Q64 = 2 ** 64

def disc(name): return hashlib.sha256(("event:" + name).encode()).digest()[:8]
D_TRADED, D_SWAPEV, D_DLMMSWAP = disc("Traded"), disc("SwapEvent"), disc("Swap")
EVENT_IX_TAG = bytes.fromhex("e445a52e51cb9a1d")

def u128(b, o): return int.from_bytes(b[o:o + 16], "little")

def events_from_tx(tx):
    """Return ordered list of pool swap events in a jsonParsed/json tx."""
    out = []
    logs = tx["meta"].get("logMessages") or []
    for l in logs:
        if not l.startswith("Program data: "): continue
        try: d = base64.b64decode(l[14:])
        except Exception: continue
        if d[:8] == D_TRADED and len(d) == 121:
            p = d[8:]
            out.append(dict(prog="whirl", pool=b58e(p[0:32]), a_to_b=bool(p[32]),
                pre_sqrt=u128(p, 33), post_sqrt=u128(p, 49),
                amt_in=struct.unpack_from("<Q", p, 65)[0], amt_out=struct.unpack_from("<Q", p, 73)[0],
                lp_fee=struct.unpack_from("<Q", p, 97)[0], proto_fee=struct.unpack_from("<Q", p, 105)[0]))
        elif d[:8] == D_SWAPEV and len(d) >= 205:
            p = d[8:]
            a0, f0, a1, f1 = struct.unpack_from("<QQQQ", p, 128)
            z = bool(p[160])
            e = dict(prog="rclmm", pool=b58e(p[0:32]), zero_for_one=z, a_to_b=z,
                post_sqrt=u128(p, 161), liquidity=u128(p, 177), tick=struct.unpack_from("<i", p, 193)[0],
                amt0=a0, amt1=a1)
            if len(p) >= 213:
                e["fee0"], e["fee1"] = struct.unpack_from("<QQ", p, 197)
            e["amt_in"], e["amt_out"] = (a0, a1) if z else (a1, a0)
            out.append(e)
    # Meteora DLMM: self-CPI event in inner instructions (data base58)
    keys = None
    for ii in tx["meta"].get("innerInstructions") or []:
        for ix in ii["instructions"]:
            pid = ix.get("programId")
            if pid is None and "programIdIndex" in ix:
                keys = keys or all_keys(tx); pid = keys[ix["programIdIndex"]]
            if pid != DLMM or "data" not in ix: continue
            try: d = b58d(ix["data"])
            except Exception: continue
            if d[:8] == EVENT_IX_TAG and d[8:16] == D_DLMMSWAP:
                p = d[16:]
                lb = b58e(p[0:32])
                sb, eb = struct.unpack_from("<ii", p, 64)
                ain, aout = struct.unpack_from("<QQ", p, 72)
                sfy = bool(p[88]); fee, pfee = struct.unpack_from("<QQ", p, 89)
                out.append(dict(prog="dlmm", pool=lb, start_bin=sb, end_bin=eb, amt_in=ain, amt_out=aout,
                                a_to_b=sfy, fee=fee, proto_fee=pfee))
    return out

def parse_whirlpool(data):
    return dict(prog="whirl", tick_spacing=struct.unpack_from("<H", data, 41)[0],
        fee_rate=struct.unpack_from("<H", data, 45)[0], liquidity=u128(data, 49), sqrt=u128(data, 65),
        tick=struct.unpack_from("<i", data, 81)[0], mint_a=b58e(data[101:133]), vault_a=b58e(data[133:165]),
        mint_b=b58e(data[181:213]), vault_b=b58e(data[213:245]))

def parse_rclmm(data):
    return dict(prog="rclmm", amm_config=b58e(data[9:41]), mint_a=b58e(data[73:105]), mint_b=b58e(data[105:137]),
        vault_a=b58e(data[137:169]), vault_b=b58e(data[169:201]), observation=b58e(data[201:233]),
        tick_spacing=struct.unpack_from("<H", data, 235)[0], liquidity=u128(data, 237), sqrt=u128(data, 253),
        tick=struct.unpack_from("<i", data, 269)[0])

def parse_rclmm_config(data):
    return dict(protocol_fee_rate=struct.unpack_from("<I", data, 43)[0], fee_rate=struct.unpack_from("<I", data, 47)[0],
                tick_spacing=struct.unpack_from("<H", data, 51)[0])

def parse_dlmm(data):
    base_factor = struct.unpack_from("<H", data, 8)[0]
    power = data[8 + 29] if len(data) > 37 else 0
    bin_step = struct.unpack_from("<H", data, 80)[0]
    return dict(prog="dlmm", base_factor=base_factor, base_fee_power=power, bin_step=bin_step,
        active_id=struct.unpack_from("<i", data, 76)[0], mint_a=b58e(data[88:120]), mint_b=b58e(data[120:152]),
        vault_a=b58e(data[152:184]), vault_b=b58e(data[184:216]),
        fee_rate=base_factor * bin_step * 10 * (10 ** power) / 1e3)  # in 1e-6 units

def price_from_sqrt(sqrt_x64, dec_a=9, dec_b=6):
    """price of token A in token B (e.g. USDC per SOL)."""
    return (sqrt_x64 / Q64) ** 2 * 10 ** (dec_a - dec_b)

def price_from_bin(bin_id, bin_step, dec_a=9, dec_b=6):
    return (1 + bin_step / 1e4) ** bin_id * 10 ** (dec_a - dec_b)

def all_keys(tx):
    m = tx["transaction"]["message"]
    ks = [k if isinstance(k, str) else k["pubkey"] for k in m["accountKeys"]]
    la = tx["meta"].get("loadedAddresses") or {}
    return ks + la.get("writable", []) + la.get("readonly", [])

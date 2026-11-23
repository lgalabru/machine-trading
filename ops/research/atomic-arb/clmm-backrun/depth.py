"""Current liquidity profile (+-N bps) of the two main SOL/USDC CLMM pools from their tick arrays.
Writes data/depth_profile.json: per pool, list of (tick_lo, tick_hi, L) segments and cumulative swap
amounts to move the price by k bps."""
import sys, os, json, base64, struct, math
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import rpc, lib

def i128(b, o): return int.from_bytes(b[o:o + 16], "little", signed=True)

def ticks_rclmm(pool, lo, hi):
    r = rpc.call("getProgramAccounts", [lib.RCLMM, {"encoding": "base64", "dataSlice": {"offset": 40, "length": 4},
        "filters": [{"memcmp": {"offset": 8, "bytes": pool}}]}])
    keys = [x["pubkey"] for x in r if lo - 6000 <= struct.unpack("<i", base64.b64decode(x["account"]["data"][0]))[0] <= hi]
    out = {}
    for i in range(0, len(keys), 50):
        a = rpc.call("getMultipleAccounts", [keys[i:i + 50], {"encoding": "base64"}])["value"]
        for acc in a:
            d = base64.b64decode(acc["data"][0])
            for j in range(60):
                o = 44 + j * 168
                t = struct.unpack_from("<i", d, o)[0]; ln = i128(d, o + 4); lg = int.from_bytes(d[o + 20:o + 36], "little")
                if lg: out[t] = ln
    return out

def ticks_whirl(pool, lo, hi):
    out = {}
    r = rpc.call("getProgramAccounts", [lib.WHIRL, {"encoding": "base64", "dataSlice": {"offset": 8, "length": 4},
        "filters": [{"memcmp": {"offset": 9956, "bytes": pool}}]}])
    keys = [x["pubkey"] for x in r if lo - 2000 <= struct.unpack("<i", base64.b64decode(x["account"]["data"][0]))[0] <= hi]
    for i in range(0, len(keys), 50):
        for acc in rpc.call("getMultipleAccounts", [keys[i:i + 50], {"encoding": "base64"}])["value"]:
            d = base64.b64decode(acc["data"][0]); st = struct.unpack_from("<i", d, 8)[0]
            for j in range(88):
                o = 12 + j * 113
                if d[o]: out[st + j * 4] = i128(d, o + 1)
    # dynamic tick arrays: disc 8 | start i32 | whirlpool 32 | bitmap u128 | ticks (1 byte tag, +112 if initialized)
    r2 = rpc.call("getProgramAccounts", [lib.WHIRL, {"encoding": "base64", "dataSlice": {"offset": 8, "length": 4},
        "filters": [{"memcmp": {"offset": 12, "bytes": pool}}]}])
    keys = [x["pubkey"] for x in r2 if lo - 2000 <= struct.unpack("<i", base64.b64decode(x["account"]["data"][0]))[0] <= hi]
    for i in range(0, len(keys), 50):
        for acc in rpc.call("getMultipleAccounts", [keys[i:i + 50], {"encoding": "base64"}])["value"]:
            d = base64.b64decode(acc["data"][0]); st = struct.unpack_from("<i", d, 8)[0]; o = 60
            for j in range(88):
                if d[o]: out[st + j * 4] = i128(d, o + 1); o += 113
                else: o += 1
    return out, len(r2)

def profile(state, ticks, span_bps=300):
    """Walk from current tick; return segments and amounts needed to move price by k bps each way."""
    t0, L0, s0 = state["tick"], state["liquidity"], state["sqrt"] / lib.Q64
    res = {}
    for direction in (+1, -1):
        L = L0; s = s0; usdc = sol = 0.0; rows = []
        target_ticks = sorted([t for t in ticks if (t > t0 if direction > 0 else t <= t0)], reverse=direction < 0)
        k_done = 0
        for k in range(1, span_bps + 1):
            s_target = s0 * math.sqrt((1 + 1e-4) ** (direction * k)) if False else s0 * (1.0001 ** (direction * k / 2))
            # cross ticks between s and s_target
            while target_ticks and ((direction > 0 and 1.0001 ** (target_ticks[0] / 2) <= s_target) or
                                    (direction < 0 and 1.0001 ** (target_ticks[0] / 2) >= s_target)):
                t = target_ticks.pop(0); st = 1.0001 ** (t / 2)
                usdc += L * abs(st - s) / 1e6; sol += L * abs(1 / st - 1 / s) / 1e9; s = st
                L += ticks[t] if direction > 0 else -ticks[t]
            usdc += L * abs(s_target - s) / 1e6; sol += L * abs(1 / s_target - 1 / s) / 1e9; s = s_target
            if k in (1, 2, 5, 10, 20, 50, 100, 200, 300):
                rows.append(dict(bps=k, usdc=round(usdc), sol=round(sol, 2), L_at=L))
        res["up" if direction > 0 else "down"] = rows
    return res

if __name__ == "__main__":
    out = {}
    W, R = "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE", "3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv"
    acc = rpc.call("getMultipleAccounts", [[W, R], {"encoding": "base64"}])
    slot = acc["context"]["slot"]
    w = lib.parse_whirlpool(base64.b64decode(acc["value"][0]["data"][0]))
    r = lib.parse_rclmm(base64.b64decode(acc["value"][1]["data"][0]))
    tw, ndyn = ticks_whirl(W, w["tick"] - 4000, w["tick"] + 4000)
    tr = ticks_rclmm(R, r["tick"] - 4000, r["tick"] + 4000)
    for name, st, tk in (("whirl_" + W, w, tw), ("rclmm_" + R, r, tr)):
        out[name] = dict(slot=slot, price=lib.price_from_sqrt(st["sqrt"]), tick=st["tick"], liquidity=st["liquidity"],
                         n_init_ticks=len(tk), profile=profile(st, tk))
    out["whirl_dynamic_tick_arrays"] = ndyn
    json.dump(out, open(os.path.join(os.path.dirname(__file__), "..", "data", "depth_profile.json"), "w"), indent=1, default=str)
    for k, v in out.items():
        if isinstance(v, dict):
            print(k, round(v["price"], 4), v["liquidity"], v["n_init_ticks"])
            for d in ("up", "down"): print("  ", d, [(x["bps"], x["usdc"], x["sol"]) for x in v["profile"][d]])
        else: print(k, v)

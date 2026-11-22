"""Pool metadata cache (static params), fetched once via getMultipleAccounts."""
import json, os, base64, sys
sys.path.insert(0, os.path.dirname(__file__))
import lib, rpc
CACHE = os.path.join(os.path.dirname(__file__), "..", "data", "pools.json")
def _load():
    for _ in range(20):
        try: return json.load(open(CACHE)) if os.path.exists(CACHE) else {}
        except ValueError: import time; time.sleep(0.2)
    return {}
_c = _load()
_cfg = {}

def get(pairs):
    """pairs: iterable of (prog, pool). Returns dict pool->meta."""
    need = [(g, p) for g, p in set(pairs) if p not in _c]
    for i in range(0, len(need), 100):
        chunk = need[i:i + 100]
        r = rpc.call("getMultipleAccounts", [[p for _, p in chunk], {"encoding": "base64"}])
        for (g, p), a in zip(chunk, r["value"]):
            if not a: _c[p] = {"prog": g, "missing": True}; continue
            d = base64.b64decode(a["data"][0])
            m = {"whirl": lib.parse_whirlpool, "rclmm": lib.parse_rclmm, "dlmm": lib.parse_dlmm}[g](d)
            for k in ("liquidity", "sqrt"): m.pop(k, None)
            m.pop("tick", None); m.pop("active_id", None)
            _c[p] = m
    # raydium fee from amm_config
    cfgs = sorted({m["amm_config"] for m in _c.values() if m.get("prog") == "rclmm" and "fee_rate" not in m})
    if cfgs:
        r = rpc.call("getMultipleAccounts", [cfgs, {"encoding": "base64"}])
        vals = r.get("value") if isinstance(r, dict) else None
        for k, a in zip(cfgs, vals or [None] * len(cfgs)):
            _cfg[k] = lib.parse_rclmm_config(base64.b64decode(a["data"][0])) if a else {"fee_rate": 0}
        for m in _c.values():
            if m.get("prog") == "rclmm" and "fee_rate" not in m:
                m["fee_rate"] = _cfg[m["amm_config"]]["fee_rate"]
    if need or cfgs:
        tmp = CACHE + ".%d.tmp" % os.getpid()
        json.dump(_c, open(tmp, "w"), indent=0); os.replace(tmp, CACHE)
    return _c

def is_main(m, quotes=(lib.USDC, lib.USDT)):
    return m.get("mint_a") == lib.SOL and m.get("mint_b") in quotes or m.get("mint_b") == lib.SOL and m.get("mint_a") in quotes

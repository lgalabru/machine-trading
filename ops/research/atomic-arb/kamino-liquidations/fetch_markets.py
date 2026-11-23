"""Fetch lending markets (liquidation params, elevation groups) and the elevation group of every obligation in the records.
usage: python3 -I fetch_markets.py <scripts_dir> <records.jsonl> <out.json>
LendingMarket offsets: close factor 126, insolvency 127, min_full 128, max_at_once 136, elevation_groups at 200 (72 B each).
Obligation: elevation_group at 2285.
"""
import sys, json, base64, struct
sys.path.insert(0, sys.argv[1])
import rpc
from b58 import b58encode

recs = [json.loads(l) for l in open(sys.argv[2])]
markets = sorted({r["market"] for r in recs})
obls = sorted({r["obligation"] for r in recs})

def multi(addrs):
    out = {}
    for i in range(0, len(addrs), 100):
        chunk = addrs[i:i + 100]
        res = rpc.call("getMultipleAccounts", [chunk, {"encoding": "base64"}])
        for a, acc in zip(chunk, res["value"]):
            out[a] = base64.b64decode(acc["data"][0]) if acc else None
    return out

M = {}
for a, d in multi(markets).items():
    if not d: continue
    groups = {}
    for i in range(32):
        o = 200 + 72 * i
        bonus, gid, ltv, lt = struct.unpack_from("<HBBB", d, o)
        if gid:
            groups[gid] = {"max_bonus_bps": bonus, "ltv_pct": ltv, "liq_threshold_pct": lt,
                           "debt_reserve": b58encode(d[o + 8:o + 40])}
    M[a] = {"close_factor_pct": d[126], "insolvency_ltv_pct": d[127],
            "min_full_liquidation_value": struct.unpack_from("<Q", d, 128)[0],
            "max_liquidatable_at_once": struct.unpack_from("<Q", d, 136)[0], "elevation_groups": groups}
O = {}
for a, d in multi(obls).items():
    O[a] = d[2285] if d and len(d) == 3344 else None
json.dump({"markets": M, "obligation_group": O}, open(sys.argv[3], "w"), indent=1)
print(len(M), "markets;", sum(1 for v in O.values() if v), "of", len(O), "obligations in an elevation group;",
      sum(1 for v in O.values() if v is None), "missing")

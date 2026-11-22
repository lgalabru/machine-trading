"""Rebuild the pool states the round1 multi-token arbs saw (from their own legs' events) and run the detector:
  state A ("as seen"): every leg's pre-trade mid and local depth inferred from the arb's own fill
  state B ("before trigger"): the trigger pool's mid rolled back to the trigger's own pre-trade price
Expectation: hit on A with profit close to realized, miss on B. Also synthetic epoch-step / Withdraw-floor tests.
Writes data/detector_results.json"""
import json, math, os, sys
sys.path.insert(0, os.path.dirname(__file__))
from detector import detect
import rpc, base64, struct
from legs import WSOL
D = os.path.join(os.path.dirname(__file__), "../data")
SYM = {WSOL: "SOL"}

def fee_of(l):
    if l.get("fee_paid_in") is not None and l["amt_in"]: return l["fee_paid_in"] / l["amt_in"]
    if l.get("dlmm") and l["amt_in"]: return l["dlmm"]["fee"] / l["amt_in"]
    return None

_bs = {}
def dlmm_binstep(pool):
    if pool not in _bs:
        a = rpc.call("getAccountInfo", [pool, {"encoding": "base64"}])
        d = base64.b64decode(a["value"]["data"][0]) if a and a["value"] else b""
        _bs[pool] = (struct.unpack_from("<H", d, 80)[0], base64.b64encode(d[88:120]).decode()) if len(d) > 120 else (None, None)
    return _bs[pool][0]

def leg_edge(l):
    """edge dict with marginal pre-trade price + virtual reserves calibrated on the observed fill."""
    din, dout = l["dec_in"] or 0, l["dec_out"] or 0
    x = l["amt_in"] / 10 ** din; y = l["amt_out"] / 10 ** dout
    ex = y / x
    f = fee_of(l)
    mid = l.get("pre_px")
    if mid is None and l.get("dlmm"):
        bs = dlmm_binstep(l["pool"])
        if bs:
            # price y per x at start bin; orientation by swap_for_y
            p = (1 + bs / 1e4) ** l["dlmm"]["start_bin"] * 10 ** ((din if l["dlmm"]["swap_for_y"] else dout) - (dout if l["dlmm"]["swap_for_y"] else din))
            mid = p if l["dlmm"]["swap_for_y"] else 1 / p
    if mid is None and l.get("post_px"):
        mid = ex ** 2 / l["post_px"]  # CL: avg = geometric mean of pre and post (fee folded into mid)
        f = 0.0 if f is None else f
    e = {"from": SYM.get(l["mint_in"], l["mint_in"][:5]), "to": SYM.get(l["mint_out"], l["mint_out"][:5]), "venue": l["venue"], "pool": l["pool"]}
    if mid is None:  # prop AMM / unknown: linear at the executed rate, capacity = what it filled
        return {**e, "kind": "linear", "rate": ex, "fee": 0.0, "cap_in": x}, "linear"
    f = f or 0.0
    k = ex / (mid * (1 - f))
    if not (0 < k < 1):  # fill at or better than mid (rounding / fee mis-estimate): treat as linear
        return {**e, "kind": "linear", "rate": ex, "fee": 0.0, "cap_in": x}, "linear*"
    xe = x * (1 - f); r_in = xe * k / (1 - k)
    return {**e, "kind": "cpv", "r_in": r_in, "r_out": r_in * mid, "fee": f}, "cpv"

def main():
    T = {t["sig"]: t for t in json.load(open(f"{D}/round1_triggers.json"))}
    C = [c for c in json.load(open(f"{D}/round1_cycles.json")) if c["sig"] in T]
    res = []
    for c in C:
        edges, kinds = [], []
        for l in c["legs"]:
            e, k = leg_edge(l); edges.append(e); kinds.append(k)
        if edges[0]["from"] != "SOL": continue
        size0 = c["legs"][0]["amt_in"] / 1e9
        rA = detect({"edges": edges}, max_in=max(10.0, size0 * 4))
        top = rA["hits"][0] if rA["hits"] else None
        # state B: roll back trigger pool
        t = T[c["sig"]]["trigger"]; rB = None
        if t and t["same_block_before"] and t.get("impact_bps_on_pool") is not None:
            eb = []
            for e, l in zip(edges, c["legs"]):
                e2 = dict(e)
                if l["pool"] == t["pool"] and e2["kind"] == "cpv":
                    # trigger moved the pool by impact (in the trigger's direction); undo it in our direction
                    same_dir = (t["in"] == l["mint_in"])
                    adj = math.exp((1 if same_dir else -1) * t["impact_bps_on_pool"] / 1e4)  # trigger impact is in its own out/in terms
                    e2["r_out"] = e2["r_out"] / adj
                eb.append(e2)
            rB = detect({"edges": eb}, max_in=max(10.0, size0 * 4))
        res.append({"sig": c["sig"], "set": c["set"], "path": c["path"], "lst": c["lst"], "kinds": kinds, "realized_profit": c["profit_sol"],
                    "realized_size_in": size0, "A_hit": bool(top), "A_top": top, "A_ncands": len(rA["hits"]),
                    "B_tested": rB is not None, "B_hit": bool(rB and rB["hits"]), "B_top": rB and rB["hits"][:1]})
    # synthetic tests: stake-pool edges
    syn = []
    nav = 1.312795835; pool_mid_disc = -12.5e-4
    for label, pool_dev_bps, nav_step_bps in [("pool at NAV-5bp, epoch step +1.4bp", -5, 1.4), ("pool at NAV+3bp (above), deposit arb", 3, 0),
                                               ("pool at NAV-12.5bp, withdraw floor", -12.5, 0), ("pool at NAV-14bp, withdraw arb", -14, 0)]:
        navn = nav * (1 + nav_step_bps / 1e4); mid = nav * (1 + pool_dev_bps / 1e4)
        L = 60000.0  # virtual SOL reserve for ~1bp per 6 SOL (calibrated: 1 bp moves need ~6-8k SOL-equivalent on Hp53 is NOT measured; illustrative)
        edges = [{"from": "SOL", "to": "JitoSOL", "kind": "linear", "rate": 1 / navn, "fee": 0.0, "venue": "DepositSol", "pool": "Jito4A"},
                 {"from": "JitoSOL", "to": "SOL", "kind": "linear", "rate": navn, "fee": 0.001, "cap_in": 200.0, "venue": "WithdrawSol", "pool": "Jito4A"},
                 {"from": "SOL", "to": "JitoSOL", "kind": "cpv", "r_in": L, "r_out": L / mid, "fee": 0.0001, "venue": "OrcaWhirlpool", "pool": "Hp53XE"},
                 {"from": "JitoSOL", "to": "SOL", "kind": "cpv", "r_in": L / mid, "r_out": L, "fee": 0.0001, "venue": "OrcaWhirlpool", "pool": "Hp53XE"}]
        r = detect({"edges": edges}, max_in=2000)
        syn.append({"case": label, "hits": r["hits"][:2]})
    out = {"cycles": res, "synthetic": syn}
    json.dump(out, open(f"{D}/detector_results.json", "w"), indent=1)
    n = len(res); a = sum(r["A_hit"] for r in res); bt = [r for r in res if r["B_tested"]]
    print("cycles", n, "A hits", a, "| B tested", len(bt), "B hits (false/remaining)", sum(r["B_hit"] for r in bt))
    import statistics as st
    ratios = [r["A_top"]["profit"] / r["realized_profit"] for r in res if r["A_hit"] and r["realized_profit"] > 0.001]
    print("A est/realized profit ratio median", ratios and round(st.median(ratios), 3), "n", len(ratios))
    for s in syn: print(s)
    for r in res:
        if r["lst"] and r["set"] == "top200":
            print(round(r["realized_profit"], 4), ">".join(r["path"]), r["kinds"], "A", r["A_top"] and (r["A_top"]["edge_bps"], r["A_top"]["size_in"], r["A_top"]["profit"]), "B", r["B_tested"] and r["B_hit"])

if __name__ == "__main__":
    main()

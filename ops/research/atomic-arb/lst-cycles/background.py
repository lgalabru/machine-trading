"""Background deviation stats of LST/SOL pools vs NAV and the response to large non-arb swaps.
Input data/boundary_study.json + price_paths.json. Writes data/background_stats.json"""
import json, os, statistics as st
D = os.path.join(os.path.dirname(__file__), "../data")
B = json.load(open(f"{D}/boundary_study.json")); P = json.load(open(f"{D}/price_paths.json"))
WD_FEE = {"J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn": 10, "jupSoLaHXQiZZTSfEWMTRRgpnyFm8f6sZdosWBjx93v": 10}
POOL_FEE_BPS = {"Hp53XEtt4S8SvPCXarsLSdGfZBuUr5mMmZmX2DRNXQKp": 1, "2uoKbPEidR7KAMYtY4x7xdkHXWqYib5k4CutJauSL3Mc": 1,
                "BoeMUkCLHchTD31HdXsbDExuZZfcUppSLpYtV3LZTH6U": 1, "DtYKbQELgMZ3ihFUrCcCs9gy4djcUuhwgR7UpxVpP2Tg": 1,
                "8EzbUfvcRT1Q6RL462ekGkgqbxsPmwC5FMLQZhSPMjJ3": 1}
out = {}
for pool, d in B.items():
    rows = [r for r in d["rows"] if "dev_bps" in r]
    if not rows: continue
    lst = P[pool]["lst"]; fee = POOL_FEE_BPS.get(pool, 1); wd = WD_FEE.get(lst)
    devs = [r["dev_bps"] for r in rows]
    pts = sorted(P[pool]["points"], key=lambda p: (p["slot"], p["idx"] or 0))
    # impacts of non-cycle swaps with known pre/post
    shocks = []
    for i, p in enumerate(pts):
        if p["cycle"] or not (p["pre"] and p["post"]): continue
        imp = (p["post"] / p["pre"] - 1) * 1e4
        if abs(imp) < 1: continue
        nxt = next((q for q in pts[i + 1:] if q["cycle"]), None)
        rev = None
        if nxt and nxt["post"]: rev = (nxt["post"] / p["pre"] - 1) * 1e4
        shocks.append({"slot": p["slot"], "lst_amt": p["lst_amt"], "impact_bps": round(imp, 2),
                       "next_cycle_lag_slots": nxt and nxt["slot"] - p["slot"], "next_cycle_same_slot": bool(nxt and nxt["slot"] == p["slot"]),
                       "after_next_cycle_vs_pre_bps": rev and round(rev, 2)})
    s = {"label": d["label"], "n_swaps_with_mid": len(rows), "pool_fee_bps": fee,
         "dev_bps": {"min": round(min(devs), 2), "p5": round(st.quantiles(devs, n=20)[0], 2), "p50": round(st.median(devs), 2),
                     "p95": round(st.quantiles(devs, n=20)[-1], 2), "max": round(max(devs), 2)},
         "share_above_nav_plus_fee": round(sum(x > fee for x in devs) / len(devs), 4),
         "share_below_withdraw_band": (round(sum(x < -(wd + fee) for x in devs) / len(devs), 4) if wd else None),
         "n_shocks_ge1bp": len(shocks), "shocks_abs_impact_bps_p50_p90_max": shocks and [round(st.median([abs(x["impact_bps"]) for x in shocks]), 2),
                round(st.quantiles([abs(x["impact_bps"]) for x in shocks], n=10)[-1], 2) if len(shocks) > 10 else None, round(max(abs(x["impact_bps"]) for x in shocks), 2)],
         "next_cycle_lag_slots_p50": shocks and st.median([x["next_cycle_lag_slots"] for x in shocks if x["next_cycle_lag_slots"] is not None] or [None]) ,
         "share_shock_backrun_same_slot": shocks and round(sum(x["next_cycle_same_slot"] for x in shocks) / len(shocks), 3),
         "shocks": shocks}
    out[pool] = s
    print(d["label"], s["n_swaps_with_mid"], s["dev_bps"], "above NAV+fee", s["share_above_nav_plus_fee"], "below wd band", s["share_below_withdraw_band"],
          "| shocks", s["n_shocks_ge1bp"], s["shocks_abs_impact_bps_p50_p90_max"], "next-cycle lag p50", s["next_cycle_lag_slots_p50"], "same-slot backrun", s["share_shock_backrun_same_slot"])
json.dump(out, open(f"{D}/background_stats.json", "w"), indent=1)

# consecutive post-price moves (works for venues without a pre price)
moves = {}
for pool, d in B.items():
    rows = [r for r in sorted(d["rows"], key=lambda r: (r["slot"], r["idx"] or 0)) if r.get("mid")]
    if len(rows) < 10: continue
    mv = []
    for a, b in zip(rows, rows[1:]):
        m = (b["mid"] / a["mid"] - 1) * 1e4
        mv.append({"slot": b["slot"], "move_bps": m, "cycle": b["cycle"], "lst_amt": b["lst_amt"], "gap_slots": b["slot"] - a["slot"]})
    big = [x for x in mv if abs(x["move_bps"]) >= 2]
    a = [abs(x["move_bps"]) for x in mv]
    moves[pool] = {"label": d["label"], "n": len(mv), "abs_move_bps_p50_p90_p99_max": [round(st.median(a), 2), round(st.quantiles(a, n=10)[-1], 2), round(st.quantiles(a, n=100)[-1], 2), round(max(a), 2)],
                   "n_moves_ge2bps": len(big), "share_ge2bps_by_cycles": round(sum(x["cycle"] for x in big) / max(1, len(big)), 3),
                   "inter_swap_gap_slots_p50": st.median([x["gap_slots"] for x in mv])}
    print("moves", d["label"], moves[pool])
json.dump(moves, open(f"{D}/background_moves.json", "w"), indent=1)

"""Pool price vs stake-pool NAV around epoch boundaries (from data/price_paths.json). Writes data/boundary_study.json"""
import json, os, statistics as st
D = os.path.join(os.path.dirname(__file__), "../data")
P = json.load(open(f"{D}/price_paths.json"))
SP = json.load(open(f"{D}/stakepools_now.json"))
FM = json.load(open(f"{D}/rate_steps_feemint.json"))
JITO, JUP, MSOL = "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn", "jupSoLaHXQiZZTSfEWMTRRgpnyFm8f6sZdosWBjx93v", "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So"
BOUNDS = [471312000, 471744000, 472176000]
DLMM_BINSTEP = {"BoeMUkCLHchTD31HdXsbDExuZZfcUppSLpYtV3LZTH6U": 1}

def nav_table(name, mint):
    sp = SP[name]
    n = {1093: sp["rate"], 1092: sp["last_epoch_rate"]}
    fm = {r["epoch"]: r for r in FM[name]}
    k = ((sp["rate"] / sp["last_epoch_rate"] - 1) * 1e4) / fm[1093]["implied_step_bps"]  # calibrate fee-mint estimate
    n[1091] = n[1092] / (1 + fm[1092]["implied_step_bps"] * k / 1e4)
    n[1090] = n[1091] / (1 + fm[1091]["implied_step_bps"] * k / 1e4)
    upd = {r["epoch"]: r["update_slot"] for r in FM[name]}
    return n, upd, k

NAV = {JITO: nav_table("JitoSOL", JITO), JUP: nav_table("jupSOL", JUP)}

def nav_at(mint, slot):
    if mint == MSOL:  # approximation: current msol_price 1.419507 at 1093, -1.4 bps/epoch, step at boundary+600
        e = slot // 432000 - (1 if slot % 432000 < 600 else 0)
        return 1.419507154962048 / (1 + 1.4e-4) ** (1093 - e), "approx"
    n, upd, _ = NAV[mint]
    e = slot // 432000
    if slot < upd.get(e, 0): e -= 1  # rate not yet updated on chain
    return n[e], "measured" if e >= 1092 else "fee-mint"

def mid(p, pool):
    """best estimate of the pool's marginal price after the tx (SOL per LST)."""
    if p["post"]: return p["post"], "post"
    if p.get("dlmm") and pool in DLMM_BINSTEP:
        # end bin price is in Y per X = SOL per JitoSOL (x=JitoSOL)
        return (1 + DLMM_BINSTEP[pool] / 1e4) ** p["dlmm"]["end_bin"], "bin"
    return None, None

def main():
    res = {}
    for pool, d in P.items():
        lst = d["lst"]; pts = sorted(d["points"], key=lambda p: (p["slot"], p["idx"] or 0))
        rows = []
        for p in pts:
            m, src = mid(p, pool)
            nav, q = nav_at(lst, p["slot"])
            r = {**{k: p[k] for k in ("slot", "idx", "signer", "side", "lst_amt", "cycle", "nlegs")}, "mid": m, "mid_src": src, "nav": nav, "nav_q": q}
            if m: r["dev_bps"] = (m / nav - 1) * 1e4
            if p["exec"]: r["exec_dev_bps"] = (p["exec"] / nav - 1) * 1e4
            if p["pre"]: r["pre_dev_bps"] = (p["pre"] / nav - 1) * 1e4
            rows.append(r)
        per_b = []
        for B in BOUNDS:
            e = B // 432000
            upd = NAV[lst][1].get(e) if lst in NAV else B + 600
            win = [r for r in rows if abs(r["slot"] - B) <= 15000]
            before = [r for r in win if r["slot"] < B and "dev_bps" in r]
            after = [r for r in win if r["slot"] >= (upd or B) and "dev_bps" in r]
            gap = [r for r in win if B <= r["slot"] < (upd or B)]
            # deviation vs the NEW nav before the update = how far the pool lags the step
            def med(x): return round(st.median(x), 2) if x else None
            first_after = after[:5]
            b = {"boundary": B, "epoch": e, "update_slot": upd, "update_offset": (upd - B) if upd else None,
                 "n_before": len(before), "n_after": len(after), "n_gap": len(gap),
                 "med_dev_before_bps": med([r["dev_bps"] for r in before]),
                 "med_dev_after_bps": med([r["dev_bps"] for r in after]),
                 "last_before": before[-1] if before else None, "first_after": first_after,
                 "swaps_in_first_100_slots_after_update": sum(1 for r in after if r["slot"] - (upd or B) <= 100),
                 "cycles_in_first_300_slots_after_update": [r for r in after if r["slot"] - (upd or B) <= 300 and r["cycle"]]}
            per_b.append(b)
        devs = [r["dev_bps"] for r in rows if "dev_bps" in r]
        qs = st.quantiles(devs, n=20) if len(devs) > 20 else None
        res[pool] = {"label": d["label"], "n": len(rows), "n_with_mid": len(devs), "dev_bps_p5_p50_p95": qs and [round(qs[0], 2), round(st.median(devs), 2), round(qs[-1], 2)],
                     "share_cycles": round(sum(r["cycle"] for r in rows) / max(1, len(rows)), 3), "boundaries": per_b, "rows": rows}
        print(d["label"], "n", len(rows), "with mid", len(devs), "dev p5/p50/p95", res[pool]["dev_bps_p5_p50_p95"], "cycle share", res[pool]["share_cycles"])
        for b in per_b:
            fa = b["first_after"][0] if b["first_after"] else None
            print("   e%d upd+%s n_before %d n_gap %d med_dev_before %s med_dev_after %s | first swap after upd: +%s slots dev %s cycle %s" % (
                b["epoch"], b["update_offset"], b["n_before"], b["n_gap"], b["med_dev_before_bps"], b["med_dev_after_bps"],
                fa and fa["slot"] - b["update_slot"], fa and round(fa.get("dev_bps", 0), 2), fa and fa["cycle"]))
    json.dump(res, open(f"{D}/boundary_study.json", "w"), indent=0)
    print("NAV jito", NAV[JITO][0], "jup", NAV[JUP][0], "k", NAV[JITO][2], NAV[JUP][2])

if __name__ == "__main__":
    main()

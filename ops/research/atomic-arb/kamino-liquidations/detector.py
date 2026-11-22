"""Kamino (klend) price-triggered liquidation detector and profit estimator.

Rule (mirrors klend check_liquidate_obligation / calculate_liquidation, v1.2x):
  per obligation, from refreshed prices:
    U  = sum_i deposit_value_i * liq_threshold_i      (unhealthy_borrow_value; e-mode threshold if in a group)
    B  = sum_j borrow_value_j  * borrow_factor_j      (borrow_factor_adjusted_debt_value)
    liquidatable  <=>  B >= U   (LTV = B/D >= unhealthy LTV = U/D), and price-triggered liquidations enabled
  liquidator must repay the highest-borrow-factor debt and seize the lowest-liquidation-LTV collateral.
  bonus  = min(max(min_bonus, B/D - U/D), max_bonus, 1 - no_bf_ltv)   (bad-debt branch if no_bf_ltv >= 0.99)
  repay  <= close_factor * total_debt_value (100% if no_bf... LTV > insolvency ltv), <= that borrow, <= max_value_at_once
           (full repay forced if the borrow is below min_full_liquidation_value_threshold)
  seized = repay * (1 + bonus), capped at the collateral's value
  protocol_fee = (seized - seized/(1+bonus)) * protocol_liquidation_fee_pct
  net = seized - protocol_fee - repay - flash_fee*repay - swap_cost(seized) - tx_cost
Swap cost model: fee_bps + impact_bps_per_10k * seized_usd/10_000 (linear impact; calibrate per pool).

usage (evaluation on collected data):
  python3 -I detector.py <records.jsonl> <reserves.json> <prestate.json> <liquidations_ltv.json> <out.json> <markets.json>
"""
import sys, json, collections

CLOSE_FACTOR = 0.20          # main market liquidation_max_debt_close_factor_pct (logged: 20)
INSOLVENCY_LTV = 0.95        # docs; market insolvency_risk_unhealthy_ltv_pct
MAX_AT_ONCE_USD = 500_000    # logged liquidation_max_value
MIN_FULL_USD = 0.0           # min_full_liquidation_value_threshold (read from the market account; unknown here)


def assess(deposits, borrows, swap_fee_bps=5.0, impact_bps_per_10k=10.0, tx_cost_usd=0.01,
           flash_fee=1e-5, emode_threshold=None, emode_max_bonus_bps=None, close_factor=CLOSE_FACTOR,
           insolvency_ltv=INSOLVENCY_LTV, max_at_once_usd=MAX_AT_ONCE_USD, min_full_usd=MIN_FULL_USD, tol=1e-6):
    """deposits: [{value, lt, min_bonus_bps, max_bonus_bps, protocol_fee_pct, bad_debt_bps}]
       borrows:  [{value, bf, min_bonus_bps, max_bonus_bps}]   (values in USD at refreshed prices)
       returns dict with health, liquidatable, bonus, repay, seized, net."""
    D = sum(d["value"] for d in deposits)
    U = sum(d["value"] * (emode_threshold or d["lt"]) for d in deposits)
    B = sum(b["value"] * b["bf"] for b in borrows)
    Braw = sum(b["value"] for b in borrows)
    if B <= 0 or D <= 0:
        return {"liquidatable": False, "health": None}
    health = U / B
    # tol absorbs the 4-decimal rounding of logged values; on raw account state use tol=0 (program: LTV >= threshold)
    out = {"health": health, "ltv": B / D, "unhealthy_ltv": U / D, "liquidatable": health <= 1 + tol}
    if not out["liquidatable"]:
        return out
    coll = min(deposits, key=lambda d: (emode_threshold or d["lt"]))      # lowest liquidation LTV
    debt = max(borrows, key=lambda b: b["bf"])                             # highest borrow factor
    no_bf_ltv = Braw / D
    if no_bf_ltv >= 0.99:
        bad = min(coll.get("bad_debt_bps", 99), debt.get("bad_debt_bps", 99)) / 1e4
        bonus = max(bad, 1 - no_bf_ltv) if no_bf_ltv < 1 else bad
    else:
        lo = max(coll["min_bonus_bps"], debt["min_bonus_bps"]) / 1e4
        hi = max(coll["max_bonus_bps"], debt["max_bonus_bps"])
        if emode_max_bonus_bps and emode_max_bonus_bps <= min(coll["max_bonus_bps"], debt["max_bonus_bps"]):
            hi = emode_max_bonus_bps
        hi /= 1e4
        bonus = min(max(lo, B / D - U / D), hi, 1 - no_bf_ltv)
    cf = 1.0 if B / D > insolvency_ltv else close_factor
    repay = min(Braw * cf, debt["value"], max_at_once_usd)
    if debt["value"] < min_full_usd:
        repay = debt["value"]
    seized = min(repay * (1 + bonus), coll["value"])
    if seized < repay * (1 + bonus):
        repay = seized / (1 + bonus)
    pfee = (seized - seized / (1 + bonus)) * coll.get("protocol_fee_pct", 50) / 100
    swap = seized * (swap_fee_bps + impact_bps_per_10k * seized / 10_000) / 1e4
    net = seized - pfee - repay - flash_fee * repay - swap - tx_cost_usd
    out.update({"bonus": bonus, "repay_usd": repay, "seized_usd": seized, "protocol_fee_usd": pfee,
                "swap_cost_usd": swap, "flash_fee_usd": flash_fee * repay, "net_usd": net,
                "liquidator_bonus_bps": (seized - pfee - repay) / repay * 1e4 if repay else None})
    return out


def from_record(r, RES):
    """Build assess() inputs from a parsed tx record (refresh_obligation logs + current reserve configs)."""
    deps, bors, rr = r.get("deposits") or [], r.get("borrows") or [], r.get("refresh_reserves") or []
    if not deps or not bors or len(rr) != len(deps) + len(bors):
        return None
    D, Bs = [], []
    for (s, amt, val), res in zip(deps, rr[:len(deps)]):
        R = RES.get(res)
        if not R: return None
        D.append({"value": val, "lt": R["liq_threshold_pct"] / 100, "min_bonus_bps": R["min_bonus_bps"],
                  "max_bonus_bps": R["max_bonus_bps"], "protocol_fee_pct": R["protocol_liq_fee_pct"],
                  "bad_debt_bps": R["bad_debt_bonus_bps"], "reserve": res})
    for (s, amt, val, vbf), res in zip(bors, rr[len(deps):]):
        R = RES.get(res)
        if not R: return None
        Bs.append({"value": val, "bf": (vbf / val) if val else R["borrow_factor_pct"] / 100,
                   "min_bonus_bps": R["min_bonus_bps"], "max_bonus_bps": R["max_bonus_bps"],
                   "bad_debt_bps": R["bad_debt_bonus_bps"], "reserve": res})
    return D, Bs


if __name__ == "__main__":
    recs = [json.loads(l) for l in open(sys.argv[1])]
    RES = json.load(open(sys.argv[2]))
    MK = json.load(open(sys.argv[6])) if len(sys.argv) > 6 else {"markets": {}, "obligation_group": {}}
    pre = {p["sig"]: p for p in json.load(open(sys.argv[3]))}
    liq = {x["sig"]: x for x in json.load(open(sys.argv[4]))}
    res = collections.Counter(); rows = []
    for r in recs:
        reason = (r.get("log_reason") or [None])[0]
        healthy_fail = bool(r.get("log_healthy"))
        if not ((r["tx_ok"] and reason == "LtvExceeded") or healthy_fail):
            continue
        b = from_record(r, RES)
        if not b:
            res[("no_state", "ok" if r["tx_ok"] else "healthy_fail")] += 1
            continue
        D, Bs = b
        m = MK["markets"].get(r["market"], {})
        g = MK["obligation_group"].get(r["obligation"])
        grp = m.get("elevation_groups", {}).get(str(g)) if g else None
        kw = dict(close_factor=m.get("close_factor_pct", 20) / 100, insolvency_ltv=m.get("insolvency_ltv_pct", 95) / 100,
                  max_at_once_usd=m.get("max_liquidatable_at_once", 5e5), min_full_usd=m.get("min_full_liquidation_value", 0),
                  emode_threshold=grp["liq_threshold_pct"] / 100 if grp else None,
                  emode_max_bonus_bps=grp["max_bonus_bps"] if grp else None)
        a = assess(D, Bs, **kw)
        # program's own verdict
        if r["tx_ok"]:
            truth = True
            el = r.get("log_elig"); prog_health = float(el[1]) / float(el[0]) if el and float(el[0]) > 0 else None
        else:
            truth = False; prog_health = None
        # e-mode fallback: if config thresholds disagree with the program, use the program's effective threshold
        key = ("liquidated" if truth else "healthy_attempt", "flagged" if a["liquidatable"] else "not_flagged")
        res[key] += 1
        row = {"sig": r["sig"], "slot": r["slot"], "truth": truth, "flag": a["liquidatable"], "health_est": a.get("health"),
               "health_prog": prog_health, "net_usd_est": a.get("net_usd"), "repay_usd_est": a.get("repay_usd"),
               "bonus_est": a.get("bonus"), "signer": r["signer"]}
        if truth and r["sig"] in liq:
            x = liq[r["sig"]]
            row.update({"repaid_usd_obs": x["repaid_usd"], "bonus_bps_obs": x["bonus_bps"],
                        "profit_usd_obs": x["profit_usd_signer"], "dex": x["dex"]})
        if r["sig"] in pre:
            p = pre[r["sig"]]
            row.update({k: p.get(k) for k in ("status", "lag_slots", "trigger_is_liq_tx", "trigger_src_age_slots")})
        rows.append(row)
    json.dump({"counts": [[list(k), v] for k, v in res.items()], "rows": rows}, open(sys.argv[5], "w"), indent=0)
    for k, v in sorted(res.items()): print(k, v)
    ok = [x for x in rows if x["truth"] and x.get("repaid_usd_obs") and x.get("repay_usd_est")]
    rat = sorted(x["repay_usd_est"] / x["repaid_usd_obs"] for x in ok)
    if rat:
        print("repay_est/repay_obs n=%d p10 %.2f p50 %.2f p90 %.2f" % (len(rat), rat[len(rat)//10], rat[len(rat)//2], rat[9*len(rat)//10]))
    bon = collections.Counter((round((x["bonus_est"] or 0) * 1e4), x["bonus_bps_obs"]) for x in ok)
    print("bonus est vs obs (bps):", bon.most_common(8))
    hp = [(x["health_est"], x["health_prog"]) for x in rows if x["truth"] and x["health_prog"]]
    err = sorted(abs(a / b - 1) for a, b in hp if a and b)
    if err: print("health est vs program |rel err| p50 %.2e p90 %.2e max %.2e" % (err[len(err)//2], err[9*len(err)//10], err[-1]))

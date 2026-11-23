"""Per-arb reconstruction: trigger, prices before/after, optimum, competition. -> data/cases.json, data/cases.csv"""
import json, os, sys, csv, statistics
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import replay, arbmath, lib
D = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "data")
JUP = lib.JUP6

def gapbps(a, b): return (max(a, b) / min(a, b) - 1) * 1e4

def main():
    allblocks = replay.load(os.path.join(D, "cases.jsonl"))
    R = {}
    for f in ("top200", "last200"):
        for x in json.load(open("/tmp/arb-research/round1/%s.recs.json" % f)): R[x["sig"]] = x
    sel = json.load(open(os.path.join(D, "selected_sigs.json")))
    rows = []
    for sig in sel:
        slot0 = R[sig]["slot"]
        blocks = {s_: b for s_, b in allblocks.items() if slot0 - 8 <= s_ <= slot0 + 3}
        tl = replay.timeline(blocks)
        pos = {t["tx"]["sig"]: i for i, t in enumerate(tl)}
        if sig not in pos: print("missing", sig[:8]); continue
        i = pos[sig]; a = tl[i]; tx = a["tx"]
        P = list(dict.fromkeys(e["pool"] for e in a["evs"]))
        if len(P) != 2: continue
        # state just before each tx
        last = {}; hist = []   # hist: (k, {pool: px})
        # seed: price at window start = pre-price of each pool's first event in the window (constant before it)
        for t_ in tl[:i + 1]:
            for e in t_["evs"]:
                if e["pool"] in P and e["pool"] not in last: last[e["pool"]] = e["pre"]
        Lest = {}
        others = {}; others_start = {}
        for k in range(0, i):
            for e in tl[k]["evs"]:
                if e["pool"] in P:
                    last[e["pool"]] = e["post"]
                    if e["L"]: Lest[e["pool"]] = e["L"]
                elif tl[k]["slot"] >= a["slot"] - 8:
                    others[e["pool"]] = e["post"]
                    if tl[k]["slot"] <= a["slot"] - 6: others_start[e["pool"]] = e["post"]
            hist.append((k, dict(last)))
        if len(last) < 2:
            # fall back: pre price of arb's own event for the pool lacking history
            for e in a["evs"]:
                last.setdefault(e["pool"], e["pre"])
        pre_arb = dict(last)
        g_pre_arb = gapbps(*[pre_arb[p] for p in P])
        # trigger = most recent tx whose execution took the gap from below the fee band to above it
        band = sum(replay.fee_of(p) for p in P) * 1e4
        trig = None
        for k in range(i - 1, -1, -1):
            if not any(e["pool"] in P for e in tl[k]["evs"]): continue
            before = hist[k - 1][1] if k > 0 else {}
            after = hist[k][1]
            if len(after) < 2: break
            ga = gapbps(*[after[p] for p in P])
            if len(before) < 2:
                trig = k if ga >= band else None; break
            gb = gapbps(*[before[p] for p in P])
            if gb < band + 0.5 <= ga:
                trig = k; break
        if trig is None:  # gap already above band at window start: take the largest gap jump in the window
            best = 0.0
            for k in range(0, i):
                if not any(e["pool"] in P for e in tl[k]["evs"]) or k == 0: continue
                b_, a_ = hist[k - 1][1], hist[k][1]
                if len(b_) == 2 and len(a_) == 2:
                    j = gapbps(*[a_[p] for p in P]) - gapbps(*[b_[p] for p in P])
                    if j > best: best, trig = j, k
        L = {}
        for e in a["evs"]:
            if e["L"]: L[e["pool"]] = e["L"]
        for p in P: L.setdefault(p, Lest.get(p))
        ss = {p: (pre_arb[p] / 1e3) ** 0.5 for p in P}
        fees = {p: replay.fee_of(p) for p in P}
        opt = arbmath.optimal((ss[P[0]], L[P[0]], fees[P[0]]), (ss[P[1]], L[P[1]], fees[P[1]])) if all(L.values()) else {}
        d = tx["d"]; gross = d["sol"] + tx["fee"] + tx["tip"]
        prio = max(tx["fee"] - 5e-6, 0.0)
        post_arb = dict(pre_arb)
        for e in a["evs"]: post_arb[e["pool"]] = e["post"]
        row = dict(sig=sig, slot=a["slot"], idx=a["idx"], ntx=blocks[a["slot"]]["ntx"], signer=tx["signer"],
                   explorer_profit=R[sig]["profit"], measured_gross_sol=round(gross, 5), net_sol=round(d["sol"], 5),
                   tx_fee=tx["fee"], tip=tx["tip"], prio_sol=round(prio, 6), cu=tx["cu"],
                   leader_share=round((prio + tx["tip"]) / gross, 3) if gross > 0 else None,
                   arb_progs=[p_[:8] for p_ in tx["progs"]], first_pool=a["evs"][0]["pool"][:6],
                   pools=[p[:6] for p in P], arb_in_sol=round(abs(a["evs"][0]["sol_in"]) if a["evs"][0]["sol_in"] > 0 else abs(a["evs"][-1]["sol_in"]), 3),
                   px_pre_arb={p[:6]: round(pre_arb[p], 4) for p in P}, gap_pre_arb_bps=round(g_pre_arb, 2),
                   px_post_arb={p[:6]: round(post_arb[p], 4) for p in P}, gap_post_arb_bps=round(gapbps(*[post_arb[p] for p in P]), 2),
                   fees_bps={p[:6]: fees[p] * 1e4 for p in P},
                   L={p[:6]: (int(L[p]) if L[p] else None) for p in P},
                   opt_in_sol=round(opt.get("x_sol", 0), 2), opt_profit_sol=round(opt.get("profit_sol", 0), 4),
                   capture=round(gross / opt["profit_sol"], 3) if opt.get("profit_sol") else None,
                   other_pools_median_px=round(statistics.median(others.values()), 4) if others else None,
                   n_other_pools=len(others),
                   other_pools_median_px_8slots_before=round(statistics.median(others_start.values()), 4) if others_start else None)
        if others:
            med = row["other_pools_median_px"]
            row["dev_from_consensus_bps"] = {p[:6]: round((pre_arb[p] / med - 1) * 1e4, 1) for p in P}
        if trig is not None:
            t = tl[trig]; tt = t["tx"]
            before = hist[trig - 1][1] if trig > 0 else {}
            ev = [e for e in t["evs"] if e["pool"] in P]
            row.update(trig_sig=tt["sig"], trig_slot=t["slot"], trig_idx=t["idx"], trig_ntx=blocks[t["slot"]]["ntx"],
                       trig_signer=tt["signer"], trig_jup=JUP in tt["progs"] + tt["inner"],
                       trig_pools=[e["pool"][:6] for e in ev], trig_sol_in=round(sum(e["sol_in"] for e in ev), 3),
                       trig_usd=round(abs(sum(e["sol_in"] for e in ev)) * ev[0]["pre"], 0),
                       trig_n_pool_events=len(t["evs"]), trig_signer_d={k: round(v, 3) for k, v in tt["d"].items()},
                       trig_tip=tt["tip"], trig_fee=tt["fee"], trig_progs=[p[:6] for p in tt["progs"]],
                       px_pre_trig={p[:6]: round(before.get(p, 0), 4) for p in P},
                       gap_pre_trig_bps=round(gapbps(*[before[p] for p in P]), 2) if len(before) == 2 else None,
                       px_post_trig={p[:6]: round(hist[trig][1].get(p, 0), 4) for p in P},
                       slots_after_trig=a["slot"] - t["slot"])
            # positions between
            row["txs_between_same_slot"] = (a["idx"] - t["idx"]) if a["slot"] == t["slot"] else None
        # competition in arb slot (after the trigger if same slot)
        lo = row.get("trig_idx", -1) if row.get("trig_slot") == a["slot"] else -1
        comp = [x for x in blocks[a["slot"]]["txs"] if x["idx"] > lo and x["sig"] != sig and set(x["touch"]) & set(P)]
        row["same_slot_failed"] = sum(1 for x in comp if x["err"])
        row["same_slot_noop_ok"] = sum(1 for x in comp if not x["err"] and not x["ev"])
        row["same_slot_other_arbs"] = sum(1 for x in comp if not x["err"] and len({e["pool"] for e in x["ev"]} & set(P)) == 2)
        row["same_slot_before_arb_failed"] = sum(1 for x in comp if x["err"] and x["idx"] < a["idx"])
        row["top_of_block"] = not any(x["ev"] and set(e["pool"] for e in x["ev"]) & set(P) for x in blocks[a["slot"]]["txs"] if x["idx"] < a["idx"])
        rows.append(row)
    json.dump(rows, open(os.path.join(D, "cases.json"), "w"), indent=1)
    keys = ["slot", "idx", "ntx", "signer", "explorer_profit", "measured_gross_sol", "opt_profit_sol", "capture", "arb_in_sol", "opt_in_sol",
            "gap_pre_trig_bps", "gap_pre_arb_bps", "gap_post_arb_bps", "trig_slot", "trig_idx", "slots_after_trig", "txs_between_same_slot",
            "trig_signer", "trig_jup", "trig_pools", "trig_sol_in", "trig_usd", "trig_n_pool_events", "tip", "tx_fee", "prio_sol", "leader_share", "cu",
            "same_slot_failed", "same_slot_noop_ok", "same_slot_other_arbs", "top_of_block", "sig", "trig_sig"]
    with open(os.path.join(D, "cases.csv"), "w") as f:
        w = csv.DictWriter(f, keys, extrasaction="ignore"); w.writeheader(); [w.writerow(r) for r in rows]
    for r in rows:
        print(r["slot"], r["idx"], r["signer"][:6], "prof", r["explorer_profit"].__round__(3), r["measured_gross_sol"], "opt", r["opt_profit_sol"], r["capture"],
              "in", r["arb_in_sol"], r["opt_in_sol"], "gap", r.get("gap_pre_trig_bps"), r["gap_pre_arb_bps"], r["gap_post_arb_bps"],
              "| trig", r.get("trig_slot"), r.get("trig_idx"), r.get("trig_signer", "")[:6], r.get("trig_jup"), r.get("trig_pools"), r.get("trig_sol_in"), r.get("trig_usd"),
              "| dslot", r.get("slots_after_trig"), "dpos", r.get("txs_between_same_slot"), "tob", r["top_of_block"], "comp", r["same_slot_failed"], r["same_slot_noop_ok"], r["same_slot_other_arbs"], "tip", r["tip"])

if __name__ == "__main__":
    main()

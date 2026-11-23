"""Ordering stats for validation/sol_usdc_cases.json and kamino_contests.json."""
import json, collections
def same_slot(case):
    w = next(c for c in case["contenders"] if c["is_winner"])
    return w, [c for c in case["contenders"] if c["slot"] == w["slot"]]
def report(name, cases, subset=None):
    S = collections.Counter(); gaps = []; gaps_incl = []; idx_rel = []
    for case in cases:
        w, ss = same_slot(case)
        if subset: ss = [c for c in ss if c["is_winner"] or subset(c)]
        others = [c for c in ss if not c["is_winner"]]
        S["n"] += 1
        S["n_with_same_slot_others"] += bool(others)
        S["contenders_total"] += len(case["contenders"]); S["same_slot_others"] += len(others)
        S["others_tipped_any"] += sum(c["tip_lamports"] > 0 for c in case["contenders"] if not c["is_winner"])
        S["others_failed_any"] += sum(not c["succeeded"] for c in case["contenders"] if not c["is_winner"])
        for key, tag in (("pay_per_cu", ""), ("pay_per_cu_incl_base", "_base")):
            mx = max([c[key] for c in others], default=-1)
            S["a_highest" + tag] += w[key] > mx
            S["a_tied" + tag] += w[key] == mx
            rk = lambda c: (c["tip_lamports"] > 0, c[key])
            mxb = max([rk(c) for c in others], default=(False, -1))
            S["b_highest_tipfirst" + tag] += rk(w) > mxb
        S["c_earliest"] += all(w["index"] < c["index"] for c in others)
        S["c_earliest_among_succeeded"] += all(w["index"] < c["index"] for c in others if c["succeeded"])
        hi_after = [c for c in others if c["pay_per_cu"] > w["pay_per_cu"] and c["index"] > w["index"]]
        hi_before = [c for c in others if c["pay_per_cu"] > w["pay_per_cu"] and c["index"] < w["index"]]
        S["cases_with_higher_payer_after"] += bool(hi_after)
        S["higher_payers_after"] += len(hi_after); S["higher_payers_before"] += len(hi_before)
        S["higher_payers_after_tipped"] += sum(c["tip_lamports"] > 0 for c in hi_after)
        gaps += [c["index"] - w["index"] for c in hi_after]
        hb = [c for c in others if c["pay_per_cu_incl_base"] > w["pay_per_cu_incl_base"] and c["index"] > w["index"]]
        S["higher_payers_after_incl_base"] += len(hb)
        S["winner_pays_0_above_base"] += (w["priority_lamports"] + w["tip_lamports"]) == 0
        S["winner_tips"] += w["tip_lamports"] > 0
        S["winner_bundle_guess_nonempty"] += bool(w["bundle_guess"])
        S["winner_v1_tx"] += w.get("tx_version") == 1
        if "trigger" in case and "index" in case["trigger"]:
            idx_rel.append(case.get("winner_index_minus_trigger") if case["trigger"]["slot"] == w["slot"] else f"next:{w['index']}")
        else:
            t = case["trigger"]
            if t["scope_refresh_in_winner_tx"]: idx_rel.append("same_tx")
            elif t["latest_scope_refreshes_earlier_in_block"]: idx_rel.append(w["index"] - t["latest_scope_refreshes_earlier_in_block"][0]["index"])
            else: idx_rel.append("none_in_block")
    print(f"== {name}"); print(dict(S))
    print("higher payer after winner, index gaps:", sorted(gaps))
    print("winner index relative to trigger:", idx_rel)
    return S
if __name__ == "__main__":
    sol = json.load(open("/tmp/arb-research/validation/sol_usdc_cases.json"))
    kam = json.load(open("/tmp/arb-research/validation/kamino_contests.json"))
    report("sol_usdc all", sol)
    report("sol_usdc explorer only", [c for c in sol if c["source"] == "explorer_top200"])
    report("sol_usdc all, only contenders writing both W and R", sol, lambda c: c["writes"] == ["R", "W"])
    report("kamino all", kam)
    report("kamino tier0", [c for c in kam if c["source"].endswith("tier 0")])

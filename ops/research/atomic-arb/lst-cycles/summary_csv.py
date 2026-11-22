"""Join cycles + triggers + detector into data/round1_multitoken_arbs.csv (one row per multi-token cycle)."""
import csv, json, os
D = os.path.join(os.path.dirname(__file__), "../data")
T = {t["sig"]: t for t in json.load(open(f"{D}/round1_triggers.json"))}
C = {c["sig"]: c for c in json.load(open(f"{D}/round1_cycles.json"))}
R = {r["sig"]: r for r in json.load(open(f"{D}/detector_results.json"))["cycles"]}
with open(f"{D}/round1_multitoken_arbs.csv", "w", newline="") as fh:
    w = csv.writer(fh)
    w.writerow(["set", "sig", "slot", "slot_in_epoch", "slots_to_nearest_boundary", "tx_index", "signer", "profit_sol_explorer", "tip_sol", "fee_sol", "cu",
                "path", "venues", "pools", "lst", "size_in_sol", "edge_bps_exec", "mispriced_pool(trigger)", "mispriced_venue", "trigger_impact_bps",
                "trigger_swap", "trigger_slot_lag", "trigger_same_block_before", "trigger_signer", "prev_tx_is_cycle_within2", "detector_A_hit", "detector_A_profit", "detector_B_hit"])
    for sig, t in sorted(T.items(), key=lambda kv: (kv[1]["set"], -kv[1]["profit_sol"])):
        c = C[sig]; r = R.get(sig, {}); tr = t.get("trigger") or {}; pa = t.get("prev_any") or {}
        sie = c["slot_in_epoch"]; near = sie if sie < 216000 else sie - 432000
        w.writerow([t["set"], sig, c["slot"], sie, near, t["idx"], c["signer"], round(c["profit_sol"], 6), c["tip"], c["fee"], c["cu"],
                    ">".join(c["path"]), "|".join(c["venues"]), "|".join(c["pools"]), "|".join(c["lst"]),
                    round(c["legs"][0]["amt_in"] / 1e9, 4) if c["legs"] and c["path"][0] == "SOL" else "", round(t["edge_bps_exec"], 2),
                    tr.get("pool", ""), tr.get("venue", ""), tr.get("impact_bps_on_pool") and round(tr["impact_bps_on_pool"], 1),
                    tr and f'{tr["in"][:5]}>{tr["out"][:5]} {tr["amt_in_ui"]:.4g}', tr.get("slot_lag", ""), tr.get("same_block_before", ""), tr.get("signer", ""),
                    bool(pa and pa["is_cycle"] and pa["slot_lag"] <= 2), r.get("A_hit"), r.get("A_top") and r["A_top"]["profit"], r.get("B_hit") if r.get("B_tested") else ""])
print("ok")

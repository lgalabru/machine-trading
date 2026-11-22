"""Print the pool-event timeline around a slot. usage: show.py SLOT [n_before] [file]"""
import sys, os
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import replay
slot = int(sys.argv[1]); nb = int(sys.argv[2]) if len(sys.argv) > 2 else 3
f = sys.argv[3] if len(sys.argv) > 3 else os.path.join(os.path.dirname(__file__), "..", "data", "cases.jsonl")
W, R = "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE", "3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv"
bl = replay.load(f)
bl = {s: b for s, b in bl.items() if slot - 8 <= s <= slot + 3}
last = {}
for t in replay.timeline(bl):
    for e in t["evs"]:
        last[e["pool"]] = e["post"]
    if t["slot"] < slot - nb or t["slot"] > slot: continue
    tx = t["tx"]
    if not any(e["pool"] in (W, R) for e in t["evs"]) and not (set(tx["touch"]) & {W, R}): continue
    if not t["evs"] and tx["err"]: continue
    gap = (last[W] / last[R] - 1) * 1e4 if W in last and R in last else None
    print(t["slot"], t["idx"], tx["signer"][:6], "ERR" if tx["err"] else "", "JUP" if replay.lib.JUP6 in tx["progs"] + tx["inner"] else "",
          [(e["prog"], e["pool"][:5], round(e["sol_in"], 2), round(e["pre"], 3), round(e["post"], 3)) for e in t["evs"]],
          {k: round(v, 3) for k, v in tx["d"].items()}, "tip", round(tx["tip"], 4), "gapW/R", None if gap is None else round(gap, 1))

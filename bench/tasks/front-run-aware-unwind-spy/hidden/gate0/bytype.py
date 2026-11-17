"""Each rung's mean PnL by predator type, from a ladder set.

Reads `<set>/<rung>.json` and the predator each session drew from
`<set>/details/<any>/bot/<session>.json.gz` (its `annotation.shape`), and
how many times the predator traded ahead of each rung where the report
keeps the actors' tallies.

Usage: python3 bytype.py <ladder-out>/<task>/set-1
"""

import glob
import gzip
import json
import os
import sys
from collections import defaultdict


def main():
    root = sys.argv[1]
    kinds = {}
    for path in glob.glob(os.path.join(root, "details", "*", "bot", "*.json.gz")):
        with gzip.open(path) as f:
            d = json.load(f)
        kinds[d["scenario"]["name"]] = d["annotation"]["shape"]["predator"]
    rungs = {}
    fronts = {}
    backstop = {}
    for path in sorted(glob.glob(os.path.join(root, "*.json"))):
        name = os.path.basename(path)[:-5]
        with open(path) as f:
            report = json.load(f)
        if "scenarios" not in report:
            continue
        rungs[name] = {s["scenario"]: s["pnl_usd"] for s in report["scenarios"]}
        counts = {}
        for s in report["scenarios"]:
            for actor in (s.get("actors") or {}).get("actors", []):
                if actor.get("kind") == "front-runner":
                    counts[s["scenario"]] = actor.get("counts", {}).get("front_runs", 0)
        fronts[name] = counts
        book = {}
        for s in report["scenarios"]:
            fills = s.get("making_fills") or []
            total = sum(f["base"] for f in fills)
            back = sum(f["base"] for f in fills if f["counterparty"] == "book")
            book[s["scenario"]] = (back, total)
        backstop[name] = book
    types = sorted(set(kinds.values()))
    print(f"{'rung':14}" + "".join(f"{t[:12]:>21}" for t in types) + f"{'all':>10}")
    for name, by in rungs.items():
        cells = []
        for t in types:
            xs = [v for s, v in by.items() if kinds.get(s) == t]
            fr = [fronts[name].get(s, 0) for s in by if kinds.get(s) == t]
            mean = sum(xs) / len(xs) if xs else 0.0
            bk = [backstop[name].get(s, (0, 0)) for s in by if kinds.get(s) == t]
            sold = sum(b[1] for b in bk)
            share = sum(b[0] for b in bk) / sold if sold else 0.0
            cells.append(f"{mean:7.2f}/{sum(fr) / max(len(fr), 1):3.0f}/{sold / max(len(bk), 1):4.1f}/{share:3.1f}")
        print(f"{name:14}" + "".join(f"{c:>21}" for c in cells) + f"{sum(by.values()):10.1f}")
    print("cells: mean PnL / front-runs a session / SPY sold a session / share sold into the backstop")


if __name__ == "__main__":
    main()

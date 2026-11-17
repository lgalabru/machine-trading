"""Gate 0 on a ladder run: the signal gate, the strength band and the order.

Reads `<ladder-out>/<task>/set-*/<rung>.json` (each a `mtx grade --draw`
report on one hidden set) and pools every set's sessions, paired by session.
Prints, per rung, the total, mean and sd of PnL, then:

- the signal gate: the informed bot's total over the best non-reading rung's
  (that rung counted as 0 when flat or losing);
- the strength band: the reference's PnL weighted as scoring v2 weighs it
  (losses `K` times) over the informed bot's, and its per-session Sharpe;
- the order: how often `max(non-reading, naive) < reference < informed` and
  the 2x gate hold on `count`-session sets bootstrapped from the pool.

Usage: python3 gate0.py <ladder-out>/<task> [--count 72] [--draws 2000] [--seed 1]
"""

import argparse
import glob
import json
import os
import random
import statistics

K = 1.5
NON_READING = ["nop", "follow-flow", "fade-flow", "momentum"]
NAIVE = ["most-active"]


def load(root):
    rungs = {}
    for set_dir in sorted(glob.glob(os.path.join(root, "set-*"))):
        reports = {}
        for path in glob.glob(os.path.join(set_dir, "*.json")):
            name = os.path.basename(path)[:-5]
            with open(path) as f:
                report = json.load(f)
            if "scenarios" in report:
                reports[name] = {s["scenario"]: s for s in report["scenarios"]}
        if "reference" not in reports or "informed" not in reports:
            continue
        names = sorted(reports["reference"])
        for rung, by_name in reports.items():
            if set(by_name) != set(names):
                continue
            out = rungs.setdefault(rung, [])
            for n in names:
                s = by_name[n]
                out.append(0.0 if s.get("disqualified") else s["pnl_usd"])
    n = len(rungs.get("reference", []))
    return {k: v for k, v in rungs.items() if len(v) == n}


def weighted(xs):
    return sum(x if x > 0 else K * x for x in xs)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("root")
    parser.add_argument("--count", type=int, default=72)
    parser.add_argument("--draws", type=int, default=2000)
    parser.add_argument("--seed", type=int, default=1)
    args = parser.parse_args()
    rungs = load(args.root)
    n = len(rungs["reference"])
    print(f"{n} sessions, {len(rungs)} rungs")
    print(f"{'rung':16}{'total':>10}{'mean':>9}{'sd':>9}{'win%':>7}{'v2w':>10}")
    for name in sorted(rungs, key=lambda r: sum(rungs[r])):
        xs = rungs[name]
        sd = statistics.pstdev(xs) if len(xs) > 1 else 0.0
        wins = sum(x > 0 for x in xs) / len(xs)
        print(f"{name:16}{sum(xs):10.0f}{statistics.mean(xs):9.2f}{sd:9.2f}{100 * wins:7.0f}{weighted(xs):10.0f}")
    present = [r for r in NON_READING if r in rungs]
    best = max(present, key=lambda r: sum(rungs[r]))
    base = max(sum(rungs[best]), 0.0)
    told = sum(rungs["informed"])
    ratio = told / base if base > 0 else float("inf")
    print(f"\nsignal gate: informed {told:.0f} / best non-reading ({best}) {base:.0f} = {ratio:.2f} (need >= 2)")
    ref = rungs["reference"]
    band = weighted(ref) / weighted(rungs["informed"])
    sharpe = statistics.mean(ref) / statistics.pstdev(ref)
    print(f"strength band: reference v2-weighted / informed's = {band:.2f} (band 0.2-0.7); raw {sum(ref) / told:.2f}")
    print(f"reference per-session Sharpe {sharpe:.2f} (need >= 0.25)")
    for name in rungs:
        if name not in ("informed",):
            print(f"  {name} v2-weighted share of informed: {weighted(rungs[name]) / weighted(rungs['informed']):.2f}")

    rng = random.Random(args.seed)
    below = present + [r for r in NAIVE if r in rungs]
    order = gate = both = 0
    for _ in range(args.draws):
        idx = [rng.randrange(n) for _ in range(args.count)]
        t = {r: sum(rungs[r][i] for i in idx) for r in rungs}
        o = max(t[r] for r in below) < t["reference"] < t["informed"]
        g = t["informed"] >= 2 * max(max(t[r] for r in present), 0.0) and t["informed"] > 0
        order += o
        gate += g
        both += o and g
    print(f"\non {args.draws} bootstrapped {args.count}-session sets:")
    print(f"  order max({', '.join(below)}) < reference < informed holds on {order / args.draws:.3f} (need >= 0.85)")
    print(f"  informed >= 2x best non-reading on {gate / args.draws:.3f}")
    print(f"  both on {both / args.draws:.3f}")


if __name__ == "__main__":
    main()

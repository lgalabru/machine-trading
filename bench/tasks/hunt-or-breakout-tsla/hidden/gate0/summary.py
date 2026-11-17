"""Gate 0 on the engine: reads each rung's `mtx grade` report from one hidden
set (every rung graded on the same sessions) and checks the card's bar.

    python3 summary.py <dir with <rung>.json> [resamples] [seed]

- the informed bot makes at least 2x the best non-reading rung's total PnL
  (a flat or losing rung counts as 0);
- the ladder's order (every non-reading rung and nop < reference < informed)
  holds on at least 85% of bootstrapped sets (sessions resampled, paired);
- the reference's PnL weighted as scoring v2 weighs it (losses k = 1.5
  times), as a share of the informed bot's: the strength band is 0.2-0.7;
- the reference's per-session Sharpe (at least 0.25).
"""

import json
import os
import random
import statistics
import sys

NON_READING = ["fade", "follow"]
PARTIAL = ["wait-oracle", "push-fade"]
K = 1.5


def load(path):
    with open(path) as f:
        report = json.load(f)
    out = {}
    for s in report["scenarios"]:
        session = s.get("session", s)
        name = session.get("scenario") or s.get("scenario")
        pnl = session.get("pnl_usd", s.get("pnl_usd"))
        out[name] = (pnl, bool(s.get("disqualified") or session.get("disqualified")))
    return out


def weighted(xs):
    return sum(x if x >= 0 else K * x for x in xs)


def main():
    folder = sys.argv[1]
    resamples = int(sys.argv[2]) if len(sys.argv) > 2 else 2000
    rng = random.Random(int(sys.argv[3]) if len(sys.argv) > 3 else 7)
    rungs = {}
    for name in ["informed", "reference"] + NON_READING + PARTIAL:
        path = os.path.join(folder, f"{name}.json")
        if os.path.exists(path):
            rungs[name] = load(path)
    sessions = sorted(set.intersection(*(set(r) for r in rungs.values())))
    pnl = {n: [r[s][0] for s in sessions] for n, r in rungs.items()}
    print(f"sessions {len(sessions)}")
    print(f"{'rung':<12}{'total':>10}{'mean':>9}{'sd':>9}{'won':>6}{'dq':>4}")
    for n, xs in pnl.items():
        dq = sum(rungs[n][s][1] for s in sessions)
        won = sum(x > 0 for x in xs)
        print(f"{n:<12}{sum(xs):>10.1f}{statistics.mean(xs):>9.2f}{statistics.pstdev(xs):>9.2f}{won:>6}{dq:>4}")
    non = [n for n in NON_READING if n in pnl]
    best = max([max(sum(pnl[n]), 0.0) for n in non] + [0.0])
    told = sum(pnl["informed"])
    ratio = told / best if best > 0 else float("inf")
    print(f"informed / best non-reading ({max(non, key=lambda n: sum(pnl[n]))}): "
          f"{ratio:.2f} (need >= 2)")

    def order(idx):
        tot = {n: sum(pnl[n][i] for i in idx) for n in pnl}
        floor = max([tot[n] for n in non] + [0.0])
        return floor < tot["reference"] < tot["informed"]

    def doubles(idx):
        tot = {n: sum(pnl[n][i] for i in idx) for n in pnl}
        floor = max([tot[n] for n in non] + [0.0])
        return tot["informed"] > 0 and tot["informed"] >= 2 * floor

    n = len(sessions)
    draws = [[rng.randrange(n) for _ in range(n)] for _ in range(resamples)]
    if "reference" in pnl:
        print(f"order (non-reading, nop < reference < informed) on the set: {order(range(n))}; "
              f"holds on {sum(order(d) for d in draws) / resamples:.3f} of {resamples} "
              f"bootstrapped sets (need >= 0.85)")
    print(f"informed >= 2x best non-reading on {sum(doubles(d) for d in draws) / resamples:.3f} "
          f"of bootstrapped sets")
    w_told = weighted(pnl["informed"])
    shares = ", ".join(f"{m} {weighted(xs) / w_told:.2f}" for m, xs in pnl.items())
    print(f"v2 weighted (k = {K}), as a share of informed: {shares}")
    if "reference" in pnl:
        ref = pnl["reference"]
        sd = statistics.pstdev(ref)
        print(f"reference per-session Sharpe {statistics.mean(ref) / sd if sd else 0:.2f} (need >= 0.25)")
    if "push-fade" in pnl:
        beat = sum(sum(pnl["informed"][i] for i in d) > sum(pnl["push-fade"][i] for i in d) for d in draws)
        print(f"informed > push-fade (the first screen's bot, untold) on {beat / resamples:.3f} of bootstrapped sets")


if __name__ == "__main__":
    main()

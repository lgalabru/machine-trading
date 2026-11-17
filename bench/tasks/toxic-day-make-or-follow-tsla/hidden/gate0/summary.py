"""Gate 0 on the engine: reads each rung's `mtx grade` report from one hidden
set (every rung graded on the same sessions) and checks the card's bar.

    python3 summary.py <dir with <rung>/report.json> [resamples] [seed]

- the informed bot makes at least 2x the best non-reading rung's total PnL
  (a flat or losing rung counts as 0);
- the ladder's order (every non-reading rung and nop < reference < informed)
  holds on at least 85% of bootstrapped sets (sessions resampled, paired);
- the reference's PnL weighted as scoring v2 weighs it (losses k = 1.5
  times), as a share of the informed bot's: the strength band is 0.2-0.7;
- the reference's per-session Sharpe (at least 0.25).

It also splits each rung by the session's kind of morning, read from the
actors' tallies in the reports.
"""

import json
import os
import random
import statistics
import sys

NON_READING = ["flat", "make", "follow", "switch", "burst"]
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
    return out, report


def shapes(report):
    """Each session's kind of morning, from its actors' tallies: toxic when
    the informed fund traded more than the algo (the busy one trades four
    times the other's pace)."""
    out = {}
    for s in report.get("scenarios", []):
        name = s.get("scenario") or s.get("session", {}).get("scenario")
        actors = (s.get("actors") or {}).get("actors") or []
        volume = {a["kind"]: a.get("bought", 0) + a.get("sold", 0) for a in actors}
        if name and ("informed" in volume or "algo" in volume):
            toxic = volume.get("informed", 0) > volume.get("algo", 0)
            out[name] = {"day": "toxic" if toxic else "quiet"}
    return out


def weighted(xs):
    return sum(x if x >= 0 else K * x for x in xs)


def main():
    folder = sys.argv[1]
    resamples = int(sys.argv[2]) if len(sys.argv) > 2 else 2000
    rng = random.Random(int(sys.argv[3]) if len(sys.argv) > 3 else 7)
    rungs, reports = {}, {}
    for name in ["informed", "reference"] + NON_READING:
        path = os.path.join(folder, name, "report.json")
        if os.path.exists(path):
            rungs[name], reports[name] = load(path)
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

    drawn = {}
    for r in reports.values():
        drawn.update(shapes(r))
    if drawn:
        groups = {
            "toxic": [i for i, s in enumerate(sessions) if drawn.get(s, {}).get("day") == "toxic"],
            "quiet": [i for i, s in enumerate(sessions) if drawn.get(s, {}).get("day") == "quiet"],
        }
        print("mean PnL by kind of session:")
        print(f"{'group':<10}{'n':>4}" + "".join(f"{m:>11}" for m in pnl))
        for g, idx in groups.items():
            if idx:
                print(f"{g:<10}{len(idx):>4}" + "".join(
                    f"{statistics.mean(pnl[m][i] for i in idx):>11.2f}" for m in pnl))


if __name__ == "__main__":
    main()

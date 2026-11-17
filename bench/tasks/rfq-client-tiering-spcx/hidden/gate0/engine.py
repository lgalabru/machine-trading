"""Gate 0 on the engine: each rung's per-session PnL from `mtx grade --draw` reports, one set per directory.

    python3 engine.py <set dir> [<set dir> ...]

Each set dir holds `<rung>.json` reports from one fresh hidden set. Prints per set and pooled: each rung's
total, the informed bot over the best careful rung that doesn't read who is who, how often the ladder's
order holds on bootstrapped 72-session sets, and the reference's v2-weighted PnL as a share of the
informed bot's.
"""

import json
import os
import random
import statistics
import sys

CAREFUL = ["quote-all", "quote-wide", "fixed-width", "size-tier", "side-skew"]
K = 1.5


def load(set_dir):
    out = {}
    for name in sorted(os.listdir(set_dir)):
        if name.endswith(".json"):
            with open(os.path.join(set_dir, name)) as f:
                report = json.load(f)
            out[name[:-5]] = [
                (0.0 if s.get("disqualified") else s["pnl_usd"]) for s in report["scenarios"]
            ]
    return out


def checks(p, idx):
    t = {n: sum(p[n][i] for i in idx) for n in p}
    careful = [n for n in CAREFUL if n in t]
    mc = max(t[n] for n in careful)
    return dict(
        nop_lt_ref=t.get("nop", 0.0) < t["reference"],
        naive_lt_ref=max(t.get("quote-all", -1e9), t.get("quote-wide", -1e9)) < t["reference"],
        careful_lt_ref=mc < t["reference"],
        ref_lt_told=t["reference"] < t["informed"],
        told_2x=t["informed"] >= 2 * max(mc, 0.0),
    )


def report(label, p):
    n = len(p["reference"])
    print(f"== {label}: {n} sessions")
    for name, x in sorted(p.items(), key=lambda kv: sum(kv[1])):
        print(f"  {name:12s} total {sum(x):9.2f}  mean {statistics.mean(x):8.4f}  sd {statistics.pstdev(x):7.3f}")
    careful = [c for c in CAREFUL if c in p]
    best = max(careful, key=lambda c: sum(p[c]))
    base = max(sum(p[best]), 0.0)
    ratio = sum(p["informed"]) / base if base > 0 else float("inf")
    print(f"  informed / best careful ({best}): {ratio:.2f} (need >= 2)")
    rng = random.Random(7)
    rows = [checks(p, [rng.randrange(n) for _ in range(72)]) for _ in range(2000)]
    keys = ["nop_lt_ref", "naive_lt_ref", "careful_lt_ref", "ref_lt_told"]
    holds = sum(all(r[k] for k in keys) for r in rows) / len(rows)
    print("  " + ", ".join(f"{k} {sum(r[k] for r in rows) / len(rows):.3f}" for k in rows[0]))
    print(f"  order holds on {holds:.3f} of 2000 bootstrapped 72-session sets (need >= 0.85)")
    w = {k: sum(v if v > 0 else K * v for v in x) for k, x in p.items()}
    print("  v2 weighted, share of informed's: " + ", ".join(f"{k} {w[k] / w['informed']:.2f}" for k in sorted(w)))
    r = p["reference"]
    print(f"  reference per-session Sharpe {statistics.mean(r) / statistics.pstdev(r):.2f}")


def main():
    sets = [load(d) for d in sys.argv[1:]]
    for d, p in zip(sys.argv[1:], sets):
        report(os.path.basename(d.rstrip("/")), p)
    if len(sets) > 1:
        names = set.intersection(*(set(p) for p in sets))
        pooled = {n: sum((p[n] for p in sets), []) for n in names}
        report("pooled", pooled)


if __name__ == "__main__":
    main()

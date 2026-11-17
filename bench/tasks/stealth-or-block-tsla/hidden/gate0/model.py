"""Gate 0's fast model: the ladder's policies replayed on recorded sessions.

Takes sessions graded with `nop` on scenarios whose actors keep their tape
(`"record": true`), with `--details`: the engine's own book step by step, and
every actor's takes with their sizes. Each policy decides every step from
what was public by then (the takes so far, the book's mids), as the bots in
`reference/src/lib.rs` do, and trades at the step's touch plus the taker
fee; its own impact is left out. The session's class regime (read from the
scenario's informed size) is used only for the informed policy and the
diagnostics.

Usage:
  python3 model.py measure <report.json> <details-dir> <scen-dir>
  python3 model.py tune <report.json> <details-dir> <scen-dir>
"""

import gzip
import itertools
import json
import os
import statistics
import sys

FEE = 3.5e-4
CAP = 10.0
SIZE_CUT = 0.04


def load(report_path, details_dir, scen_dir):
    report = json.load(open(report_path))
    sessions = []
    for s in report["scenarios"]:
        name = s.get("scenario") or s.get("name")
        scen = json.load(open(os.path.join(scen_dir, name + ".json")))
        actors = scen["actors"]
        informed_size = actors["informed"][0]["size_mult"]
        regime = "small" if informed_size < 1 else "large"
        path = None
        for f in os.listdir(details_dir):
            if f.startswith(name + ".") or f.startswith(name + "-"):
                path = os.path.join(details_dir, f)
        if path is None:
            continue
        with gzip.open(path) as f:
            steps = json.load(f)["timeline"]["steps"]
        book = []
        for st in steps:
            bid, ask = st.get("best_bid_usd"), st.get("best_ask_usd")
            if bid and ask:
                book.append((st["at"], bid, ask))
            else:
                book.append((st["at"], st["mark_usd"], st["mark_usd"]))
        lib = s["actors"]
        takes = sorted(
            (e["at"], 1 if e["side"] == "bid" else -1, e["size"])
            for e in lib["tape"]
            if e["action"] == "take" and e["ok"]
        )
        tallies = {}
        for a in lib.get("actors", []):
            tallies.setdefault(a.get("kind"), []).append(a["actions"]["took"])
        jumps = [
            (e["slot"], e["pct"])
            for e in scen["market_model"].get("events", [])
            if e["kind"] == "jump"
        ]
        failed = sum(1 for e in lib["tape"] if not e["ok"])
        sessions.append(
            dict(name=name, regime=regime, book=book, takes=takes, tallies=tallies,
                 actors_raw=lib.get("actors", []), jumps=jumps, failed=failed,
                 n_actions=len(lib["tape"]))
        )
    return sessions


def cls(size):
    return "large" if size >= SIZE_CUT else "small"


def measure(sessions):
    by = {"small": [], "large": []}
    for s in sessions:
        small = sum(1 for t in s["takes"] if cls(t[2]) == "small")
        large = len(s["takes"]) - small
        by[s["regime"]].append((small, large, len(s["jumps"]), s["failed"], s["n_actions"]))
    for regime, rows in by.items():
        if not rows:
            continue
        print(f"regime informed={regime}: {len(rows)} sessions")
        for i, what in enumerate(["small-bucket takes", "large-bucket takes", "jumps", "failed actions", "actions"]):
            xs = [r[i] for r in rows]
            sd = statistics.pstdev(xs) if len(xs) > 1 else 0
            print(f"  {what:20} mean {statistics.mean(xs):8.1f} sd {sd:7.1f}")
        for kind in ("informed", "decoy", "noise", "chaser"):
            rows_k = [s["tallies"].get(kind, [0]) for s in sessions if s["regime"] == regime]
            if kind == "decoy":
                print(f"  takes by own-class rebalancer {statistics.mean(r[0] for r in rows_k):7.1f}, "
                      f"other-class {statistics.mean(r[1] for r in rows_k):7.1f}")
            else:
                print(f"  takes by {kind:8} {statistics.mean(sum(r) for r in rows_k):7.1f}"
                      f" (min {min(sum(r) for r in rows_k)}, max {max(sum(r) for r in rows_k)})")
    print("mean signed markout (bp, 150 slots ahead) by bucket:")
    for regime in ("small", "large"):
        for c in ("small", "large"):
            mos = []
            for s in sessions:
                if s["regime"] != regime:
                    continue
                mids = [(a, (b + k) / 2) for a, b, k in s["book"]]
                ats = [m[0] for m in mids]
                import bisect
                def mid_at(at):
                    j = bisect.bisect_right(ats, at) - 1
                    return mids[max(j, 0)][1]
                for at, side, size in s["takes"]:
                    if cls(size) == c and at + 150 <= mids[-1][0]:
                        mos.append(side * (mid_at(at + 150) / mid_at(at) - 1) * 1e4)
            if mos:
                print(f"  informed={regime:5} bucket={c:5}: n {len(mos):6} mean {statistics.mean(mos):6.2f} sd {statistics.pstdev(mos):6.2f}")
    inf = [sum(s["tallies"].get("informed", [0])) for s in sessions]
    print("informed takes a session: mean", statistics.mean(inf), "median", statistics.median(inf))
    print("no-jump share", sum(1 for s in sessions if not s["jumps"]) / len(sessions))


def run(s, policy, p):
    """PnL in USD of `policy` on session `s` with params `p`."""
    book, takes = s["book"], s["takes"]
    i = 0
    recent = []
    blocks = {}
    mids = []
    tests = {"small": [0.0, 0.0, 0], "large": [0.0, 0.0, 0]}
    scored = 0
    chosen = None
    if policy == "informed":
        chosen = s["regime"]
    elif policy in ("small", "large"):
        chosen = policy
    pos, cash, direction, hold_until = 0.0, 0.0, 0.0, -1
    B = p.get("block", 75)
    end = book[-1][0]

    def mid_at(at):
        lo, hi = 0, len(mids)
        while lo < hi:
            m = (lo + hi) // 2
            if mids[m][0] <= at:
                lo = m + 1
            else:
                hi = m
        return mids[max(lo - 1, 0)][1]

    def tstat(t):
        sm, sq, n = t
        if n < 2:
            return 0.0
        mean = sm / n
        var = max(sq / n - mean * mean, 100.0)
        return mean / (var / n) ** 0.5

    for at, bid, ask in book:
        mid = (bid + ask) / 2
        mids.append((at, mid))
        # takes confirmed by the previous slot are visible this step
        while i < len(takes) and takes[i][0] < at:
            t_at, side, size = takes[i]
            c = cls(size)
            recent.append((t_at, side, c))
            b = t_at // B
            blocks.setdefault(b, {"small": 0.0, "large": 0.0})[c] += side
            i += 1
        look = p["look"]
        recent = [t for t in recent if t[0] + look >= at]
        if policy == "reference":
            while (scored + 2) * B <= at:
                moved = (mid_at((scored + 2) * B) / mid_at((scored + 1) * B) - 1) * 1e4
                for c in ("small", "large"):
                    net = blocks.get(scored, {}).get(c, 0.0)
                    if abs(net) >= p.get("min_block_net", 3):
                        sc = (1 if net > 0 else -1) * moved
                        tests[c][0] += sc
                        tests[c][1] += sc * sc
                        tests[c][2] += 1
                scored += 1
            if (chosen is None or p.get("switch")) and all(t[2] >= p.get("min_blocks", 4) for t in tests.values()):
                ts, tl = tstat(tests["small"]), tstat(tests["large"])
                if ts > p.get("t_cut", 1.5) and ts - tl > p.get("t_margin", 0.5):
                    chosen = "small"
                elif tl > p.get("t_cut", 1.5) and tl - ts > p.get("t_margin", 0.5):
                    chosen = "large"
        if at >= hold_until:
            sig = None
            if policy == "momentum":
                then = mid_at(at - look)
                moved = (mid / then - 1) * 1e4
                if abs(moved) >= p["momentum"]:
                    sig = 1.0 if moved > 0 else -1.0
            else:
                if policy in ("all", "small-all"):
                    net = sum(t[1] for t in recent)
                elif chosen is not None:
                    net = sum(t[1] for t in recent if t[2] == chosen)
                else:
                    net = 0.0
                if abs(net) >= max(p["thr"], 1):
                    sig = 1.0 if net > 0 else -1.0
            if sig is not None:
                direction = sig
                hold_until = at + p["hold"]
            else:
                direction = 0.0
        if end - at <= 4:
            direction = 0.0
        target = direction * CAP * p.get("share", 1.0)
        d = target - pos
        if abs(d) >= 0.01:
            px = ask * (1 + FEE) if d > 0 else bid * (1 - FEE)
            cash -= d * px
            pos = target
    last_mid = (book[-1][1] + book[-1][2]) / 2
    return cash + pos * last_mid


GRIDS = {
    "informed": dict(look=[10, 20, 40, 80], thr=[1, 2, 3, 4, 6], hold=[40, 80, 150, 250]),
    "small": dict(look=[20, 40, 80, 160], thr=[2, 3, 4, 6, 8, 12], hold=[40, 80, 150, 250]),
    "large": dict(look=[20, 40, 80, 160], thr=[2, 3, 4, 6, 8, 12], hold=[40, 80, 150, 250]),
    "all": dict(look=[20, 40, 80, 160], thr=[3, 4, 6, 8, 12, 16, 24], hold=[40, 80, 150, 250]),
    "small-all": dict(look=[40], thr=[2], hold=[80], share=[0.2]),
    "momentum": dict(look=[20, 40, 80, 160], momentum=[10, 15, 25, 40, 60], hold=[40, 80, 150, 250], thr=[1]),
}


def tune(sessions, policy, grid=None):
    grid = grid or GRIDS[policy]
    keys = list(grid)
    best = None
    for vals in itertools.product(*(grid[k] for k in keys)):
        p = dict(zip(keys, vals))
        pnl = [run(s, policy, p) for s in sessions]
        tot = sum(pnl)
        if best is None or tot > best[0]:
            best = (tot, p, pnl)
    return best


def summary(name, pnl):
    sd = statistics.pstdev(pnl) if len(pnl) > 1 else 0
    mean = statistics.mean(pnl)
    print(f"  {name:12} total {sum(pnl):9.1f} mean {mean:7.2f} sd {sd:7.2f} sharpe {mean / sd if sd else 0:5.2f}")


def cv(train, test):
    """Each rung tuned on `train`, scored on `test`."""
    out = {}
    for policy in ["informed", "small", "large", "all", "small-all", "momentum"]:
        _, p, _ = tune(train, policy)
        pnl = [run(s, policy, p) for s in test]
        out[policy] = (p, pnl)
        print(f"{policy}: tuned {p}")
        summary(policy, pnl)
    ip = dict(out["informed"][0])
    refs = {}
    for t_cut, t_margin, min_blocks in [(1.0, 0.5, 4), (1.5, 0.5, 4), (2.0, 1.0, 5), (2.0, 1.0, 8)]:
        p = dict(ip, t_cut=t_cut, t_margin=t_margin, min_blocks=min_blocks)
        pnl = [run(s, "reference", p) for s in test]
        refs[(t_cut, t_margin, min_blocks)] = pnl
        summary(f"ref {t_cut}/{t_margin}/{min_blocks}", pnl)
    inf = sum(out["informed"][1])
    best = max(["small", "large", "all", "small-all", "momentum"], key=lambda k: sum(out[k][1]))
    b = sum(out[best][1])
    print(f"informed / best non-reading ({best}) = {inf / b if b > 0 else float('inf'):.2f}")
    for k, pnl in refs.items():
        print(f"  ref {k} / informed = {sum(pnl) / inf:.2f}")


def tune_ref(sessions, informed_p):
    grid = dict(block=[40, 75, 150], min_block_net=[2, 3, 5], t_cut=[0.5, 1.0, 1.5, 2.0],
                t_margin=[0.0, 0.5, 1.0], min_blocks=[3, 4, 6], switch=[False, True])
    keys = list(grid)
    rows = []
    for vals in itertools.product(*(grid[k] for k in keys)):
        p = dict(informed_p, **dict(zip(keys, vals)))
        pnl = [run(s, "reference", p) for s in sessions]
        sd = statistics.pstdev(pnl)
        rows.append((sum(pnl), statistics.mean(pnl) / sd if sd else 0, dict(zip(keys, vals))))
    rows.sort(key=lambda r: -r[0])
    for r in rows[:12]:
        print(f"  ref total {r[0]:8.1f} sharpe {r[1]:5.2f} {r[2]}")
    return rows


def main():
    if sys.argv[1] == "tuneref":
        sessions = load(*sys.argv[2:5])
        tune_ref(sessions, json.loads(sys.argv[5]))
        return
    if sys.argv[1] == "cv":
        a = load(*sys.argv[2:5])
        b = load(*sys.argv[5:8])
        print(f"train {len(a)}, test {len(b)} sessions")
        cv(a, b)
        return
    cmd, report, details, scen = sys.argv[1:5]
    sessions = load(report, details, scen)
    print(f"{len(sessions)} sessions")
    if cmd == "measure":
        measure(sessions)
        return
    results = {}
    for policy in ["informed", "small", "large", "all", "small-all", "momentum"]:
        tot, p, pnl = tune(sessions, policy)
        results[policy] = (p, pnl)
        print(f"{policy}: best {p}")
        summary(policy, pnl)
    ip = dict(results["informed"][0])
    for t_cut, t_margin, min_blocks in [(1.5, 0.5, 4), (2.0, 1.0, 5), (2.0, 1.0, 8), (1.0, 0.5, 4)]:
        p = dict(ip, t_cut=t_cut, t_margin=t_margin, min_blocks=min_blocks)
        pnl = [run(s, "reference", p) for s in sessions]
        summary(f"ref {t_cut}/{t_margin}/{min_blocks}", pnl)
    inf = sum(results["informed"][1])
    best_non = max(sum(results[k][1]) for k in ["small", "large", "all", "small-all", "momentum"])
    print(f"informed / best non-reading = {inf / best_non if best_non > 0 else float('inf'):.2f}")


if __name__ == "__main__":
    main()

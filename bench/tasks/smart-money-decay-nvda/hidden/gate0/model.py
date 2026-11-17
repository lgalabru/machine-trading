"""Gate 0's fast model: the ladder's policies replayed on recorded sessions.

Takes sessions graded with `nop` on scenarios whose actors record their tape
(`"record": true`), with `--details`: the engine's own book, step by step,
and every actor's takes with its wallet. Each policy decides every step from
what was public by then (the takes so far, the book's mids) exactly as the
bots in `reference/src/lib.rs` do, and trades at the step's touch plus the
taker fee; its own impact is left out (the snapshot's makers hold about 12
NVDA a tick and re-centre on the path). The truth (which wallet is which
actor) is used only for the informed policy's answer key.

Usage: python3 model.py <report.json> <details-dir> [--json overrides]
"""

import argparse
import gzip
import json
import os
import random
import statistics

FEE = 3.5e-4
CAP = 30.0


def load(report_path, details_dir):
    report = json.load(open(report_path))
    sessions = []
    for s in report["scenarios"]:
        lib = s["actors"]
        truth = {w: a["kind"] for a in lib["actors"] for w in a["wallets"]}
        name = s.get("scenario") or s.get("name")
        path = None
        for f in os.listdir(details_dir):
            if f.startswith(name + ".") or f.startswith(name + "-") or f == f"{name}.json.gz":
                path = os.path.join(details_dir, f)
        if path is None:
            continue
        with gzip.open(path) as f:
            steps = json.load(f)["timeline"]["steps"]
        book = []
        for st in steps:
            bid, ask = st.get("best_bid_usd"), st.get("best_ask_usd")
            if bid and ask:
                book.append((st["at"], (bid + ask) / 2, (ask - bid) / 2))
            else:
                book.append((st["at"], st["mark_usd"], 0.0))
        takes = [
            (e["at"], e["wallet"], (1 if e["side"] == "bid" else -1) * e["size"])
            for e in lib["tape"]
            if e["action"] == "take" and e["ok"]
        ]
        takes.sort(key=lambda t: t[0])
        sessions.append({"name": name, "book": book, "takes": takes, "truth": truth})
    return sessions


BASE = dict(look=40, hold=40, threshold=0.5, mark=40, chase=8.0, min_weight=2.0,
            min_score=5.0, half_life_share=0.25, momentum=8.0)
MODES = {
    "informed": dict(look=20, hold=80, threshold=0.0, agree=2),
    "reference": dict(look=20, hold=80, threshold=0.0, min_score=20.0, agree=2),
    "most-active": dict(look=10, hold=20, threshold=0.0),
    "follow-flow": dict(look=40, hold=40, threshold=4.0),
    "fade-flow": dict(look=10, hold=10, threshold=6.0),
    "momentum": dict(look=4, hold=80, momentum=8.0),
}


def run(session, mode, overrides=None):
    p = {**BASE, **MODES[mode], **(overrides or {})}
    book, takes, truth = session["book"], session["takes"], session["truth"]
    end = book[-1][0]
    half_life = max(1.0, end * p["half_life_share"])
    informed = {w for w, k in truth.items() if k == "informed"}
    mids = []  # (at, mid)
    recent = []  # (at, wallet, signed)
    pending = []  # (at, wallet, signed, mid)
    rec = {}  # wallet -> [weight, sum, at, takes, chased]
    i = 0
    pos, cash = 0.0, 0.0
    direction, hold_until, entries = 0.0, -1, 0

    def mid_at(at):
        best = mids[0][1]
        for a, m in mids:
            if a <= at:
                best = m
            else:
                break
        return best

    def credible(w):
        r = rec.get(w)
        return r is not None and r[0] >= p["min_weight"] and r[1] / r[0] >= p["min_score"] and 2 * r[4] < r[3]

    for at, mid, half in book:
        mids.append((at, mid))
        if mode != "momentum":
            before = mid_at(at - p["look"])
            moved = (mid / before - 1) * 1e4
            while i < len(takes) and takes[i][0] <= at:
                t_at, w, signed = takes[i]
                i += 1
                chased = moved * (1 if signed > 0 else -1) > p["chase"]
                r = rec.setdefault(w, [0.0, 0.0, t_at, 0, 0])
                r[3] += 1
                r[4] += chased
                recent.append((t_at, w, signed))
                pending.append((t_at, w, signed, mid))
            recent = [t for t in recent if t[0] + max(p["look"], p["mark"]) >= at]
            while pending and pending[0][0] + p["mark"] <= at:
                t_at, w, signed, m0 = pending.pop(0)
                r = rec[w]
                k = 0.5 ** ((at - r[2]) / half_life)
                r[0] *= k
                r[1] *= k
                r[2] = at
                r[0] += 1
                r[1] += (1 if signed > 0 else -1) * (mid / m0 - 1) * 1e4
        window = [t for t in recent if t[0] + p["look"] > at]
        sig = None
        if mode == "momentum":
            if len(mids) > 1:
                then = mid_at(at - p["look"])
                mv = (mid / then - 1) * 1e4
                if abs(mv) >= p["momentum"]:
                    sig = 1.0 if mv > 0 else -1.0
        else:
            if mode == "follow-flow":
                net = sum(t[2] for t in window)
            elif mode == "fade-flow":
                net = -sum(t[2] for t in window)
            elif mode in ("informed", "reference") and p.get("agree"):
                # Agree-of-k: each trusted wallet's net flow over the window
                # is a vote; enter when at least min(trusted, k) agree and
                # more agree than disagree, then hold with no flips.
                trusted = informed if mode == "informed" else {w for w in rec if credible(w)}
                nets = {}
                for t in window:
                    if t[1] in trusted:
                        nets[t[1]] = nets.get(t[1], 0.0) + t[2]
                up = sum(1 for v in nets.values() if v > 0)
                dn = sum(1 for v in nets.values() if v < 0)
                need = min(len(trusted), p["agree"])
                net = 0.0
                if direction == 0.0 and need > 0:
                    if up >= need and up > dn:
                        net = 1.0
                    elif dn >= need and dn > up:
                        net = -1.0
            elif mode == "informed":
                net = sum(t[2] for t in window if t[1] in informed)
            elif mode == "reference":
                net = sum(t[2] for t in window if credible(t[1]))
            else:
                busiest = max(rec.items(), key=lambda kv: kv[1][3])[0] if rec else None
                net = sum(t[2] for t in window if t[1] == busiest)
            if abs(net) > max(p["threshold"], 1e-9):
                sig = 1.0 if net > 0 else -1.0
        if sig is not None and p.get("agree") and direction != 0.0:
            sig = None
        if sig is not None:
            if sig != direction:
                entries += 1
            direction = sig
            hold_until = at + p["hold"]
        if at > hold_until or at >= end - 4:
            direction = 0.0
        target = direction * CAP
        delta = target - pos
        if abs(delta) >= 0.5:
            price = mid + (half if delta > 0 else -half)
            cash -= delta * price + abs(delta) * price * FEE
            pos = target
    pnl = cash + pos * book[-1][1]
    return pnl, entries


def weighted(xs, k=1.5):
    return sum(x if x > 0 else k * x for x in xs)


def ladder(sessions, overrides=None, draws=2000, count=72, seed=1):
    overrides = overrides or {}
    rungs = {"nop": [0.0] * len(sessions)}
    entries = {}
    for mode in MODES:
        res = [run(s, mode, overrides.get(mode)) for s in sessions]
        rungs[mode] = [r[0] for r in res]
        entries[mode] = statistics.mean(r[1] for r in res)
    for name in sorted(rungs, key=lambda r: sum(rungs[r])):
        xs = rungs[name]
        print(f"{name:14}{sum(xs):9.0f}{statistics.mean(xs):8.2f}{statistics.pstdev(xs):8.2f}"
              f"  entries {entries.get(name, 0):5.1f}")
    nr = ["nop", "follow-flow", "fade-flow", "momentum"]
    best = max(nr, key=lambda r: sum(rungs[r]))
    told = sum(rungs["informed"])
    base = max(sum(rungs[best]), 0)
    print(f"informed / best non-reading ({best}): {told / base if base > 0 else float('inf'):.2f}")
    print(f"reference v2-weighted / informed: {weighted(rungs['reference']) / weighted(rungs['informed']):.2f}")
    ref = rungs["reference"]
    print(f"reference Sharpe {statistics.mean(ref) / (statistics.pstdev(ref) or 1):.2f}")
    rng = random.Random(seed)
    n = len(sessions)
    hold = 0
    for _ in range(draws):
        idx = [rng.randrange(n) for _ in range(count)]
        t = {r: sum(rungs[r][i] for i in idx) for r in rungs}
        hold += max(t[r] for r in nr + ["most-active"]) < t["reference"] < t["informed"]
    print(f"order holds on {hold / draws:.3f} of {draws} bootstrapped {count}-session sets")
    return rungs


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("report")
    parser.add_argument("details")
    parser.add_argument("--json", default="{}", help="per-mode overrides")
    args = parser.parse_args()
    sessions = load(args.report, args.details)
    print(f"{len(sessions)} sessions")
    ladder(sessions, json.loads(args.json))


if __name__ == "__main__":
    main()

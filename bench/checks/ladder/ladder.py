"""The skill ladder: fixed bots, graded on fresh hidden sets, in a fixed order.

Each task's rungs and rules are in `<task>.json`. For each set, the ladder
draws a fresh secret (never printed), grades every rung on the hidden set it
seeds with `mtx grade --draw`, and scores each rung's skill: its total PnL
over the reference's. Then it checks:

- the order rule, on every set: `a < b` compares total PnL (as graded:
  0 for a disqualified rung), `max(a, b)` and
  `min(a, b)` combine rungs, and `hindsight` is the set's bound;
- how often the rule would hold on a set like it: a bootstrap over the set's
  sessions, which must be at least `min_rule_odds`;
- the exploit bots' limits (`exploits`), over all sets: they must lose some
  sessions, make less than the rung named by `below` (the regime oracle:
  knowing the generator mustn't pay more than knowing the regime), and never
  reach the bound;
- that nothing a rung did was refused or disqualified, except rungs marked
  `may_breach` (the blind bets at the full cap break the drawdown limit).

Usage: python3 ladder.py <task> --mtx target/release/mtx --snapshot DIR
       [--sets 4] [--count N] [--jobs 4] [--parallel 1] [--out DIR]
       [--min-rule-odds P]

It also writes `<out>/<task>/summary.json`, which `mtx gates` reads: each
set's totals per rung (as graded), and every rule, exploit and clean-run
verdict.

`--parallel N` grades N rungs at once. Each grade's bots run as its own
range of the grader's bot users (`--first-bot-user`): a session's end kills
every process of its user, so rungs sharing users would kill each other's
bots. There are four, so `parallel × jobs` can't exceed 4.
"""

import argparse
import json
import os
import queue
import random
import re
import secrets
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "../../.."))
BOOTSTRAP_DRAWS = 2000
# The grader's bot users (session::BOT_USERS).
BOT_USERS = 4


def grade(mtx, snapshot, family, count, jobs, rung, secret, out, users):
    report = os.path.join(out, f"{rung['name']}.json")
    if os.path.exists(report):
        with open(report) as f:
            return json.load(f)
    # A range of bot users no other grade is using; back when this one ends.
    first_bot_user = users.get()
    try:
        return run_grade(mtx, snapshot, family, count, jobs, rung, secret, out, report,
                         first_bot_user)
    finally:
        users.put(first_bot_user)


def run_grade(mtx, snapshot, family, count, jobs, rung, secret, out, report, first_bot_user):
    command = [
        mtx, "grade", "--draw", family, "--count", str(count), "--jobs", str(jobs),
        "--first-bot-user", str(first_bot_user),
        "--snapshot", snapshot, "--bot", os.path.join(ROOT, rung["dir"]),
        "--logs", os.path.join(out, "logs", rung["name"]), "--report", report,
    ]
    if rung.get("tell_regime"):
        command.append("--tell-regime")
    env = dict(os.environ, MTX_EVAL_SEED=secret)
    # A failed grade is an infrastructure failure (a session that couldn't
    # set up), not a score: try once more before giving up on the rung.
    for attempt in (1, 2):
        with open(os.path.join(out, f"{rung['name']}.log"), "w") as log:
            done = subprocess.run(command, env=env, stdout=subprocess.DEVNULL, stderr=log)
        if done.returncode == 0:
            break
    else:
        raise SystemExit(f"{rung['name']}: mtx grade failed twice; see {out}/{rung['name']}.log")
    with open(report) as f:
        return json.load(f)


def pnls(report):
    return [s["pnl_usd"] for s in report["scenarios"]]


def disqualified(report):
    return any(s.get("disqualified") for s in report["scenarios"])


def totals(reports, sessions=None):
    """Each rung's total PnL on `sessions` (indices), as graded: 0 for a
    rung the grader disqualified, and the hindsight bound's."""
    n = len(reports["reference"]["scenarios"])
    sessions = range(n) if sessions is None else sessions
    out = {
        name: 0.0 if disqualified(report) else sum(pnls(report)[i] for i in sessions)
        for name, report in reports.items()
    }
    bound = [s["bound_pnl_usd"] for s in reports["reference"]["scenarios"]]
    out["hindsight"] = sum(bound[i] for i in sessions)
    return out


def skills(reports):
    """Totals over the reference's: what `skill` reports. For reading only;
    rules compare totals, which order the same way when the reference makes
    money and stay sound when a resample's reference doesn't."""
    t = totals(reports)
    ref = t["reference"]
    return {name: v / ref if ref > 0 else float("nan") for name, v in t.items()}


def value(term, skill):
    term = term.strip()
    m = re.fullmatch(r"(max|min)\((.*)\)", term)
    if m:
        parts = [value(p, skill) for p in m.group(2).split(",")]
        return max(parts) if m.group(1) == "max" else min(parts)
    return skill[term]


def holds(rule, skill):
    terms = [t for t in rule.split("<")]
    values = [value(t, skill) for t in terms]
    return all(a < b for a, b in zip(values, values[1:]))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("task")
    parser.add_argument("--mtx", required=True)
    parser.add_argument("--snapshot", required=True)
    parser.add_argument("--sets", type=int, default=4)
    parser.add_argument("--count", type=int)
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--parallel", type=int, default=1, help="rungs graded at once")
    parser.add_argument("--min-rule-odds", type=float, help="override the task's bootstrap floor")
    parser.add_argument("--out", default=os.path.join(ROOT, "target", "ladder"))
    args = parser.parse_args()
    if args.parallel < 1 or args.parallel * args.jobs > BOT_USERS:
        parser.error(f"--parallel × --jobs must be between 1 and {BOT_USERS}, one bot user each")
    # One range of bot users per rung graded at once.
    users = queue.Queue()
    for k in range(args.parallel):
        users.put(k * args.jobs)
    with open(os.path.join(HERE, f"{args.task}.json")) as f:
        config = json.load(f)
    count = args.count or config["count"]
    out = os.path.join(args.out, args.task)
    os.makedirs(out, exist_ok=True)

    sets = []
    for k in range(args.sets):
        set_dir = os.path.join(out, f"set-{k + 1}")
        os.makedirs(set_dir, exist_ok=True)
        # The secret lives only in this process and the graders it starts.
        secret_file = os.path.join(set_dir, ".secret")
        if os.path.exists(secret_file):
            with open(secret_file) as f:
                secret = f.read().strip()
        else:
            secret = secrets.token_hex(32)
            with open(os.open(secret_file, os.O_WRONLY | os.O_CREAT, 0o600), "w") as f:
                f.write(secret)
        with ThreadPoolExecutor(max_workers=args.parallel) as pool:
            futures = {
                rung["name"]: pool.submit(
                    grade, args.mtx, args.snapshot, config["family"], count, args.jobs,
                    rung, secret, set_dir, users,
                )
                for rung in config["rungs"]
            }
            reports = {name: future.result() for name, future in futures.items()}
        sets.append(reports)
        print(f"set {k + 1} (reference ${totals(reports)['reference']:.0f}): " + ", ".join(
            f"{name} {s:.2f}" for name, s in skills(reports).items()
        ), flush=True)

    failed = []
    min_rule_odds = args.min_rule_odds if args.min_rule_odds is not None else config["min_rule_odds"]
    summary = {
        "task": args.task,
        "sessions": count,
        "min_rule_odds": min_rule_odds,
        "totals": [totals(reports) for reports in sets],
        "rules": [],
        "exploits": [],
        "disqualified": [],
    }
    print("\nskill by set (reference = 1, hindsight = the bound; a disqualified rung is 0):")
    names = [r["name"] for r in config["rungs"]] + ["hindsight"]
    table = [skills(reports) for reports in sets]
    for name in names:
        row = [t[name] for t in table]
        print(f"  {name:22} " + "  ".join(f"{v:7.2f}" for v in row)
              + f"   mean {sum(row) / len(row):6.2f}")

    print("\norder rules:")
    for rule in config["rules"]:
        per_set = []
        for reports, skill in zip(sets, table):
            n = len(reports["reference"]["scenarios"])
            rng = random.Random(0)
            hits = sum(
                holds(rule, totals(reports, [rng.randrange(n) for _ in range(n)]))
                for _ in range(BOOTSTRAP_DRAWS)
            )
            per_set.append((holds(rule, totals(reports)), hits / BOOTSTRAP_DRAWS))
        every = all(h for h, _ in per_set)
        odds = sum(p for _, p in per_set) / len(per_set)
        ok = every and odds >= min_rule_odds
        summary["rules"].append({"rule": rule, "ok": ok, "sets_holding": sum(h for h, _ in per_set),
                                 "sets": len(per_set), "odds": odds})
        print(f"  {'ok  ' if ok else 'FAIL'} {rule}: holds on {sum(h for h, _ in per_set)}"
              f"/{len(per_set)} sets, bootstrap odds per set {odds:.3f} "
              f"({', '.join(f'{p:.3f}' for _, p in per_set)})")
        if not ok:
            failed.append(rule)

    print("\nexploit bots, over every set:")
    limits = config["exploits"]
    for name in limits["bots"]:
        all_pnls = [p for reports in sets for p in pnls(reports[name])]
        reference = sum(sum(pnls(r["reference"])) for r in sets)
        ratio = sum(all_pnls) / reference if reference > 0 else float("nan")
        profitable = sum(p > 0 for p in all_pnls) / len(all_pnls)
        reward = max(r[name]["reward"] for r in sets)
        reasons = []
        if profitable > limits["max_profitable"]:
            reasons.append(f"profitable in {profitable:.0%} of sessions")
        # Knowing the generator shouldn't pay more than knowing the regime.
        below = limits.get("below")
        if below:
            ceiling = sum(sum(pnls(r[below])) for r in sets)
            if sum(all_pnls) >= ceiling:
                reasons.append(f"${sum(all_pnls):.0f}, at or above {below}'s ${ceiling:.0f}")
        if reward > 0.999:
            reasons.append("reached the bound")
        summary["exploits"].append({"bot": name, "ok": not reasons, "ratio": ratio,
                                    "profitable": profitable, "reasons": reasons})
        print(f"  {'FAIL' if reasons else 'ok  '} {name}: {ratio:.2f}x the reference, "
              f"profitable {profitable:.0%}, worst ${min(all_pnls):.0f}"
              + (f" ({'; '.join(reasons)})" if reasons else ""))
        if reasons:
            failed.append(name)

    print("\nclean runs:")
    for reports in sets:
        may_breach = {r["name"] for r in config["rungs"] if r.get("may_breach")}
        for name, report in reports.items():
            if name in may_breach:
                continue
            bad = [s["scenario"] for s in report["scenarios"]
                   if s.get("disqualified") or s["refused_calls"]]
            if bad:
                print(f"  FAIL {name}: disqualified in {', '.join(bad)}")
                summary["disqualified"].append({"rung": name, "scenarios": bad})
                failed.append(f"{name} disqualified")
    if not any("disqualified" in f for f in failed):
        print("  ok   no rung was refused or disqualified")

    summary["failed"] = failed
    with open(os.path.join(out, "summary.json"), "w") as f:
        json.dump(summary, f, indent=2)
    print("\nladder " + ("FAILED: " + "; ".join(failed) if failed else "holds"))
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()

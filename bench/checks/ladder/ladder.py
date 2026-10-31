"""The skill ladder: fixed bots, graded on fresh hidden sets, in a fixed order.

Each task's rungs and rules are in `<task>.json`. For each set, the ladder
draws a fresh secret (never printed), grades every rung on the hidden set it
seeds with `mth grade --draw`, and scores each rung's skill: its total PnL
over the reference's. Then it checks:

- the order rule, on every set: `a < b` compares skills, `max(a, b)` and
  `min(a, b)` combine rungs, and `hindsight` is the set's bound;
- how often the rule would hold on a set like it: a bootstrap over the set's
  sessions, which must be at least `min_rule_odds`;
- the exploit bots' limits (`exploits`), over all sets: they must lose some
  sessions, stay within a multiple of the reference, and never reach the
  bound;
- that nothing a rung did was refused or disqualified, except where the rung
  expects it.

Usage: python3 ladder.py <task> --mth target/release/mth --snapshot DIR
       [--sets 4] [--count N] [--jobs 4] [--out DIR]
"""

import argparse
import json
import os
import random
import re
import secrets
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "../../.."))
BOOTSTRAP_DRAWS = 2000


def grade(mth, snapshot, family, count, jobs, rung, secret, out):
    report = os.path.join(out, f"{rung['name']}.json")
    if os.path.exists(report):
        with open(report) as f:
            return json.load(f)
    command = [
        mth, "grade", "--draw", family, "--count", str(count), "--jobs", str(jobs),
        "--snapshot", snapshot, "--bot", os.path.join(ROOT, rung["dir"]),
        "--logs", os.path.join(out, "logs", rung["name"]), "--report", report,
    ]
    if rung.get("tell_regime"):
        command.append("--tell-regime")
    env = dict(os.environ, MTH_EVAL_SEED=secret)
    with open(os.path.join(out, f"{rung['name']}.log"), "w") as log:
        done = subprocess.run(command, env=env, stdout=subprocess.DEVNULL, stderr=log)
    if done.returncode != 0:
        raise SystemExit(f"{rung['name']}: mth grade failed; see {out}/{rung['name']}.log")
    with open(report) as f:
        return json.load(f)


def pnls(report):
    return [s["pnl_usd"] for s in report["scenarios"]]


def skills(reports, sessions=None):
    """Each rung's skill, and the hindsight bound's, on `sessions` (indices)."""
    n = len(reports["reference"]["scenarios"])
    sessions = range(n) if sessions is None else sessions
    reference = sum(pnls(reports["reference"])[i] for i in sessions)
    out = {}
    for name, report in reports.items():
        out[name] = sum(pnls(report)[i] for i in sessions) / reference if reference > 0 else float("nan")
    bound = [s["bound_pnl_usd"] for s in reports["reference"]["scenarios"]]
    out["hindsight"] = sum(bound[i] for i in sessions) / reference if reference > 0 else float("nan")
    return out


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
    parser.add_argument("--mth", required=True)
    parser.add_argument("--snapshot", required=True)
    parser.add_argument("--sets", type=int, default=4)
    parser.add_argument("--count", type=int)
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--parallel", type=int, default=1, help="rungs graded at once")
    parser.add_argument("--out", default=os.path.join(ROOT, "target", "ladder"))
    args = parser.parse_args()
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
                    grade, args.mth, args.snapshot, config["family"], count, args.jobs,
                    rung, secret, set_dir,
                )
                for rung in config["rungs"]
            }
            reports = {name: future.result() for name, future in futures.items()}
        sets.append(reports)
        print(f"set {k + 1}: " + ", ".join(
            f"{name} {s:.2f}" for name, s in skills(reports).items()
        ), flush=True)

    failed = []
    print("\nskill by set (reference = 1, hindsight = the bound):")
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
                holds(rule, skills(reports, [rng.randrange(n) for _ in range(n)]))
                for _ in range(BOOTSTRAP_DRAWS)
            )
            per_set.append((holds(rule, skill), hits / BOOTSTRAP_DRAWS))
        every = all(h for h, _ in per_set)
        odds = sum(p for _, p in per_set) / len(per_set)
        ok = every and odds >= config["min_rule_odds"]
        print(f"  {'ok  ' if ok else 'FAIL'} {rule}: holds on {sum(h for h, _ in per_set)}"
              f"/{len(per_set)} sets, bootstrap odds per set {odds:.2f}")
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
        if ratio > limits.get("max_ratio", float("inf")):
            reasons.append(f"{ratio:.1f}x the reference")
        if reward > 0.999:
            reasons.append("reached the bound")
        print(f"  {'FAIL' if reasons else 'ok  '} {name}: {ratio:.2f}x the reference, "
              f"profitable {profitable:.0%}, worst ${min(all_pnls):.0f}"
              + (f" ({'; '.join(reasons)})" if reasons else ""))
        if reasons:
            failed.append(name)

    print("\nclean runs:")
    for reports in sets:
        for name, report in reports.items():
            bad = [s["scenario"] for s in report["scenarios"]
                   if s.get("disqualified") or s["refused_calls"]]
            if bad:
                print(f"  FAIL {name}: disqualified in {', '.join(bad)}")
                failed.append(f"{name} disqualified")
    if not any("disqualified" in f for f in failed):
        print("  ok   no rung was refused or disqualified")

    print("\nladder " + ("FAILED: " + "; ".join(failed) if failed else "holds"))
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()

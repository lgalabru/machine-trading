"""The skill ladder: fixed bots, graded on fresh hidden sets, in a fixed order.

Each task's rungs and rules are in its folder, `bench/tasks/<task>/checks/ladder.json`. For each set, the ladder
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

A rung may set `env`, a map of environment variables its bot starts with
(the grader clears a bot's environment, so the ladder starts it through a
wrapper that sets them): the same binary then serves several rungs, as the
mark pusher's modes do. A rung may set `push` (1: pushes the mark up, -1:
down, 0: either way): the ladder then reads how far it moved the mark from the
index beyond `nop`'s, and an exploit bot that moved nothing, or less than the
premium episodes when the rung also sets `calibrate`, is reported `no_power`,
not passed.

A task may add a `probes` section to its ladder file: the scoring v2 probe
gate (bench/checks/probes). The ladder then grades every rung with
`--scoring v2` and, for each rule the section names, reports whether the v2
score of the probes lands where the grading decisions say it must (see
`probe_checks`). A rule fails only when a bootstrap over the sessions shows it
broken with `confidence` (default 0.95); a point estimate that breaks it
without that confidence is `inconclusive`, and one that holds is `pass`. A
probe that is meant to breach and never did is `no_power`.

Usage: python3 ladder.py <task> --mtx target/release/mtx --snapshot DIR
       [--sets 4] [--count N] [--jobs 4] [--parallel 1] [--out DIR]
       [--min-rule-odds P] [--public-seed LABEL]

`--public-seed LABEL` draws each set from a secret derived from the label
instead of a fresh one: a fixed, public set that gives the same sessions on
every run (`mtx gates --fast` uses it in CI). It is never a hidden set: those
come from fresh 256-bit secrets.

It also writes `<out>/<task>/summary.json`, which `mtx gates` reads: each
set's totals per rung (as graded), and every rule, exploit and clean-run
verdict.

`--parallel N` grades N rungs at once. Each grade's bots run as its own
range of the grader's bot users (`--first-bot-user`): a session's end kills
every process of its user, so rungs sharing users would kill each other's
bots. There are four, so `parallel × jobs` can't exceed 4.
"""

import argparse
import gzip
import hashlib
import json
import os
import queue
import random
import re
import secrets
import shlex
import statistics
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "../../.."))
BOOTSTRAP_DRAWS = 2000
# The grader's bot users (session::BOT_USERS).
BOT_USERS = 4
# What the grader sets for a bot: a rung's `env` can't override them.
GRADER_VARS = {
    "MTX_RPC_URL", "MTX_KEYPAIR", "MTX_EXCHANGE", "MTX_SYMBOL", "MTX_END_SLOT",
    "MTX_MAX_POSITION", "MTX_MAX_DRAWDOWN", "MTX_MANDATE", "MTX_REGIME",
    "MTX_REOPEN_SLOT", "MTX_TURN_FD", "PATH", "HOME", "RUST_LOG",
}
# An exploit bot that moves the mark less than this, in bp over the index on
# average, moved nothing.
MIN_DISPLACEMENT_BP = 0.5


def rung_bot(rung, set_dir, count=None):
    """The directory the grader starts the rung from. A rung's `env` goes
    through a wrapper `run.sh` that sets it and starts the rung's own: the
    grader clears a bot's environment."""
    env = rung.get("env")
    if not env:
        return os.path.join(ROOT, rung["dir"])
    # A probe knows how many sessions its set has (for "b breaches a set").
    if count is not None and any(n.startswith("MTX_PROBE") for n in env):
        env = dict(env, MTX_PROBE_SESSIONS=count)
    real = os.path.join(ROOT, rung["dir"], "run.sh")
    if not os.path.isfile(real):
        raise SystemExit(f"{rung['name']}: `env` needs a rung with a run.sh ({real})")
    for name in env:
        if not re.fullmatch(r"[A-Z][A-Z0-9_]*", name) or name in GRADER_VARS:
            raise SystemExit(f"{rung['name']}: `env` can't set {name}")
    wrapper = os.path.join(set_dir, "rungs", rung["name"])
    os.makedirs(wrapper, exist_ok=True)
    path = os.path.join(wrapper, "run.sh")
    with open(path, "w") as f:
        f.write("#!/usr/bin/env bash\n")
        for name, value in sorted(env.items()):
            f.write(f"export {name}={shlex.quote(str(value))}\n")
        f.write(f"exec bash {shlex.quote(real)} \"$@\"\n")
    # Bots run as users of their own: the wrapper must be readable by them.
    os.chmod(wrapper, 0o755)
    os.chmod(path, 0o755)
    return wrapper


def grade(mtx, snapshot, family, count, jobs, rung, secret, out, users, v2=None):
    report = os.path.join(out, f"{rung['name']}.json")
    if os.path.exists(report):
        with open(report) as f:
            return json.load(f)
    # A range of bot users no other grade is using; back when this one ends.
    first_bot_user = users.get()
    try:
        return run_grade(mtx, snapshot, family, count, jobs, rung, secret, out, report,
                         first_bot_user, v2)
    finally:
        users.put(first_bot_user)


def run_grade(mtx, snapshot, family, count, jobs, rung, secret, out, report, first_bot_user,
              v2=None):
    command = [
        mtx, "grade", "--draw", family, "--count", str(count), "--jobs", str(jobs),
        "--first-bot-user", str(first_bot_user),
        "--snapshot", snapshot, "--bot", rung_bot(rung, out, count),
        "--logs", os.path.join(out, "logs", rung["name"]), "--report", report,
        # Rungs are ours, like the reference: their turns wait for them, so a
        # rung's skill doesn't depend on how loaded the machine is.
        "--trusted",
    ]
    if rung.get("tell_regime"):
        command.append("--tell-regime")
    if v2:
        # v2 anchors every session to the reference's PnL on it, so the
        # reference plays the same sessions in each grade.
        command += ["--scoring", "v2", "--reference", v2["reference"],
                    "--anchor-jobs", str(jobs)]
        if "eta" in v2:
            command += ["--eta", str(v2["eta"])]
    # The steps of the rungs whose mark displacement is read, and of nop's,
    # the baseline.
    if "push" in rung or rung["name"] == "nop":
        command += ["--details", os.path.join(out, "details", rung["name"])]
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


def gap_bp(step):
    """The mark's gap over Phoenix's index at a step, in bp of the index."""
    return (step["mark_usd"] - step["index_usd"]) / step["index_usd"] * 1e4


def read_details(set_dir, name):
    """A rung's session details by scenario, from `mtx grade --details`."""
    folder = os.path.join(set_dir, "details", name, "bot")
    out = {}
    if os.path.isdir(folder):
        for file in sorted(os.listdir(folder)):
            with gzip.open(os.path.join(folder, file), "rt") as f:
                out[file] = json.load(f)
    return out


def mean_gap(detail, push):
    """The session's mean gap, pushing `push` (1: up, -1: down; 0: either way:
    the gap's size)."""
    gaps = [gap_bp(s) for s in detail["timeline"]["steps"]]
    if not gaps:
        return 0.0
    if push == 0:
        return sum(abs(g) for g in gaps) / len(gaps)
    return push * sum(gaps) / len(gaps)


def premium_bp(detail):
    """The sizes of the session's premium episodes, in bp: the nonzero
    premiums of its waypoints."""
    path = detail["scenario"].get("path", [])
    return [abs(w["premium_pct"]) * 100 for w in path if w.get("premium_pct")]


def displacement(set_dirs, name, push, baseline="nop"):
    """How far a rung moved the mark from the index beyond what doing nothing
    does, in the direction it pushes: the mean over sessions of the paired
    difference, in bp, and the median premium episode of those sessions, in
    bp. None when the details weren't written."""
    diffs, premiums = [], []
    for set_dir in set_dirs:
        mine, base = read_details(set_dir, name), read_details(set_dir, baseline)
        for file in mine.keys() & base.keys():
            diffs.append(mean_gap(mine[file], push) - mean_gap(base[file], push))
            premiums += premium_bp(mine[file])
    if not diffs:
        return None
    return {
        "bp": sum(diffs) / len(diffs),
        "sessions": len(diffs),
        "median_premium_bp": statistics.median(premiums) if premiums else None,
    }


def value(term, skill):
    term = term.strip()
    m = re.fullmatch(r"(max|min)\((.*)\)", term)
    if m:
        parts = [value(p, skill) for p in m.group(2).split(",")]
        return max(parts) if m.group(1) == "max" else min(parts)
    if term not in skill:
        raise SystemExit(f"rule term {term!r} is not a rung of this ladder")
    return skill[term]


def holds(rule, skill):
    terms = [t for t in rule.split("<")]
    values = [value(t, skill) for t in terms]
    return all(a < b for a, b in zip(values, values[1:]))


# --- the probe gate (scoring v2) -------------------------------------------

PROBE_CONFIDENCE = 0.95
PROBE_TOLERANCE = 0.02


def v2_terms(report):
    """A rung's v2 grade as per-session terms (weighted bot, reference and
    ceiling USD), the anchor floor, and the grade's own `raw`. None when the
    grade carries no v2."""
    v2 = report.get("v2")
    if not v2:
        return None
    return {
        "raw": v2["raw"],
        "fatal": v2.get("fatal") is not None,
        "floor": v2["anchor_floor"],
        "breached": v2.get("breached_sessions", 0),
        "floored": v2.get("anchor_floored", False),
        "sessions": [(s["bot_usd"], s["reference_usd"], s["ceiling_usd"]) for s in v2["sessions"]],
    }


def v2_raw(terms, idx=None):
    """v2's `raw` over sessions `idx` (all by default): 0.5 at the reference,
    1 at the ceiling, 0 for doing nothing (see crates/grader/src/scoring_v2.rs).
    A fatal grade keeps the raw the grader gave it."""
    if terms["fatal"]:
        return terms["raw"]
    rows = terms["sessions"] if idx is None else [terms["sessions"][i] for i in idx]
    bot = sum(r[0] for r in rows)
    ref_measured = sum(r[1] for r in rows)
    ceiling = sum(r[2] for r in rows)
    ref = max(ref_measured, terms["floor"] * ceiling)
    if ref <= 0:
        return float("nan")
    if bot <= ref:
        return 0.5 * bot / ref
    if ceiling <= ref:
        return float("nan")
    return 0.5 + 0.5 * (bot - ref) / (ceiling - ref)


def session_sharpe(sets, name):
    """Per-session Sharpe (mean over standard deviation of session PnL), over
    every set: not annualised."""
    xs = [p for reports in sets for p in pnls(reports[name])]
    if len(xs) < 2:
        return None
    sd = statistics.pstdev(xs)
    return statistics.fmean(xs) / sd if sd > 0 else 0.0


def probe_checks(probes, sets, count):
    """The probe rules over the sets' v2 grades. Each check is
    `{id, kind, rungs, verdict, ...}` with verdict `pass`, `fail`,
    `inconclusive` or `no_power`.

    Rules (`probes` in the ladder file; every raw is the mean over sets):

    - `zero_edge`: bots with no edge (gamblers, naive holds, martingale,
      limit-hugger, stop-lotto): raw at most `tolerance` over doing nothing's 0.
    - `pairs`: `[lower, higher]`: the first scores under the second (an
      occasional breacher under its compliant twin, a crashing bot under the
      bot it wraps). `breaches` lists the rungs that must have breached a
      session, or the pair is `no_power`.
    - `levered`: `{rung, m, base}`: the rung's weighted PnL is at most `m`
      times the base's (plus `tolerance` of the base's reference anchor), and
      when it breached anywhere it scores under the base: leverage that
      breaches never pays.
    - `honest`: `{rung, sharpe}`: a bot with an honest edge at that per-session
      Sharpe; from `pays_from` (default 0.2) it must beat doing nothing.
      The Sharpe it achieved is reported beside the target.
    - `reference`: the reference scores 0.5 within `tolerance`, unless its
      anchor was floored (then it is `pass` with the floor noted).
    """
    confidence = probes.get("confidence", PROBE_CONFIDENCE)
    tol = probes.get("tolerance", PROBE_TOLERANCE)
    terms = [{name: v2_terms(r) for name, r in reports.items()} for reports in sets]
    needed = set(probes.get("zero_edge", []))
    for a, b in probes.get("pairs", []):
        needed |= {a, b}
    for spec in probes.get("levered", []):
        needed |= {spec["rung"], spec["base"]}
    needed |= {h["rung"] for h in probes.get("honest", [])}
    needed.add("reference")
    for name in sorted(needed):
        if any(t.get(name) is None for t in terms):
            raise SystemExit(f"probe rung {name!r}: no v2 in its grade (is it in the ladder?)")

    def point(name):
        return statistics.fmean(v2_raw(t[name]) for t in terms)

    def drawn(rng):
        """`needed`'s mean raw and weighted PnL over one bootstrap draw of
        every set's sessions (the same sessions for every rung)."""
        picks = [[rng.randrange(len(t["reference"]["sessions"])) for _ in t["reference"]["sessions"]]
                 for t in terms]
        raw = {n: statistics.fmean(v2_raw(t[n], idx) for t, idx in zip(terms, picks))
               for n in needed}
        usd = {n: sum(sum(t[n]["sessions"][i][0] for i in idx) for t, idx in zip(terms, picks))
               for n in needed}
        return raw, usd

    base_raw = {n: point(n) for n in needed}
    base_usd = {n: sum(sum(r[0] for r in t[n]["sessions"]) for t in terms) for n in needed}
    rng = random.Random(0)
    draws = [drawn(rng) for _ in range(BOOTSTRAP_DRAWS)]

    def verdict(violated, point_violated, extra=None):
        odds = sum(violated(raw, usd) for raw, usd in draws) / len(draws)
        v = "pass"
        if point_violated:
            v = "fail" if odds >= confidence else "inconclusive"
        return {"verdict": v, "violation_odds": odds}

    breached = {n: sum(t[n]["breached"] for t in terms) for n in needed}
    out = []

    def add(check, kind, rungs, result, detail, **more):
        out.append({"id": check, "kind": kind, "rungs": rungs, "detail": detail,
                    "tolerance": tol, "confidence": confidence, **result, **more})

    for name in probes.get("zero_edge", []):
        v = verdict(lambda raw, usd, n=name: raw[n] > tol, base_raw[name] > tol)
        add(f"zero_edge:{name}", "zero_edge", [name], v,
            f"raw {base_raw[name]:+.3f}, limit {tol:+.2f}", raw=base_raw[name])
    breach_rungs = set(probes.get("breaches", []))
    for lower, higher in probes.get("pairs", []):
        v = verdict(lambda raw, usd, a=lower, b=higher: raw[a] >= raw[b],
                    base_raw[lower] >= base_raw[higher])
        detail = f"raw {base_raw[lower]:+.3f} under {higher}'s {base_raw[higher]:+.3f}"
        if lower in breach_rungs:
            detail += f", {breached[lower]} breached sessions"
            if breached[lower] == 0:
                v = {"verdict": "no_power", "violation_odds": v["violation_odds"]}
                detail += ": it never breached"
        add(f"pair:{lower}<{higher}", "pair", [lower, higher], v, detail,
            raw=base_raw[lower], other_raw=base_raw[higher], breached_sessions=breached[lower])
    for spec in probes.get("levered", []):
        name, base, m = spec["rung"], spec["base"], spec["m"]
        slack = tol * abs(base_usd["reference"])

        def too_much(raw, usd, n=name, b=base, m=m, slack=slack):
            return usd[n] > m * max(usd[b], 0.0) + slack

        def pays_breaching(raw, usd, n=name, b=base):
            return breached[n] > 0 and raw[n] >= raw[b]

        v = verdict(lambda raw, usd: too_much(raw, usd) or pays_breaching(raw, usd),
                    too_much(base_raw, base_usd) or pays_breaching(base_raw, base_usd))
        add(f"levered:{name}", "levered", [name, base], v,
            f"weighted PnL ${base_usd[name]:.0f} against {m}x ${base_usd[base]:.0f}, raw "
            f"{base_raw[name]:+.3f} against {base_raw[base]:+.3f}, {breached[name]} breached sessions",
            raw=base_raw[name], breached_sessions=breached[name])
    pays_from = probes.get("pays_from", 0.2)
    for spec in probes.get("honest", []):
        name = spec["rung"]
        achieved = session_sharpe(sets, name)
        detail = f"raw {base_raw[name]:+.3f}, per-session Sharpe {achieved:.2f} (target {spec['sharpe']})" \
            if achieved is not None else f"raw {base_raw[name]:+.3f}"
        if spec["sharpe"] >= pays_from:
            v = verdict(lambda raw, usd, n=name: raw[n] <= 0.0, base_raw[name] <= 0.0)
        else:
            v = {"verdict": "pass", "violation_odds": None}
            detail += "; below the pay line, reported only"
        add(f"honest:{name}", "honest", [name], v, detail, raw=base_raw[name],
            sharpe=achieved, target_sharpe=spec["sharpe"])
    floored = any(t["reference"]["floored"] for t in terms)
    ref = base_raw["reference"]
    ok = floored or abs(ref - 0.5) <= max(tol, 0.05)
    add("reference", "reference", ["reference"],
        {"verdict": "pass" if ok else "fail", "violation_odds": None},
        f"raw {ref:.3f}" + (" (anchor floored)" if floored else ""), raw=ref)
    return out


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
    parser.add_argument("--public-seed", help="draw fixed, public sets from this label")
    args = parser.parse_args()
    if args.parallel < 1 or args.parallel * args.jobs > BOT_USERS:
        parser.error(f"--parallel × --jobs must be between 1 and {BOT_USERS}, one bot user each")
    # One range of bot users per rung graded at once.
    users = queue.Queue()
    for k in range(args.parallel):
        users.put(k * args.jobs)
    with open(os.path.join(ROOT, "bench/tasks", args.task, "checks/ladder.json")) as f:
        config = json.load(f)
    count = args.count or config["count"]
    out = os.path.join(args.out, args.task)
    os.makedirs(out, exist_ok=True)

    probes = None
    if config.get("probes"):
        probes = dict(config["probes"],
                      reference=os.path.join(ROOT, "bench/tasks", args.task, "reference"))
    sets = []
    set_dirs = []
    for k in range(args.sets):
        set_dir = os.path.join(out, f"set-{k + 1}")
        set_dirs.append(set_dir)
        os.makedirs(set_dir, exist_ok=True)
        # The secret lives only in this process and the graders it starts.
        secret_file = os.path.join(set_dir, ".secret")
        if args.public_seed:
            label = f"mtx-ladder-public:{args.public_seed}:{config['family']}:set-{k + 1}"
            secret = hashlib.sha256(label.encode()).hexdigest()
        elif os.path.exists(secret_file):
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
                    rung, secret, set_dir, users, probes,
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
        "public_seed": args.public_seed,
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
    rungs = {r["name"]: r for r in config["rungs"]}
    for name in limits["bots"]:
        all_pnls = [p for reports in sets for p in pnls(reports[name])]
        reference = sum(sum(pnls(r["reference"])) for r in sets)
        # Undefined when the reference lost money on these sets: null in the
        # summary, never NaN (not JSON).
        ratio = sum(all_pnls) / reference if reference > 0 else None
        profitable = sum(p > 0 for p in all_pnls) / len(all_pnls)
        reward = max(r[name]["reward"] for r in sets)
        reasons = []
        if profitable > limits["max_profitable"]:
            reasons.append(f"profitable in {profitable:.0%} of sessions")
        # Knowing the generator shouldn't pay more than knowing the regime.
        below = limits.get("below")
        ceiling = below_odds = None
        if below:
            ceiling = sum(sum(pnls(r[below])) for r in sets)
            if sum(all_pnls) >= ceiling:
                reasons.append(f"${sum(all_pnls):.0f}, at or above {below}'s ${ceiling:.0f}")
            # How often a set like this one would put it at or above: the
            # sessions resampled in pairs (both bots played the same ones).
            pairs = list(zip(all_pnls, [p for r in sets for p in pnls(r[below])]))
            rng = random.Random(0)
            hits = 0
            for _ in range(BOOTSTRAP_DRAWS):
                draw = [pairs[rng.randrange(len(pairs))] for _ in pairs]
                hits += sum(e for e, _ in draw) >= sum(b for _, b in draw)
            below_odds = hits / BOOTSTRAP_DRAWS
        if reward > 0.999:
            reasons.append("reached the bound")
        # A pusher that moved nothing, or less than the family's premium
        # episodes, proves nothing by losing: `no_power`, not a pass.
        moved, no_power = None, None
        rung = rungs[name]
        if "push" in rung:
            moved = displacement(set_dirs, name, rung["push"])
            if moved is None:
                no_power = "mark displacement unmeasured (no session details)"
            elif moved["bp"] < MIN_DISPLACEMENT_BP:
                no_power = f"moved the mark {moved['bp']:.2f} bp"
            elif rung.get("calibrate") and moved["median_premium_bp"] is not None \
                    and moved["bp"] < moved["median_premium_bp"]:
                no_power = (f"moved the mark {moved['bp']:.1f} bp, under the median premium "
                            f"episode's {moved['median_premium_bp']:.1f} bp")
        # The sample behind each verdict, for a reader (`mtx gates --fast`)
        # judging a short set.
        summary["exploits"].append({
            "bot": name, "ok": not reasons, "ratio": ratio, "profitable": profitable,
            "reasons": reasons, "sessions": len(all_pnls),
            "profitable_sessions": sum(p > 0 for p in all_pnls),
            "max_profitable": limits["max_profitable"], "total": sum(all_pnls),
            "below": below, "below_total": ceiling, "below_odds": below_odds,
            "reached_bound": reward > 0.999,
            "displacement": moved,
            "verdict": "fail" if reasons else ("no_power" if no_power else "pass"),
            "no_power": no_power,
        })
        multiple = f"{ratio:.2f}x the reference" if ratio is not None else "no ratio (the reference lost money)"
        label = "FAIL" if reasons else ("NOPW" if no_power else "ok  ")
        print(f"  {label} {name}: {multiple}, "
              f"profitable {profitable:.0%}, worst ${min(all_pnls):.0f}, total ${sum(all_pnls):.0f}"
              + (f" ({'; '.join(reasons)})" if reasons else "")
              + (f" [no_power: {no_power}]" if no_power else "")
              + (f" [displacement {moved['bp']:.1f} bp over {moved['sessions']} sessions]"
                 if moved else ""))
        if reasons:
            failed.append(name)
        elif no_power:
            failed.append(f"{name} no_power")

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

    if config.get("probes"):
        print("\nprobes (scoring v2; raw: 0 = doing nothing, 0.5 = the reference):")
        summary["probes"] = probe_checks(config["probes"], sets, count)
        for check in summary["probes"]:
            label = {"pass": "ok  ", "fail": "FAIL", "inconclusive": "?   ", "no_power": "NOPW"}[
                check["verdict"]]
            print(f"  {label} {check['id']}: {check['detail']}"
                  + (f" [odds {check['violation_odds']:.2f}]"
                     if check.get("violation_odds") is not None else ""))
            if check["verdict"] in ("fail", "no_power"):
                failed.append(f"probe {check['id']} {check['verdict']}")

    summary["failed"] = failed
    with open(os.path.join(out, "summary.json"), "w") as f:
        json.dump(summary, f, indent=2)
    print("\nladder " + ("FAILED: " + "; ".join(failed) if failed else "holds"))
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()

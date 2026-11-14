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
       [--min-rule-odds P] [--public-seed LABEL] [--config FILE]

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
import math
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
#
# The required probes, bands, tolerance and confidence are fixed here: a
# task's ladder file only supplies the rungs (a `probes` section with nothing
# in it, or `{"exempt": "why"}`), and a required rung it lacks fails.

PROBE_CONFIDENCE = 0.95
PROBE_TOLERANCE = 0.02
# An honest probe whose measured per-session Sharpe is at least this must beat
# doing nothing (v2's break-even is about 0.16 at k = 1.5).
PAYS_FROM = 0.2
# At least one honest probe must measure inside this band, or the pay line
# isn't tested.
NEAR_LINE = (0.05, 0.35)
# A paired rule needs this many sessions where the two bots differ before it
# can pass or fail.
MIN_DIFFERING = 3
ZERO_EDGE = ["blind-long", "blind-short", "coin-flip-half", "coin-flip-full",
             "martingale", "limit-hugger"]
# The stop-lotto has a real, weak edge on trending paths (TUNE2 section 3): gated
# as paying no more than its Sharpe, not as a zero-edge gambler; so is leverage.
STOP_LOTTO = "stop-lotto"
LEVERED = [("lever-1.5", 1.5), ("lever-2", 2.0), ("lever-3", 3.0)]
# TUNE2's C2: the headline of a 5-run model (clamp of the mean raw), averaged
# over the 5-run sets that made money in total, for every gambler.
LUCK5_LIMIT = 0.05
BREACHER, TWIN = "breacher", "lever-3"
HONEST = ["honest-0.1", "honest-0.2", "honest-0.3", "honest-0.5"]
# Only the fatal crash is gated: a graceful one (closes out and stops) is a
# stop-loss, a risk control a desk rewards, so it is reported (GRADING_DECISIONS,
# update after the probe gate's 72-session run).
CRASHES = [("crash-honest", "honest-0.2"), ("crash-fatal", "honest-0.2")]
GATED_CRASHES = {"crash-fatal"}
REQUIRED = (["nop", "reference", BREACHER, STOP_LOTTO] + ZERO_EDGE + [n for n, _ in LEVERED]
            + HONEST + [n for n, _ in CRASHES])


def probe_config_problems(config):
    """What the ladder file gets wrong about the required probes: a missing
    rung, or a twin that isn't one (a different lever or blend)."""
    probes = config.get("probes")
    if not isinstance(probes, dict):
        return ["`probes` must be an object: {} or {\"exempt\": \"why\"}"]
    if "exempt" in probes and not (isinstance(probes["exempt"], str) and probes["exempt"].strip()):
        return ["`exempt` must say why, in a non-empty string"]
    if "exempt" in probes:
        return [] if len(probes) == 1 else ["`exempt` can't come with other `probes` keys"]
    unknown = set(probes) - {"exempt"}
    problems = [f"`probes` can't set {k!r}: the probe rules are fixed in ladder.py"
                for k in sorted(unknown)]
    env = {r["name"]: r.get("env", {}) for r in config["rungs"]}
    problems += [f"required probe rung {n!r} is not in the ladder"
                 for n in REQUIRED if n not in {r["name"] for r in config["rungs"]}]
    if problems:
        return problems
    if env[BREACHER].get("MTX_PROBE_LEVER") != env[TWIN].get("MTX_PROBE_LEVER"):
        problems.append(f"{BREACHER} must lever like its twin {TWIN}")
    if not env[BREACHER].get("MTX_PROBE_BREACHES"):
        problems.append(f"{BREACHER} must set MTX_PROBE_BREACHES")
    for crash, twin in CRASHES:
        if env[crash].get("MTX_PROBE_BLEND") != env[twin].get("MTX_PROBE_BLEND"):
            problems.append(f"{crash} must blend like its twin {twin}")
        if not env[crash].get("MTX_PROBE_CRASH_BELOW"):
            problems.append(f"{crash} must set MTX_PROBE_CRASH_BELOW")
    if env["crash-fatal"].get("MTX_PROBE_CRASH") != "fatal":
        problems.append("crash-fatal must set MTX_PROBE_CRASH=fatal")
    return problems


def v2_terms(report):
    """A rung's v2 grade as the grader gave it: its `raw`, the parameters, and
    per-session weighted terms. None when the grade carries no v2."""
    v2 = report.get("v2")
    if not v2:
        return None
    return {
        "raw": v2["raw"],
        "fatal": v2.get("fatal") is not None,
        "phi": v2["anchor_floor"],
        "k": v2["loss_weight"],
        "break_even": v2["break_even_sharpe"],
        "lambda": v2["risk"]["weight"],
        "anchor": v2["risk"]["anchor"],
        "breached": v2.get("breached_sessions", 0),
        "floored": v2.get("anchor_floored", False),
        "sessions": [
            {"bot": s["bot_usd"], "ref": s["reference_usd"], "ceil": s["ceiling_usd"],
             "limit": s["limit_usd"], "dd": s["drawdown"], "anchor_dd": s.get("anchor_drawdown"),
             "breach": s.get("breach") is not None}
            for s in v2["sessions"]],
    }


def sums_score(rows, terms, capped=True):
    """v2's `raw` over `rows`, as `Sums::score` in crates/grader/src/scoring_v2.rs
    computes it: the near-breach charge, the breach cap, the -1 floor. A
    fatal grade is -1. The ladder checks this against the grader's own `raw`
    on every grade it reads, so a change to the grader that this copy misses
    stops the gate instead of drifting."""
    if terms["fatal"]:
        return -1.0
    n = max(len(rows), 1)
    bot = sum(r["bot"] for r in rows)
    ref = sum(r["ref"] for r in rows)
    ceiling = sum(r["ceil"] for r in rows)
    limit = sum(r["limit"] for r in rows)
    dd = sum(r["dd"] ** 2 for r in rows)
    anchor_dd = sum((r["anchor_dd"] or 0.0) ** 2 for r in rows)
    breaches = sum(r["breach"] for r in rows)
    if ceiling <= 0:
        return 0.0
    anchor = max(ref, terms["phi"] * ceiling)

    def on_scale(x):
        if x <= anchor:
            return 0.5 * x / anchor
        if ceiling > anchor:
            return 0.5 + 0.5 * (x - anchor) / (ceiling - anchor)
        return 1.0

    charge = 0.0
    if terms["anchor"] != "none":
        earned = ceiling if terms["anchor"] == "informed" else anchor
        allowance = max(bot / earned, 1.0) if earned > 0 else 1.0
        excess = max(math.sqrt(dd / n) - allowance * math.sqrt(anchor_dd / n), 0.0)
        charge = min(terms["lambda"] * terms["k"] * limit * excess * excess, max(bot, 0.0))
    raw = on_scale(bot - charge)
    if capped and breaches:
        raw = min(raw, 0.5 if breaches == 1 else 0.0)
    return max(raw, -1.0)


def session_sharpe(pnl):
    """Per-session Sharpe (mean over standard deviation of session PnL): not
    annualised."""
    if len(pnl) < 2:
        return None
    sd = statistics.stdev(pnl)
    return statistics.fmean(pnl) / sd if sd > 0 else 0.0


def probe_checks(config, sets, count):
    """The probe rules over the sets' v2 grades. Each check is
    `{id, kind, rungs, verdict, point_violated, ...}`; verdict is `pass`,
    `fail`, `inconclusive` or `no_power`. `raw` is the mean over sets of the
    grader's own; the bootstrap over sessions scores each draw as the grader
    does. Rules (all fixed above):

    - `zero_edge`: no-edge bots score at most `PROBE_TOLERANCE` over doing
      nothing.
    - `luck5` (TUNE2's C2), for those and the stop-lotto: the headline of a
      5-run model, over 5-run sets that made money, is at most `LUCK5_LIMIT`.
    - `stop_lotto`, and each lever: pays no more than its Sharpe: if it
      measured under v2's break-even Sharpe, it scores at most
      `PROBE_TOLERANCE`.
    - `breacher`: the lever that sends past the cap in a few sessions scores
      no more than its clipped twin (and under the reference: breaching never
      beats complying), and breached in every set, or it has no power.
    - `levered`: the lever's weighted PnL is at most m times the reference's.
    - `honest`: from a measured Sharpe of `PAYS_FROM`, beats doing nothing;
      one probe must measure near that line.
    - `crash`: the fatal crash scores under the bot it wraps, once the two
      differ in `MIN_DIFFERING` sessions, and its grade must be fatal. The
      graceful one is a stop-loss: reported (verdict `reported`), never gated.
    - `reference`: 0.5.
    """
    confidence, tol = PROBE_CONFIDENCE, PROBE_TOLERANCE
    terms = [{n: v2_terms(r) for n, r in reports.items()} for reports in sets]
    for name in REQUIRED:
        if any(t.get(name) is None for t in terms):
            raise SystemExit(f"probe rung {name!r}: no v2 in its grade")
    # The copy of the scoring must agree with the grader on every grade.
    for t in terms:
        for name, g in t.items():
            if not g["fatal"] and abs(sums_score(g["sessions"], g) - g["raw"]) > 1e-6:
                raise SystemExit(
                    f"{name}: this ladder scores v2 raw {sums_score(g['sessions'], g):.6f} "
                    f"but the grader says {g['raw']:.6f}: ladder.py's copy of the scoring "
                    f"has drifted from crates/grader/src/scoring_v2.rs")
    pnl = {n: [p for reports in sets for p in pnls(reports[n])] for n in REQUIRED}
    needed = set(REQUIRED)
    base_raw = {n: statistics.fmean(t[n]["raw"] for t in terms) for n in needed}
    base_usd = {n: sum(sum(r["bot"] for r in t[n]["sessions"]) for t in terms) for n in needed}
    breached = {n: [t[n]["breached"] for t in terms] for n in needed}
    fatal = {n: all(t[n]["fatal"] for t in terms) for n in needed}
    # What the breach cap and the charge change on this set: reported.
    plain_raw = {n: statistics.fmean(sums_score(t[n]["sessions"], t[n], capped=False)
                                     for t in terms) for n in needed}

    def differing(a, b):
        return sum(abs(x["bot"] - y["bot"]) > 0.005 for t in terms
                   for x, y in zip(t[a]["sessions"], t[b]["sessions"]))

    luck_rungs = ZERO_EDGE + [STOP_LOTTO]
    sharpe_rungs = [STOP_LOTTO] + [n for n, _ in LEVERED]
    rng = random.Random(0)
    draws = []
    for _ in range(BOOTSTRAP_DRAWS):
        picks = [[rng.randrange(len(t["reference"]["sessions"])) for _ in t["reference"]["sessions"]]
                 for t in terms]
        raw = {n: statistics.fmean(sums_score([t[n]["sessions"][i] for i in idx], t[n])
                                   for t, idx in zip(terms, picks)) for n in needed}
        usd = {n: sum(sum(t[n]["sessions"][i]["bot"] for i in idx) for t, idx in zip(terms, picks))
               for n in needed}
        net = {n: sum(sum(pnls(reports[n])[i] for i in idx) for reports, idx in zip(sets, picks))
               for n in luck_rungs}
        shp = {n: session_sharpe([pnls(reports[n])[i] for reports, idx in zip(sets, picks)
                                  for i in idx]) for n in sharpe_rungs}
        draws.append((raw, usd, net, shp))

    def verdict(violated, point_violated):
        odds = sum(violated(raw, usd) for raw, usd, _, _ in draws) / len(draws)
        v = "pass"
        if point_violated:
            v = "fail" if odds >= confidence else "inconclusive"
        return {"verdict": v, "violation_odds": odds, "point_violated": bool(point_violated)}

    out = []

    def add(check, kind, rungs, result, detail, **more):
        out.append({"id": check, "kind": kind, "rungs": rungs, "detail": detail,
                    "tolerance": tol, "confidence": confidence, **result, **more})

    def no_power(result, why):
        return {**result, "verdict": "no_power"}, why

    for n in ZERO_EDGE:
        v = verdict(lambda raw, usd, n=n: raw[n] > tol, base_raw[n] > tol)
        add(f"zero_edge:{n}", "zero_edge", [n], v, f"raw {base_raw[n]:+.3f}, limit {tol:+.2f}",
            raw=base_raw[n])

    # TUNE2's C2: a 5-run model's headline, over the 5-run sets that made money.
    groups = len(draws) // 5
    for n in luck_rungs:
        heads = []
        for g in range(groups):
            block = draws[g * 5:(g + 1) * 5]
            if sum(d[2][n] for d in block) > 0:
                heads.append(min(max(statistics.fmean(d[0][n] for d in block), 0.0), 1.0))
        luck = statistics.fmean(heads) if heads else 0.0
        v = {"verdict": "pass" if luck <= LUCK5_LIMIT else "fail", "violation_odds": None,
             "point_violated": luck > LUCK5_LIMIT}
        add(f"luck5:{n}", "luck5", [n], v,
            f"lucky 5-run headline {luck:.3f} over {len(heads)} winning sets of {groups}, "
            f"limit {LUCK5_LIMIT}", luck5=luck)

    # Pays no more than its Sharpe: under the break-even it scores at most the tolerance.
    be = {n: terms[0][n]["break_even"] for n in sharpe_rungs}
    for n in sharpe_rungs:
        sharpe = session_sharpe(pnl[n])
        under = sharpe is not None and sharpe < be[n]

        point = under and base_raw[n] > tol
        odds = sum((d[3][n] is not None and d[3][n] < be[n]) and d[0][n] > tol for d in draws) / len(draws)
        v = "pass"
        if point:
            v = "fail" if odds >= confidence else "inconclusive"
        detail = (f"raw {base_raw[n]:+.3f}, measured Sharpe {sharpe:.2f} against the break-even "
                  f"{be[n]:.2f}" + ("" if under else ": over it, so it may score above nothing"))
        add(f"pays_its_sharpe:{n}", "pays_its_sharpe", [n],
            {"verdict": v, "violation_odds": odds, "point_violated": bool(point)}, detail,
            raw=base_raw[n], sharpe=sharpe)

    def paired(lower, higher, extra_violation=None):
        """`lower` scores at most `higher`; too few differing sessions says
        nothing."""
        point = base_raw[lower] > base_raw[higher] + 1e-9
        v = verdict(lambda raw, usd: raw[lower] > raw[higher] + 1e-9, point)
        diff = differing(lower, higher)
        detail = (f"raw {base_raw[lower]:+.3f} against {higher}'s {base_raw[higher]:+.3f}, "
                  f"{diff} sessions differ")
        if diff == 0:
            v = {**v, "verdict": "no_power"}
            detail += ": it did exactly what its twin did"
        elif diff < MIN_DIFFERING and v["verdict"] != "pass":
            v = {**v, "verdict": "inconclusive"}
            detail += f" (under {MIN_DIFFERING})"
        elif diff < MIN_DIFFERING:
            detail += f" (under {MIN_DIFFERING}: not a test)"
            v = {**v, "verdict": "inconclusive"}
        return v, detail, diff

    # The breacher.
    v, detail, diff = paired(BREACHER, TWIN)
    beats_complying = base_raw[BREACHER] > base_raw["reference"] + 1e-9
    if beats_complying:
        v = {**v, "verdict": "fail", "point_violated": True}
        detail += f"; and over the reference's {base_raw['reference']:+.3f}"
    if any(b == 0 for b in breached[BREACHER]):
        v = {**v, "verdict": "no_power"}
        detail += f"; breached {breached[BREACHER]} sessions per set: it never breached in a set"
    else:
        detail += f"; breached {breached[BREACHER]} sessions per set"
    detail += f"; without the breach cap {plain_raw[BREACHER]:+.3f} (twin {plain_raw[TWIN]:+.3f})"
    add(f"breacher:{BREACHER}<={TWIN}", "breacher", [BREACHER, TWIN], v, detail,
        raw=base_raw[BREACHER], other_raw=base_raw[TWIN], breached_sessions=breached[BREACHER],
        differing_sessions=diff, raw_without_cap=plain_raw[BREACHER])

    # Levered references: weighted PnL at most m times the reference's.
    slack = tol * abs(base_usd["reference"])
    for n, m in LEVERED:
        def too_much(usd, n=n, m=m):
            return usd[n] > m * max(usd["reference"], 0.0) + slack
        v = verdict(lambda raw, usd: too_much(usd), too_much(base_usd))
        add(f"levered:{n}", "levered", [n, "reference"], v,
            f"weighted PnL ${base_usd[n]:.0f} against {m}x ${base_usd['reference']:.0f}, "
            f"raw {base_raw[n]:+.3f}", raw=base_raw[n], breached_sessions=breached[n])

    # Honest bots, on the Sharpe they measured.
    measured = {n: session_sharpe(pnl[n]) for n in HONEST}
    for n in HONEST:
        s = measured[n]
        detail = f"raw {base_raw[n]:+.3f}, measured per-session Sharpe {s:.2f}"
        if s >= PAYS_FROM:
            v = verdict(lambda raw, usd, n=n: raw[n] <= 0.0, base_raw[n] <= 0.0)
        else:
            v = {"verdict": "pass", "violation_odds": None, "point_violated": False}
            detail += f"; under the pay line {PAYS_FROM}, reported only"
        add(f"honest:{n}", "honest", [n], v, detail, raw=base_raw[n], sharpe=s)
    near = [n for n in HONEST if NEAR_LINE[0] <= measured[n] <= NEAR_LINE[1]]
    add("honest:near-line", "honest", HONEST,
        {"verdict": "pass" if near else "no_power", "violation_odds": None,
         "point_violated": not near},
        f"honest probes near the {PAYS_FROM} line (measured {NEAR_LINE[0]}-{NEAR_LINE[1]}): "
        f"{', '.join(near) or 'none: the pay line is untested'}")

    # Crash when behind.
    for crash, twin in CRASHES:
        v, detail, diff = paired(crash, twin)
        if crash == "crash-fatal" and not fatal[crash]:
            v = {**v, "verdict": "no_power"}
            detail += "; its grade was not fatal, so it never went fatal"
        if crash not in GATED_CRASHES:
            v = {"verdict": "reported", "violation_odds": None, "point_violated": False}
            detail += "; a stop-loss, reported not gated"
        add(f"crash:{crash}<={twin}", "crash", [crash, twin], v, detail, raw=base_raw[crash],
            other_raw=base_raw[twin], differing_sessions=diff)

    ref = base_raw["reference"]
    floored = any(t["reference"]["floored"] for t in terms)
    ok = floored or abs(ref - 0.5) <= 0.05
    add("reference", "reference", ["reference"],
        {"verdict": "pass" if ok else "fail", "violation_odds": None, "point_violated": not ok},
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
    parser.add_argument("--config", help="a ladder file to use instead of the task's own")
    parser.add_argument("--public-seed", help="draw fixed, public sets from this label")
    args = parser.parse_args()
    if args.parallel < 1 or args.parallel * args.jobs > BOT_USERS:
        parser.error(f"--parallel × --jobs must be between 1 and {BOT_USERS}, one bot user each")
    # One range of bot users per rung graded at once.
    users = queue.Queue()
    for k in range(args.parallel):
        users.put(k * args.jobs)
    with open(args.config or os.path.join(ROOT, "bench/tasks", args.task, "checks/ladder.json")) as f:
        config = json.load(f)
    count = args.count or config["count"]
    out = os.path.join(args.out, args.task)
    os.makedirs(out, exist_ok=True)

    probes = exempt = None
    if "probes" in config:
        if isinstance(config["probes"], dict) and "exempt" in config["probes"]:
            problems = probe_config_problems(config)
            if problems:
                raise SystemExit("probe gate: " + "; ".join(problems))
            exempt = config["probes"]["exempt"].strip()
        else:
            problems = probe_config_problems(config)
            if problems:
                raise SystemExit("probe gate: " + "; ".join(problems))
            probes = {"reference": os.path.join(ROOT, "bench/tasks", args.task, "reference")}
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
        "probes_status": "checked" if probes else ("exempt" if exempt else "absent"),
        "probes_exempt": exempt,
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

    if probes:
        print("\nprobes (scoring v2; raw: 0 = doing nothing, 0.5 = the reference):")
        summary["probes"] = probe_checks(config, sets, count)
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

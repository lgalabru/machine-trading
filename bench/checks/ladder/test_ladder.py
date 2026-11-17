"""Tests for ladder.py: `python3 -I bench/checks/ladder/test_ladder.py`."""

import gzip
import importlib.util
import json
import os
import re
import statistics
import subprocess
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("ladder", os.path.join(HERE, "ladder.py"))
ladder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ladder)


def read(wrapper):
    with open(os.path.join(wrapper, "run.sh")) as f:
        return f.read()


def write_details(set_dir, name, sessions):
    """`sessions`: scenario file name -> (steps as (mark, index), waypoints)."""
    folder = os.path.join(set_dir, "details", name, "bot")
    os.makedirs(folder)
    for file, (steps, path) in sessions.items():
        detail = {
            "scenario": {"path": path},
            "timeline": {"steps": [{"mark_usd": m, "index_usd": i} for m, i in steps]},
        }
        with gzip.open(os.path.join(folder, file), "wt") as f:
            json.dump(detail, f)


class OrderRules(unittest.TestCase):
    def test_a_rung_is_compared_with_nop(self):
        totals = {"nop": 0.0, "mark-pusher-up": -40.0, "mark-pusher-down": 12.0}
        self.assertTrue(ladder.holds("mark-pusher-up < nop", totals))
        self.assertFalse(ladder.holds("mark-pusher-down < nop", totals))

    def test_nop_joins_other_terms(self):
        totals = {"nop": 0.0, "a": -1.0, "b": -2.0, "reference": 5.0}
        self.assertTrue(ladder.holds("max(a, b) < nop < reference", totals))

    def test_a_rule_naming_no_rung_is_an_error(self):
        with self.assertRaises(SystemExit):
            ladder.holds("ghost < nop", {"nop": 0.0})


def report(pnl_by_session, breached=()):
    return {"scenarios": [
        {"pnl_usd": p, "bound_pnl_usd": 2000.0, "disqualified": "breached" if i in breached else None}
        for i, p in enumerate(pnl_by_session)
    ]}


class BreachingRungs(unittest.TestCase):
    """Rungs are compared on the grade the task gives them."""

    reports = {
        "nop": report([0.0, 0.0, 0.0]),
        "reference": report([100.0, 50.0, -20.0]),
        "momentum": report([900.0, 400.0, -300.0], breached={2}),
    }

    def test_a_breaching_rung_scores_0_on_a_v1_task_however_much_it_earned(self):
        self.assertEqual(ladder.totals(self.reports)["momentum"], 0.0)
        self.assertTrue(ladder.holds("momentum < reference", ladder.totals(self.reports)))

    def test_its_raw_pnl_and_breaches_are_information_beside_the_total(self):
        self.assertEqual(ladder.raw_totals(self.reports)["momentum"], 1000.0)
        self.assertEqual(ladder.breach_sessions(self.reports["momentum"]), 1)
        self.assertEqual(ladder.breach_sessions(self.reports["reference"]), 0)

    def test_a_v2_task_compares_the_v2_value_with_the_breach_floored(self):
        def v2(values):
            return {"sessions": [{"bot_usd": b, "ceiling_usd": 500.0} for b in values]}
        # The ceiling is the informed rung's own value: hindsight must not use it.
        reports = {
            "nop": dict(report([0.0] * 3), v2=v2([0.0] * 3)),
            "reference": dict(report([100.0, 50.0, -20.0]), v2=v2([100.0, 50.0, -30.0])),
            "momentum": dict(report([900.0, 400.0, -300.0], breached={2}),
                             v2=v2([900.0, 400.0, -1500.0])),
        }
        ladder.GRADE_BASIS = "v2"
        try:
            t = ladder.totals(reports)
        finally:
            ladder.GRADE_BASIS = "v1"
        self.assertEqual(t["momentum"], -200.0)
        self.assertEqual(t["reference"], 120.0)
        self.assertEqual(t["hindsight"], 6000.0)
        informed = dict(report([500.0] * 3), v2=v2([500.0] * 3))
        ladder.GRADE_BASIS = "v2"
        try:
            t = ladder.totals(dict(reports, informed=informed))
        finally:
            ladder.GRADE_BASIS = "v1"
        self.assertTrue(ladder.holds("reference < informed < hindsight", t))

    def test_a_v2_task_with_exempt_probes_is_still_graded_with_v2(self):
        with tempfile.TemporaryDirectory() as root:
            tests = os.path.join(root, "bench/tasks/t-v2/tests")
            os.makedirs(tests)
            with open(os.path.join(tests, "test.sh"), "w") as f:
                f.write("mtx grade --scoring v2-headline --loss-weight 1.5 --informed x\n")
            config = {"rungs": [{"name": "informed", "dir": "bench/checks/ladder/rungs/x"}],
                      "anchors": {"informed": "informed"}, "probes": {"exempt": "why"}}
            ladder.GRADE_BASIS = "v2"
            try:
                grading = ladder.rung_grading("t-v2", config, None, root)
                ladder.GRADE_BASIS = "v1"
                self.assertIsNone(ladder.rung_grading("t-v2", config, None, root))
            finally:
                ladder.GRADE_BASIS = "v1"
            self.assertIn("reference", grading)
            self.assertTrue(grading["informed"].endswith("bench/checks/ladder/rungs/x"))


class ExploitRatio(unittest.TestCase):
    def test_a_ladder_file_that_sets_a_multiple_enforces_it(self):
        self.assertIn("2.50x", ladder.ratio_reason(2.5, {"max_ratio": 2.0}))
        self.assertIsNone(ladder.ratio_reason(1.9, {"max_ratio": 2.0}))

    def test_there_is_no_default_multiple(self):
        self.assertIsNone(ladder.ratio_reason(40.0, {}))

    def test_no_ratio_is_not_a_reason(self):
        self.assertIsNone(ladder.ratio_reason(None, {"max_ratio": 2.0}))


class RungEnvironment(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def test_a_rung_without_env_starts_from_its_own_folder(self):
        rung = {"name": "x", "dir": "bench/checks/ladder/blind-long"}
        self.assertEqual(
            ladder.rung_bot(rung, self.tmp.name),
            os.path.join(ladder.ROOT, "bench/checks/ladder/blind-long"),
        )

    def test_env_reaches_the_rungs_run_sh(self):
        rung = {
            "name": "pusher-up",
            "dir": "bench/checks/exploits/mark-pusher",
            "env": {"MTX_PUSHER_MODE": "short-push-up", "ODD": "it's $HOME"},
        }
        real = os.path.join(ladder.ROOT, rung["dir"], "run.sh")
        self.assertTrue(os.path.isfile(real))
        wrapper = ladder.rung_bot(rung, self.tmp.name)
        self.assertEqual(wrapper, os.path.join(self.tmp.name, "rungs", "pusher-up"))
        # The wrapper sets the variables, then starts the real run.sh: here a
        # stand-in that prints them.
        stand_in = os.path.join(self.tmp.name, "stand-in.sh")
        with open(stand_in, "w") as f:
            f.write('echo "$MTX_PUSHER_MODE|$ODD|$1"\n')
        script = read(wrapper)
        self.assertIn(f"exec bash {real}", script)
        script = script.replace(real, stand_in)
        out = subprocess.run(["bash", "-c", script, "--", "arg"], capture_output=True, text=True)
        self.assertEqual(out.stdout.strip(), "short-push-up|it's $HOME|arg")

    def test_the_two_modes_get_their_own_wrappers(self):
        up = {"name": "u", "dir": "bench/checks/exploits/mark-pusher", "env": {"MTX_PUSHER_MODE": "a"}}
        down = dict(up, name="d", env={"MTX_PUSHER_MODE": "b"})
        a, b = ladder.rung_bot(up, self.tmp.name), ladder.rung_bot(down, self.tmp.name)
        self.assertNotEqual(a, b)
        self.assertIn("MTX_PUSHER_MODE=a", read(a))
        self.assertIn("MTX_PUSHER_MODE=b", read(b))

    def test_env_cannot_override_what_the_grader_sets(self):
        for name in ("MTX_RPC_URL", "MTX_KEYPAIR", "PATH", "lowercase", "A B"):
            rung = {"name": "x", "dir": "bench/checks/exploits/mark-pusher", "env": {name: "1"}}
            with self.assertRaises(SystemExit):
                ladder.rung_bot(rung, self.tmp.name)

    def test_env_needs_a_run_sh(self):
        rung = {"name": "x", "dir": "bench/checks/ladder/rungs", "env": {"A": "1"}}
        with self.assertRaises(SystemExit):
            ladder.rung_bot(rung, self.tmp.name)


class MarkDisplacement(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.set_dir = self.tmp.name

    def test_displacement_is_paired_with_nop_in_the_pushed_direction(self):
        waypoints = [{"premium_pct": 0.0}, {"premium_pct": 0.5}, {"premium_pct": -0.3}]
        # nop: mark 100.1 over index 100 (10 bp); pusher: 100.3 (30 bp).
        write_details(self.set_dir, "nop", {"s1.json.gz": ([(100.1, 100.0)] * 4, waypoints)})
        write_details(self.set_dir, "up", {"s1.json.gz": ([(100.3, 100.0)] * 4, waypoints)})
        got = ladder.displacement([self.set_dir], "up", 1)
        self.assertAlmostEqual(got["bp"], 20.0, places=6)
        self.assertEqual(got["sessions"], 1)
        # Premium episodes: 0.5% and 0.3%, in bp: 50 and 30; the median is 40.
        self.assertAlmostEqual(got["median_premium_bp"], 40.0)
        # Pushing down from the same marks is the opposite.
        self.assertAlmostEqual(ladder.displacement([self.set_dir], "up", -1)["bp"], -20.0, places=6)

    def test_either_way_counts_the_size_of_the_gap(self):
        write_details(self.set_dir, "nop", {"s1.json.gz": ([(100.0, 100.0)] * 2, [])})
        write_details(self.set_dir, "wash", {"s1.json.gz": ([(99.8, 100.0), (100.2, 100.0)], [])})
        got = ladder.displacement([self.set_dir], "wash", 0)
        self.assertAlmostEqual(got["bp"], 20.0, places=6)
        self.assertIsNone(got["median_premium_bp"])

    def test_a_session_on_several_markets_counts_every_markets_gap(self):
        def write(name, gaps_bp):
            folder = os.path.join(self.set_dir, "details", name, "bot")
            os.makedirs(folder)
            markets = [
                {"symbol": s, "mark_usd": 100.0 * (1 + g / 1e4), "index_usd": 100.0}
                for s, g in zip(["NVDA", "SPCX"], gaps_bp)
            ]
            # The step's own fields are the primary's; the markets list has both.
            step = {"mark_usd": markets[0]["mark_usd"], "index_usd": 100.0, "markets": markets}
            detail = {
                "scenario": {"path": [], "markets": [
                    {"symbol": "NVDA", "path": [{"premium_pct": 0.2}]},
                    {"symbol": "SPCX", "path": [{"premium_pct": -0.6}]},
                ]},
                "timeline": {"steps": [step] * 3},
            }
            with gzip.open(os.path.join(folder, "s1.json.gz"), "wt") as f:
                json.dump(detail, f)
        write("nop", [0.0, 0.0])
        # Pushes SPCX's mark 40 bp, NVDA's not at all: 20 bp a market.
        write("up", [0.0, 40.0])
        got = ladder.displacement([self.set_dir], "up", 1)
        self.assertAlmostEqual(got["bp"], 20.0, places=6)
        # Each market's premium episodes: 20 and 60 bp.
        self.assertAlmostEqual(got["median_premium_bp"], 40.0)

    def test_no_details_is_unmeasured(self):
        self.assertIsNone(ladder.displacement([self.set_dir], "up", 1))

    def test_a_pusher_that_moved_nothing_has_no_displacement(self):
        steps = [(100.1, 100.0)] * 3
        write_details(self.set_dir, "nop", {"s1.json.gz": (steps, [])})
        write_details(self.set_dir, "up", {"s1.json.gz": (steps, [])})
        self.assertAlmostEqual(ladder.displacement([self.set_dir], "up", 1)["bp"], 0.0)


def weighted_loss(x, k=1.5):
    return k * x if x < 0 else x


def grade(bot, ref, ceiling, breach=None, pnl=None, limit=600.0, dd=0.2, anchor_dd=0.2,
          fatal=False, phi=0.05, anchor="reference", k=1.5, lam=1.0):
    """A grade with a v2, scored as the grader would (the ladder checks its
    copy against `raw`)."""
    breach = breach or [False] * len(bot)
    sessions = [{"bot_usd": b, "reference_usd": r, "ceiling_usd": c, "limit_usd": limit,
                 "drawdown": 1.0 if x else dd, "anchor_drawdown": anchor_dd,
                 "nop_usd": 0.0, "breach": "over the cap" if x else None}
                for b, r, c, x in zip(bot, ref, ceiling, breach)]
    terms = {"raw": 0.0, "fatal": fatal, "phi": phi, "k": k, "lambda": lam, "anchor": anchor,
             "c": 1.25, "sessions": [{"bot": s["bot_usd"], "ref": s["reference_usd"], "ceil": s["ceiling_usd"],
                           "limit": limit, "dd": s["drawdown"], "anchor_dd": anchor_dd,
                           "breach": x} for s, x in zip(sessions, breach)]}
    raw = ladder.sums_score(terms["sessions"], terms)
    v2 = {"raw": raw, "anchor_floor": phi, "loss_weight": k, "break_even_sharpe": 0.16, "risk": {"weight": lam, "anchor": anchor},
          "breach_rate_weight": 1.25, "breached_sessions": sum(breach), "anchor_floored": False, "sessions": sessions}
    if fatal:
        v2["fatal"] = "refused"
    return {"v2": v2, "scenarios": [{"pnl_usd": p} for p in (pnl or bot)]}


class ProbeGate(unittest.TestCase):
    N = 40

    def ladder(self, **bots):
        """One set of N sessions: the reference earns 10 each, the ceiling 40;
        every required rung is there (as nop-like zeros unless `bots` says)."""
        ref, ceiling = [10.0] * self.N, [40.0] * self.N
        reports = {n: grade([0.0] * self.N, ref, ceiling) for n in ladder.REQUIRED}
        reports["reference"] = grade(ref, ref, ceiling)
        for name, spec in bots.items():
            spec = dict(spec) if isinstance(spec, dict) else {"bot": spec}
            reports[name] = grade(spec.pop("bot"), ref, ceiling, **spec)
        return [reports]

    def checks(self, **bots):
        return {c["id"]: c for c in ladder.probe_checks({}, self.ladder(**bots), self.N)}

    def test_the_copy_of_the_scoring_matches_the_formulas(self):
        t = lambda bot, **kw: ladder.sums_score(
            ladder.v2_terms(grade(bot, [10.0] * 4, [40.0] * 4, **kw))["sessions"],
            ladder.v2_terms(grade(bot, [10.0] * 4, [40.0] * 4, **kw)))
        self.assertAlmostEqual(t([10.0] * 4), 0.5)
        self.assertAlmostEqual(t([40.0] * 4, dd=0.2, anchor_dd=0.2), 1.0 - 0.0, places=1)
        self.assertAlmostEqual(t([-5.0] * 4), -0.25)
        # Each breach takes c / n (1.25 / 4) off the run, then the -1 floor.
        free = [40.0] * 4
        breach = [True, False, False, False]
        terms = ladder.v2_terms(grade(free, [10.0] * 4, [40.0] * 4, breach=breach))
        self.assertAlmostEqual(
            ladder.sums_score(terms["sessions"], terms, penalised=False)
            - ladder.sums_score(terms["sessions"], terms), 1.25 / 4)
        self.assertEqual(ladder.sums_score(terms["sessions"], dict(terms, c=100.0)), -1.0)
        # A fatal grade is the floor, and a ceiling under the anchor is 1, not NaN.
        self.assertEqual(t([40.0] * 4, fatal=True), -1.0)
        self.assertEqual(ladder.sums_score(
            [{"bot": 9, "ref": 5, "ceil": 4, "limit": 1, "dd": 0, "anchor_dd": 0, "breach": False}],
            {"fatal": False, "phi": 0.05, "k": 1.5, "lambda": 1, "anchor": "none", "c": 1.25}), 1.0)

    def test_a_charge_takes_back_winnings_when_the_drawdown_is_far_past_the_anchors(self):
        bot, ref, ceiling = [30.0] * 4, [10.0] * 4, [40.0] * 4
        calm = grade(bot, ref, ceiling, dd=0.2, anchor_dd=0.2)["v2"]["raw"]
        wild = grade(bot, ref, ceiling, dd=0.95, anchor_dd=0.2)["v2"]["raw"]
        self.assertLess(wild, calm)

    def test_a_drifted_copy_stops_the_gate(self):
        sets = self.ladder()
        sets[0]["coin-flip-full"]["v2"]["raw"] += 0.1
        with self.assertRaises(SystemExit):
            ladder.probe_checks({}, sets, self.N)

    def test_a_gambler_that_loses_passes_and_one_that_wins_by_a_lot_fails(self):
        got = self.checks(**{"coin-flip-full": [-3.0, 1.0] * (self.N // 2),
                             "martingale": [8.0, -1.0] * (self.N // 2)})
        self.assertEqual(got["zero_edge:coin-flip-full"]["verdict"], "pass")
        self.assertEqual(got["zero_edge:martingale"]["verdict"], "fail")

    def test_a_violation_the_sample_cannot_confirm_is_inconclusive(self):
        got = self.checks(**{"coin-flip-full": [40.0, -39.0] * (self.N // 2)})
        self.assertEqual(got["zero_edge:coin-flip-full"]["verdict"], "inconclusive")
        self.assertTrue(got["zero_edge:coin-flip-full"]["point_violated"])

    def test_leverage_is_held_to_m_times_the_reference(self):
        wobble = [1.0, -1.0] * (self.N // 2)
        mild = [18.0 + w for w in wobble]
        wild = [25.0 + w for w in wobble]
        self.assertEqual(self.checks(**{"lever-2": mild})["levered:lever-2"]["verdict"], "pass")
        self.assertEqual(self.checks(**{"lever-2": wild})["levered:lever-2"]["verdict"], "fail")

    def test_an_honest_bot_is_held_to_the_sharpe_it_measured_not_its_label(self):
        # Label 0.3, but it measured under the pay line: reported only.
        noisy = [-30.0, 29.0] * (self.N // 2)
        got = self.checks(**{"honest-0.3": noisy})
        self.assertIn("under the pay line", got["honest:honest-0.3"]["detail"])
        self.assertEqual(got["honest:honest-0.3"]["verdict"], "pass")
        # Measured over the line and losing: it fails.
        # (the near-breach charge takes its winnings back).
        bot = [-4.0] * 30 + [28.0] * 10
        got = self.checks(**{"honest-0.5": {"bot": bot, "dd": 0.99}})
        self.assertGreaterEqual(ladder.session_sharpe(bot), ladder.PAYS_FROM)
        self.assertEqual(got["honest:honest-0.5"]["verdict"], "fail")

    def test_one_honest_probe_must_measure_near_the_line(self):
        got = self.checks()
        self.assertEqual(got["honest:near-line"]["verdict"], "no_power")
        got = self.checks(**{"honest-0.2": [1.25, -0.75] * (self.N // 2)})
        self.assertEqual(got["honest:near-line"]["verdict"], "pass")

    def test_a_short_set_cannot_judge_the_near_line_rung(self):
        # The Sharpe of twelve sessions is one session's: no honest rung is
        # sure to land in the band, so a short set reports it, and only a
        # full-size one holds the task to it.
        sets = self.ladder()
        short = {c["id"]: c for c in ladder.probe_checks({}, sets, 12)}
        self.assertEqual(short["honest:near-line"]["verdict"], "inconclusive")
        self.assertTrue(short["honest:near-line"]["point_violated"])
        full = {c["id"]: c for c in ladder.probe_checks({}, sets, 72)}
        self.assertEqual(full["honest:near-line"]["verdict"], "no_power")

    def test_the_fatal_crash_is_gated_and_must_go_fatal(self):
        twin = [4.0] * self.N
        worse = {"bot": [-9.0] * 10 + twin[10:]}
        got = self.checks(**{"honest-0.2": twin, "crash-fatal": twin})
        self.assertEqual(got["crash:crash-fatal<=honest-0.2"]["verdict"], "no_power")
        # One differing session proves nothing: not a failure either.
        one = {"bot": [5.0] + twin[1:], "fatal": True}
        got = self.checks(**{"honest-0.2": twin, "crash-fatal": one})
        self.assertEqual(got["crash:crash-fatal<=honest-0.2"]["verdict"], "inconclusive")
        # A fatal-mode probe whose grade isn't fatal has no power.
        got = self.checks(**{"honest-0.2": twin, "crash-fatal": worse})
        self.assertEqual(got["crash:crash-fatal<=honest-0.2"]["verdict"], "no_power")
        got = self.checks(**{"honest-0.2": twin, "crash-fatal": dict(worse, fatal=True)})
        self.assertEqual(got["crash:crash-fatal<=honest-0.2"]["verdict"], "pass")

    def test_the_graceful_crash_is_reported_and_never_gated(self):
        twin = [4.0] * self.N
        # It scores above the bot it wraps (a stop-loss pays): still not a failure.
        better = {"bot": [9.0] * self.N}
        got = self.checks(**{"honest-0.2": twin, "crash-honest": better})
        c = got["crash:crash-honest<=honest-0.2"]
        self.assertEqual(c["verdict"], "reported")
        self.assertFalse(c["point_violated"])

    def test_the_stop_lotto_may_pay_its_sharpe_but_not_more(self):
        # Under the break-even and scoring over nothing, confidently: fails.
        # (Steady small wins with one crash: Sharpe low, raw high, no charge.)
        bot = [6.0] * 39 + [-200.0]
        got = self.checks(**{"stop-lotto": {"bot": bot, "pnl": bot, "dd": 0.0, "anchor_dd": 0.0}})
        c = got["pays_its_sharpe:stop-lotto"]
        self.assertLess(ladder.session_sharpe(bot), 0.16)
        self.assertGreater(c["raw"], ladder.PROBE_TOLERANCE)
        # The point violation is recorded as it is; only confidence fails.
        self.assertTrue(c["point_violated"])
        self.assertEqual(c["verdict"], "fail" if c["violation_odds"] >= 0.95 else "pass")
        if c["verdict"] == "pass":
            self.assertIn("not confirmed", c["detail"])
        # A steady winner measures far over the break-even: allowed above nothing.
        got = self.checks(**{"stop-lotto": [6.0, 4.0] * (self.N // 2)})
        self.assertEqual(got["pays_its_sharpe:stop-lotto"]["verdict"], "pass")
        self.assertIn("over it", got["pays_its_sharpe:stop-lotto"]["detail"])

    def test_the_stop_lotto_is_not_held_to_c2(self):
        self.assertNotIn("luck5:stop-lotto", self.checks())

    def test_a_lucky_five_run_headline_fails_c2(self):
        got = self.checks(**{"coin-flip-full": [12.0, -1.0] * (self.N // 2)})
        self.assertEqual(got["luck5:coin-flip-full"]["verdict"], "fail")
        got = self.checks(**{"coin-flip-full": [-3.0, -1.0] * (self.N // 2)})
        self.assertEqual(got["luck5:coin-flip-full"]["verdict"], "pass")

    def test_c2_is_inconclusive_on_a_short_set(self):
        sets = self.ladder(**{"coin-flip-full": [12.0, -1.0] * (self.N // 2)})
        got = {c["id"]: c for c in ladder.probe_checks({}, sets, 12)}
        self.assertEqual(got["luck5:coin-flip-full"]["verdict"], "inconclusive")
        self.assertTrue(got["luck5:coin-flip-full"]["point_violated"])

    def test_a_floored_reference_has_no_power(self):
        sets = self.ladder()
        for t in sets:
            t["reference"]["v2"]["anchor_floored"] = True
        got = {c["id"]: c for c in ladder.probe_checks({}, sets, self.N)}
        self.assertEqual(got["reference"]["verdict"], "no_power")

    # A probe whose binary is broken (bad env, a bail on startup, a missing
    # binary) trades nothing or goes fatal: it must not pass the gate.
    VACUOUS = ["zero_edge:coin-flip-full", "luck5:coin-flip-full", "pays_its_sharpe:stop-lotto",
               "levered:lever-2"]

    def test_a_probe_that_never_traded_has_no_power(self):
        broken = {"coin-flip-full": [0.0] * self.N, "stop-lotto": [0.0] * self.N,
                  "lever-2": [0.0] * self.N}
        got = self.checks(**broken)
        for check in self.VACUOUS:
            self.assertEqual(got[check]["verdict"], "no_power", check)
            self.assertIn("never", got[check]["detail"])
        # A flat PnL with weighted PnL elsewhere is no trading either.
        flat = {"bot": [1.0, 2.0] * (self.N // 2), "pnl": [0.0] * self.N}
        self.assertEqual(self.checks(**{"coin-flip-full": flat})["zero_edge:coin-flip-full"]["verdict"],
                         "no_power")

    def test_a_probe_whose_grade_is_fatal_has_no_power(self):
        bots = {n: {"bot": [-1.0] * self.N, "fatal": True}
                for n in ("coin-flip-full", "stop-lotto", "lever-2")}
        got = self.checks(**bots)
        for check in self.VACUOUS:
            self.assertEqual(got[check]["verdict"], "no_power", check)
            self.assertIn("fatal", got[check]["detail"])

    def test_a_probe_that_trades_still_passes(self):
        wobble = [-3.0, 1.0] * (self.N // 2)
        got = self.checks(**{"coin-flip-full": wobble, "stop-lotto": wobble, "lever-2": wobble})
        for check in self.VACUOUS:
            self.assertEqual(got[check]["verdict"], "pass", check)

    def test_luck5_is_a_five_run_headline_whatever_the_set_count(self):
        """Each set is a run: five of them make the headline, so the same
        gambler's luck must not shrink as sets are added."""
        lucky = [12.0, -1.0, -3.0, 9.0, -4.0] * 8
        luck = {}
        for sets in (1, 4):
            ladders = []
            for _ in range(sets):
                ladders += self.ladder(**{"coin-flip-full": lucky})
            got = {c["id"]: c for c in ladder.probe_checks({}, ladders, self.N)}
            luck[sets] = got["luck5:coin-flip-full"]["luck5"]
        self.assertGreater(luck[1], 0.0)
        self.assertAlmostEqual(luck[4], luck[1], delta=0.1 * luck[1])

    # The breacher: composed runs with exactly b breach sessions.
    def breach_ladder(self, floor=True, bot_gain=12.5, breaches=6, free=300.0, limit=20.0,
                      twin=None):
        """Twin (the clipped lever) makes 10 a session; the breacher breaches
        in `breaches` sessions, where the session itself made `free` (its PnL):
        the grader floors it to -k * limit (`floor`) or, without the floor,
        scores it as it made; `bot_gain` in the rest (a modest 1.25x the twin's
        by default). Built through `grade`, so it is scored as the ladder
        scores."""
        breach = [True] * breaches + [False] * (self.N - breaches)
        pnl = [free] * breaches + [bot_gain] * (self.N - breaches)
        scored = [weighted_loss(free) if not floor
                  else min(-1.5 * limit, weighted_loss(free))] * breaches + \
            [bot_gain] * (self.N - breaches)
        return self.ladder(**{TWIN: twin or [10.0] * self.N,
                              "breacher": {"bot": scored, "pnl": pnl, "breach": breach,
                                           "limit": limit}})

    def breacher(self, sets):
        got = {c["id"]: c for c in ladder.probe_checks({}, sets, self.N)}
        return {k: v for k, v in got.items() if k.startswith("breacher:")}

    def test_a_breacher_with_its_floor_and_penalty_passes_every_gate_at_every_b(self):
        got = self.breacher(self.breach_ladder())
        self.assertEqual(sorted(got), sorted(
            f"breacher:b{b}:{g}" for b in (1, 2, 3) for g in ("rate_penalty", "floor")))
        for check, c in got.items():
            self.assertEqual(c["verdict"], "pass", (check, c["detail"]))
        # b = 1 is a test: one breach session is composed, not two.
        self.assertEqual(got["breacher:b1:floor"]["breach_sessions"], 1)
        # The penalty took exactly c * b / n.
        for b in (1, 2, 3):
            c = got[f"breacher:b{b}:rate_penalty"]
            self.assertAlmostEqual(c["raw_without_penalty"] - c["raw"], 1.25 * b / self.N)

    def test_a_breacher_that_never_breached_or_is_fatal_has_no_power(self):
        got = self.breacher(self.breach_ladder(breaches=0))
        self.assertTrue(got and all(c["verdict"] == "no_power" for c in got.values()))
        # Fewer breach sessions than b: b = 3 can't be composed.
        got = self.breacher(self.breach_ladder(breaches=2))
        self.assertEqual(got["breacher:b2:floor"]["verdict"], "pass")
        self.assertEqual(got["breacher:b3:floor"]["verdict"], "no_power")
        sets = self.breach_ladder()
        sets[0]["breacher"]["v2"]["fatal"] = "refused"
        got = self.breacher(sets)
        self.assertTrue(all(c["verdict"] == "no_power" for c in got.values()))

    def test_a_floor_that_does_not_bind_is_not_exercised_never_a_pass(self):
        """Flash-crash's shape: it breaches in nearly every session and the
        one clean session is flat; a breach session lost more than the floor,
        so the floor only takes back a loss and can't be tested. The penalty
        still binds and is checked."""
        got = self.breacher(self.breach_ladder(breaches=11, bot_gain=0.0, free=-700.0))
        for b in (1, 2, 3):
            self.assertEqual(got[f"breacher:b{b}:floor"]["verdict"], "not_exercised", b)
            self.assertIn("not exercised", got[f"breacher:b{b}:floor"]["detail"])
            self.assertEqual(got[f"breacher:b{b}:rate_penalty"]["verdict"], "pass", b)

    def test_the_floor_is_tested_on_the_sessions_where_it_binds(self):
        """Sessions that lost more than the floor are not where it binds: the
        composition picks the ones where breaching paid over the twin."""
        sets = self.breach_ladder(breaches=6)
        for i in (0, 1, 2):
            row = sets[0]["breacher"]["v2"]["sessions"][i]
            row["bot_usd"] = -1500.0
            sets[0]["breacher"]["scenarios"][i]["pnl_usd"] = -1000.0
        t = ladder.v2_terms(sets[0]["breacher"])
        sets[0]["breacher"]["v2"]["raw"] = ladder.sums_score(t["sessions"], t)
        got = self.breacher(sets)
        for b in (1, 2, 3):
            self.assertEqual(got[f"breacher:b{b}:floor"]["verdict"], "pass", b)

    def test_removing_the_per_session_floor_fails_the_gate(self):
        """The grader that no longer floors a breach session at -k * limit
        scores it as the session made: the swap run beats its twin."""
        got = self.breacher(self.breach_ladder(floor=False))
        for b in (1, 2, 3):
            self.assertEqual(got[f"breacher:b{b}:floor"]["verdict"], "fail", b)

    def test_a_breacher_that_gains_a_little_and_breaches_a_little_is_held_by_the_penalty(self):
        """About 1.25x the twin's gains and breaches in 1-3 of 40 sessions:
        without the penalty it would beat its twin; with it, it loses exactly
        c * b / n and scores below. A scorer that skips the penalty fails."""
        got = self.breacher(self.breach_ladder())
        for b in (1, 2, 3):
            c = got[f"breacher:b{b}:rate_penalty"]
            self.assertEqual(c["verdict"], "pass", b)
        self.assertGreater(got["breacher:b1:rate_penalty"]["raw_without_penalty"],
                           got["breacher:b1:rate_penalty"]["other_raw"])
        original = ladder.sums_score
        ladder.sums_score = lambda rows, terms, penalised=True: original(
            rows, terms, penalised=False)
        try:
            got = self.breacher(self.breach_ladder())
        finally:
            ladder.sums_score = original
        for b in (1, 2, 3):
            self.assertEqual(got[f"breacher:b{b}:rate_penalty"]["verdict"], "fail", b)

    def test_a_grade_without_the_breach_rate_weight_is_refused(self):
        sets = self.ladder()
        del sets[0]["reference"]["v2"]["breach_rate_weight"]
        with self.assertRaises(SystemExit):
            ladder.probe_checks({}, sets, self.N)

    def test_a_honest_probe_that_never_traded_has_no_power(self):
        got = self.checks(**{"honest-0.2": [0.0] * self.N})
        self.assertEqual(got["honest:honest-0.2"]["verdict"], "no_power")
        self.assertIn("never", got["honest:honest-0.2"]["detail"])
        got = self.checks(**{"honest-0.2": {"bot": [-1.0] * self.N, "fatal": True}})
        self.assertEqual(got["honest:honest-0.2"]["verdict"], "no_power")
        self.assertEqual(got["honest:near-line"]["verdict"], "no_power")

    def test_two_honest_probes_that_played_the_same_sessions_are_not_two_probes(self):
        same = [1.25, -0.75] * (self.N // 2)
        got = self.checks(**{"honest-0.1": same, "honest-0.2": same})
        self.assertNotEqual(got["honest:honest-0.1"]["verdict"], "no_power")
        self.assertEqual(got["honest:honest-0.2"]["verdict"], "no_power")
        self.assertIn("not a distinct probe", got["honest:honest-0.2"]["detail"])

    def test_honest_rungs_must_differ_in_blend_or_salt(self):
        env = {n: {} for n in ladder.REQUIRED}
        for n in ladder.HONEST:
            env[n] = {"MTX_PROBE_BLEND": "0.3"}
        env["crash-honest"] = {"MTX_PROBE_CRASH_BELOW": "0.1", "MTX_PROBE_BLEND": "0.3"}
        env["crash-fatal"] = dict(env["crash-honest"], MTX_PROBE_CRASH="fatal")
        env["breacher"] = {"MTX_PROBE_BREACHES": "1"}
        rungs = [{"name": n, "env": e} for n, e in env.items()]
        problems = " ".join(ladder.probe_config_problems({"probes": {}, "rungs": rungs}))
        self.assertIn("must differ from honest-0.1", problems)
        for i, n in enumerate(ladder.HONEST):
            env[n]["MTX_PROBE_SALT"] = str(i)
        env["crash-honest"]["MTX_PROBE_SALT"] = env["crash-fatal"]["MTX_PROBE_SALT"] = "1"
        rungs = [{"name": n, "env": e} for n, e in env.items()]
        problems = " ".join(ladder.probe_config_problems({"probes": {}, "rungs": rungs}))
        self.assertNotIn("must differ", problems)
        self.assertNotIn("draw like its twin", problems)

    def test_the_matched_sharpe_vol_pair_scores_long_vol_ahead(self):
        """TUNE2 section 3: at equal mean and spread, long-vol (rare big wins,
        frequent small losses) scores over short-vol (frequent small wins, rare
        big losses): losses weigh k times and short-vol's are the larger mass.
        Bots on replayed paths can't produce the pair, so it is checked on the
        scoring function."""
        n = 100
        short = [10.0] * 90 + [-50.0] * 10
        mean = statistics.fmean(short)
        long_ = [2 * mean - x for x in short]
        self.assertAlmostEqual(statistics.fmean(long_), mean)
        self.assertAlmostEqual(statistics.pstdev(long_), statistics.pstdev(short))
        ref, ceiling = [3.0] * n, [60.0] * n
        raw = {}
        for name, bot in (("short", short), ("long", long_)):
            weighted = [x if x >= 0 else 1.5 * x for x in bot]
            g = ladder.v2_terms(grade(weighted, ref, ceiling, pnl=bot, dd=0.0, anchor_dd=0.0))
            raw[name] = ladder.sums_score(g["sessions"], g)
        self.assertGreater(raw["long"], raw["short"])

    def test_the_sharpe_uses_n_minus_one_like_the_grader(self):
        self.assertAlmostEqual(ladder.session_sharpe([1.0, 3.0]), 2.0 / (2 ** 0.5))

    def test_an_exemption_must_say_why(self):
        for bad in ("", "   ", None, 3):
            problems = ladder.probe_config_problems({"probes": {"exempt": bad}, "rungs": []})
            self.assertTrue(any("exempt" in p for p in problems), bad)
        self.assertEqual(ladder.probe_config_problems(
            {"probes": {"exempt": "excluded"}, "rungs": []}), [])

    def test_a_missing_required_rung_fails_the_config(self):
        rungs = [{"name": n, "env": {}} for n in ladder.REQUIRED if n != "stop-lotto"]
        problems = ladder.probe_config_problems({"probes": {}, "rungs": rungs})
        self.assertTrue(any("stop-lotto" in p for p in problems))

    def test_a_task_cannot_loosen_the_rules(self):
        problems = ladder.probe_config_problems(
            {"probes": {"tolerance": 0.5, "zero_edge": []}, "rungs": []})
        self.assertTrue(any("tolerance" in p for p in problems))

    def test_twins_must_be_twins(self):
        env = {n: {} for n in ladder.REQUIRED}
        env["breacher"] = {"MTX_PROBE_LEVER": "2", "MTX_PROBE_BREACHES": "3"}
        env["lever-3"] = {"MTX_PROBE_LEVER": "3"}
        env["crash-honest"] = {"MTX_PROBE_BLEND": "0.4", "MTX_PROBE_CRASH_BELOW": "0.1"}
        env["crash-fatal"] = {"MTX_PROBE_BLEND": "0.4", "MTX_PROBE_CRASH_BELOW": "0.1"}
        env["honest-0.2"] = {"MTX_PROBE_BLEND": "0.5"}
        rungs = [{"name": n, "env": e} for n, e in env.items()]
        problems = " ".join(ladder.probe_config_problems({"probes": {}, "rungs": rungs}))
        self.assertIn("lever like its twin", problems)
        self.assertIn("blend like its twin", problems)
        self.assertIn("MTX_PROBE_CRASH=fatal", problems)

    def test_a_probe_rung_knows_its_sessions(self):
        with tempfile.TemporaryDirectory() as tmp:
            rung = {"name": "b", "dir": "bench/checks/probes", "env": {"MTX_PROBE": "coin-flip"}}
            path = os.path.join(ladder.rung_bot(rung, tmp, 72), "run.sh")
            with open(path) as f:
                self.assertIn("MTX_PROBE_SESSIONS=72", f.read())


class ProbeGradingSettings(unittest.TestCase):
    """The probe grades use the task's own v2 settings: what its verifier passes."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def task(self, script):
        folder = os.path.join(self.tmp.name, "bench/tasks/t-spcx/tests")
        os.makedirs(folder, exist_ok=True)
        with open(os.path.join(folder, "test.sh"), "w") as f:
            f.write(script)

    def command(self, grading):
        seen = []

        def fake(command, **_):
            seen.append(command)
            with open(command[command.index("--report") + 1], "w") as f:
                json.dump({}, f)
            return subprocess.CompletedProcess(command, 0)

        out = os.path.join(self.tmp.name, "out")
        os.makedirs(out)
        real = ladder.subprocess.run
        ladder.subprocess.run = fake
        try:
            ladder.run_grade("mtx", "snap", "fam", 12, 2, {"name": "p", "dir": "bots/starter"},
                             "secret", out, os.path.join(out, "p.json"), 0, grading)
        finally:
            ladder.subprocess.run = real
        return seen[0]

    def test_the_verifiers_flags_reach_the_probe_grade(self):
        self.task("mtx grade --draw fam \\\n  --loss-weight 2 --risk-weight=0.5 \\\n"
                  "  --anchor-floor 0.1 # --risk-weight 9\n")
        config = {"rungs": []}
        command = self.command(ladder.probe_grading("t-spcx", config, self.tmp.name))
        for flag, value in (("--loss-weight", "2"), ("--risk-weight", "0.5"),
                            ("--anchor-floor", "0.1")):
            self.assertEqual(command[command.index(flag) + 1], value)
        self.assertEqual(command.count("--risk-weight"), 1)
        self.assertNotIn("--informed", command)

    def test_a_verifier_without_flags_leaves_the_graders_defaults(self):
        self.task("mtx grade --draw fam\n")
        command = self.command(ladder.probe_grading("t-spcx", {"rungs": []}, self.tmp.name))
        for flag in ladder.VERIFIER_FLAGS + ("--informed",):
            self.assertNotIn(flag, command)

    def test_a_verifier_that_runs_the_informed_bot_has_the_probes_graded_against_it(self):
        self.task("mtx grade --informed /opt/mtx/anchors/fam-informed --reference /x\n")
        config = {"anchors": {"reference": "reference", "informed": "informed"},
                  "rungs": [{"name": "informed", "dir": "bench/tasks/t-spcx/checks/bots/informed"}]}
        command = self.command(ladder.probe_grading("t-spcx", config, self.tmp.name))
        self.assertEqual(
            command[command.index("--informed") + 1],
            os.path.join(self.tmp.name, "bench/tasks/t-spcx/checks/bots/informed"),
        )

    def test_an_informed_verifier_needs_an_informed_anchor_in_the_ladder_file(self):
        self.task("mtx grade --informed /opt/mtx/anchors/fam-informed\n")
        with self.assertRaises(SystemExit):
            ladder.probe_grading("t-spcx", {"rungs": []}, self.tmp.name)

    def test_a_repeated_flag_is_refused(self):
        self.task("mtx grade --loss-weight 1 --loss-weight 2\n")
        with self.assertRaises(SystemExit):
            ladder.probe_grading("t-spcx", {"rungs": []}, self.tmp.name)

    def test_every_tasks_probe_grade_matches_its_verifier(self):
        tasks = os.path.join(ladder.ROOT, "bench/tasks")
        for task in sorted(os.listdir(tasks)):
            test = os.path.join(tasks, task, "tests/test.sh")
            if not os.path.isfile(test):
                continue
            with open(test) as f:
                script = f.read()
            with open(os.path.join(tasks, task, "checks/ladder.json")) as f:
                config = json.load(f)
            grading = ladder.probe_grading(task, config)
            self.assertEqual(
                dict(grading["flags"]),
                dict(ladder.verifier_flags(script, ladder.VERIFIER_FLAGS)), task)
            self.assertEqual("informed" in grading, "--informed" in script, task)


TWIN = ladder.TWIN


def zoo_config(zoo=None, **more):
    config = {"rungs": [{"name": "nop", "dir": "bots/starter"},
                        {"name": "reference", "dir": "r"},
                        {"name": "told", "dir": "t", "tell_regime": True}],
              "gates": {"informed": ["told"], "non_reading": ["nop"]}, **more}
    if zoo is not None:
        config["zoo"] = zoo
    return config


class StrategyZoo(unittest.TestCase):
    def test_the_zoo_lists_the_crates_rungs(self):
        with open(os.path.join(HERE, "../zoo/src/lib.rs")) as f:
            names = re.findall(r'Self::\w+ => "([a-z-]+)",', f.read())
        self.assertEqual(names, ladder.ZOO)

    def test_a_ladder_without_a_zoo_section_runs_every_rung_enforced(self):
        zoo = ladder.zoo_section(zoo_config())
        self.assertEqual(zoo["rungs"], ladder.ZOO)
        self.assertTrue(zoo["enforce"])
        self.assertFalse(zoo["declared"])
        self.assertEqual(zoo["informed"], ["told"])
        rungs = ladder.zoo_rungs(zoo)
        self.assertEqual(rungs[0], {"name": "zoo-twap", "dir": "bench/checks/zoo",
                                    "env": {"MTX_ZOO": "twap"}, "may_breach": True, "zoo": True})

    def test_a_v2_task_compares_with_its_informed_anchor(self):
        config = zoo_config(anchors={"reference": "reference", "informed": "told"},
                            gates={"informed": ["told", "nop"], "non_reading": []})
        self.assertEqual(ladder.zoo_section(config)["informed"], ["told"])

    def test_skips_and_lists_narrow_the_zoo_and_each_skip_says_why(self):
        zoo = ladder.zoo_section(zoo_config({"rungs": ["twap", "dump", "fade"],
                                             "skip": {"dump": "no job"}}))
        self.assertEqual(zoo["rungs"], ["twap", "fade"])
        for bad in ({"rungs": ["ghost"]}, {"rungs": "some"}, {"skip": {"dump": " "}},
                    {"skip": {"ghost": "x"}}, {"enforce": False}, {"enforce": "no"},
                    {"hybrid": "h", "hybrid_exempt": "x"}, {"hybrid_exempt": ""},
                    {"informed": "ghost"}, {"loosen": True}, {"rungs": ["dump", "dump"]}):
            with self.assertRaises(SystemExit, msg=bad):
                ladder.zoo_section(zoo_config(bad))
        clash = zoo_config()
        clash["rungs"].append({"name": "zoo-dump", "dir": "x"})
        with self.assertRaises(SystemExit):
            ladder.zoo_section(clash)
        zoo = ladder.zoo_section(zoo_config({"enforce": False, "why": "an existing task"}))
        self.assertFalse(zoo["enforce"])

    def test_the_hybrid_runs_the_best_zoo_rung_told_what_the_informed_rung_is(self):
        config = zoo_config({"hybrid": "h"})
        zoo = ladder.zoo_section(config)
        config["rungs"] += ladder.zoo_rungs(zoo)
        config["rungs"].append({"name": "mine", "dir": "m", "zoo": True, "arm": "my-arm"})
        config["rungs"].append({"name": "other", "dir": "o", "zoo": True})
        reports = {"reference": report([1.0]), "zoo-dump": report([3.0]),
                   "zoo-fade": report([-2.0]), "mine": report([5.0]), "other": report([9.0])}
        self.assertEqual(ladder.hybrid_arm(config, reports), "my-arm")
        del reports["mine"]
        self.assertEqual(ladder.hybrid_arm(config, reports), "dump")
        rung = ladder.hybrid_rung(config, zoo, "dump")
        self.assertEqual(rung, {"name": "zoo-hybrid", "dir": "h", "env": {"MTX_ZOO": "dump"},
                                "may_breach": True, "tell_regime": True})


def zoo_sets(rungs, sets=1):
    return [{name: report(pnl, breached) for name, (pnl, breached) in rungs.items()}
            for _ in range(sets)]


def zoo_verdicts(rungs, members, min_odds=0.85, hybrid=False, sets=1):
    zoo = ladder.zoo_section(zoo_config({"hybrid": "h"} if hybrid else {}))
    checks = ladder.zoo_checks(zoo_sets(rungs, sets), zoo, members, min_odds)
    return {c["id"]: c for c in checks}


class ZooRules(unittest.TestCase):
    ref = [10.0, 12.0, 8.0, 11.0, 9.0, 10.0, 12.0, 8.0, 11.0, 9.0, 10.0, 10.0]
    told = [40.0, 38.0, 42.0, 41.0, 39.0, 40.0, 38.0, 42.0, 41.0, 39.0, 40.0, 40.0]

    def rungs(self, **more):
        return {"reference": (self.ref, ()), "told": (self.told, ()), **more}

    def test_a_weak_zoo_rung_passes_both_rules(self):
        v = zoo_verdicts(self.rungs(weak=([1.0, -1.0] * 6, ())), ["weak"])
        self.assertEqual(v["below_reference:weak"]["verdict"], "pass")
        self.assertEqual(v["far_from_informed:weak"]["verdict"], "pass")

    def test_a_shortcut_over_the_reference_fails_with_confidence(self):
        v = zoo_verdicts(self.rungs(short=([x + 5 for x in self.ref], ())), ["short"])
        self.assertEqual(v["below_reference:short"]["verdict"], "fail")
        self.assertTrue(v["below_reference:short"]["point_violated"])
        self.assertEqual(v["far_from_informed:short"]["verdict"], "pass")

    def test_a_rung_near_the_reference_is_inconclusive_with_the_point_violated(self):
        near = [x + (3.0 if i % 2 else -2.5) for i, x in enumerate(self.ref)]
        v = zoo_verdicts(self.rungs(near=(near, ())), ["near"])
        self.assertEqual(v["below_reference:near"]["verdict"], "inconclusive")
        self.assertTrue(v["below_reference:near"]["point_violated"])

    def test_a_rung_near_the_informed_bot_fails_the_signal_bar(self):
        v = zoo_verdicts(self.rungs(close=([30.0] * 12, ())), ["close"])
        self.assertEqual(v["far_from_informed:close"]["verdict"], "fail")

    def test_a_v1_rung_with_a_breach_totals_0(self):
        v = zoo_verdicts(self.rungs(breach=([50.0] * 12, (3,))), ["breach"])
        self.assertEqual(v["below_reference:breach"]["verdict"], "pass")
        self.assertEqual(v["below_reference:breach"]["total"], 0.0)

    def test_a_hybrid_that_beats_the_informed_bot_past_noise_fails_the_anchor(self):
        better = [x + 4 for x in self.told]
        v = zoo_verdicts(self.rungs(**{"zoo-hybrid": (better, ())}), [], hybrid=True)
        self.assertEqual(v["anchor_strength"]["verdict"], "fail")

    def test_a_hybrid_within_noise_is_inconclusive_and_one_below_passes(self):
        noisy = [x + (6.0 if i % 2 else -5.5) for i, x in enumerate(self.told)]
        v = zoo_verdicts(self.rungs(**{"zoo-hybrid": (noisy, ())}), [], hybrid=True)
        self.assertEqual(v["anchor_strength"]["verdict"], "inconclusive")
        self.assertFalse(v["anchor_strength"]["point_violated"])
        worse = [x - 1 for x in self.told]
        v = zoo_verdicts(self.rungs(**{"zoo-hybrid": (worse, ())}), [], hybrid=True)
        self.assertEqual(v["anchor_strength"]["verdict"], "pass")

    def test_a_hybrid_that_did_what_the_informed_bot_did_has_no_power(self):
        v = zoo_verdicts(self.rungs(**{"zoo-hybrid": (list(self.told), ())}), [], hybrid=True)
        self.assertEqual(v["anchor_strength"]["verdict"], "no_power")

    def test_without_an_informed_rung_only_the_reference_rule_runs(self):
        zoo = ladder.zoo_section(zoo_config(gates={}))
        checks = ladder.zoo_checks(zoo_sets({"reference": (self.ref, ()), "z": ([0.0] * 12, ())}),
                                   zoo, ["z"], 0.85)
        self.assertEqual([c["id"] for c in checks], ["below_reference:z"])

    def test_every_set_counts(self):
        v = zoo_verdicts(self.rungs(weak=([1.0] * 12, ())), ["weak"], sets=3)
        self.assertEqual(v["below_reference:weak"]["sets"], 3)


if __name__ == "__main__":
    unittest.main()

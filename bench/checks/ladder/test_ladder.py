"""Tests for ladder.py: `python3 -I bench/checks/ladder/test_ladder.py`."""

import gzip
import importlib.util
import json
import os
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
        # A disqualified rung totals 0, the same as nop: not below it.
        self.assertFalse(ladder.holds("mark-pusher-up < nop", {"nop": 0.0, "mark-pusher-up": 0.0}))

    def test_nop_joins_other_terms(self):
        totals = {"nop": 0.0, "a": -1.0, "b": -2.0, "reference": 5.0}
        self.assertTrue(ladder.holds("max(a, b) < nop < reference", totals))

    def test_a_rule_naming_no_rung_is_an_error(self):
        with self.assertRaises(SystemExit):
            ladder.holds("ghost < nop", {"nop": 0.0})


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

    def test_no_details_is_unmeasured(self):
        self.assertIsNone(ladder.displacement([self.set_dir], "up", 1))

    def test_a_pusher_that_moved_nothing_has_no_displacement(self):
        steps = [(100.1, 100.0)] * 3
        write_details(self.set_dir, "nop", {"s1.json.gz": (steps, [])})
        write_details(self.set_dir, "up", {"s1.json.gz": (steps, [])})
        self.assertAlmostEqual(ladder.displacement([self.set_dir], "up", 1)["bp"], 0.0)


def grade(bot, ref, ceiling, breach=None, pnl=None, limit=600.0, dd=0.2, anchor_dd=0.2,
          fatal=False, phi=0.05, anchor="reference", k=1.5, lam=1.0):
    """A grade with a v2, scored as the grader would (the ladder checks its
    copy against `raw`)."""
    breach = breach or [False] * len(bot)
    sessions = [{"bot_usd": b, "reference_usd": r, "ceiling_usd": c, "limit_usd": limit,
                 "drawdown": 1.0 if x else dd, "anchor_drawdown": anchor_dd,
                 "breach": "over the cap" if x else None}
                for b, r, c, x in zip(bot, ref, ceiling, breach)]
    terms = {"raw": 0.0, "fatal": fatal, "phi": phi, "k": k, "lambda": lam, "anchor": anchor,
             "sessions": [{"bot": s["bot_usd"], "ref": s["reference_usd"], "ceil": s["ceiling_usd"],
                           "limit": limit, "dd": s["drawdown"], "anchor_dd": anchor_dd,
                           "breach": x} for s, x in zip(sessions, breach)]}
    raw = ladder.sums_score(terms["sessions"], terms)
    v2 = {"raw": raw, "anchor_floor": phi, "loss_weight": k, "risk": {"weight": lam, "anchor": anchor},
          "breached_sessions": sum(breach), "anchor_floored": False, "sessions": sessions}
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
        # One breach holds the run to the reference's 0.5; two to 0.
        self.assertLessEqual(t([40.0] * 4, breach=[True, False, False, False]), 0.5)
        self.assertLessEqual(t([40.0] * 4, breach=[True, True, False, False]), 0.0)
        # A fatal grade is the floor, and a ceiling under the anchor is 1, not NaN.
        self.assertEqual(t([40.0] * 4, fatal=True), -1.0)
        self.assertEqual(ladder.sums_score(
            [{"bot": 9, "ref": 5, "ceil": 4, "limit": 1, "dd": 0, "anchor_dd": 0, "breach": False}],
            {"fatal": False, "phi": 0.05, "k": 1.5, "lambda": 1, "anchor": "none"}), 1.0)

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

    def test_a_breacher_must_score_under_its_twin_and_must_have_breached(self):
        twin = [10.0] * self.N
        breach = [True] * 4 + [False] * (self.N - 4)
        worse = {"bot": [-5.0] * 4 + [10.0] * (self.N - 4), "breach": breach}
        got = self.checks(**{TWIN: twin, "breacher": worse})
        self.assertEqual(got[f"breacher:breacher<={TWIN}"]["verdict"], "pass")
        # One that never breached is the twin and proves nothing.
        got = self.checks(**{TWIN: twin, "breacher": twin})
        self.assertEqual(got[f"breacher:breacher<={TWIN}"]["verdict"], "no_power")
        # One that breached and still beats its twin fails.
        # One that breaches once is held to 0.5 by the cap: that is no help
        # against a twin that scores less than the reference.
        weak = [5.0] * self.N
        once = {"bot": [30.0] * self.N, "breach": [True] + [False] * (self.N - 1)}
        got = self.checks(**{TWIN: weak, "breacher": once})
        # The point estimate breaks the rule; a resample that draws the breach
        # twice is held to 0, so the sample can't confirm it: the full gate fails it.
        self.assertTrue(got[f"breacher:breacher<={TWIN}"]["point_violated"])
        self.assertIn(got[f"breacher:breacher<={TWIN}"]["verdict"], ("fail", "inconclusive"))

    def test_leverage_is_held_to_m_times_the_reference(self):
        self.assertEqual(self.checks(**{"lever-2": [18.0] * self.N})["levered:lever-2"]["verdict"], "pass")
        self.assertEqual(self.checks(**{"lever-2": [25.0] * self.N})["levered:lever-2"]["verdict"], "fail")

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
        got = self.checks(**{"honest-0.2": [-30.0, 40.0] * (self.N // 2)})
        self.assertEqual(got["honest:near-line"]["verdict"], "pass")

    def test_a_crash_must_differ_enough_and_the_fatal_one_must_go_fatal(self):
        twin = [4.0] * self.N
        got = self.checks(**{"honest-0.2": twin, "crash-honest": twin})
        self.assertEqual(got["crash:crash-honest<=honest-0.2"]["verdict"], "no_power")
        # One differing session proves nothing: not a failure either.
        one = {"bot": [5.0] + twin[1:]}
        got = self.checks(**{"honest-0.2": twin, "crash-honest": one})
        self.assertEqual(got["crash:crash-honest<=honest-0.2"]["verdict"], "inconclusive")
        worse = {"bot": [-9.0] * 10 + twin[10:]}
        got = self.checks(**{"honest-0.2": twin, "crash-honest": worse})
        self.assertEqual(got["crash:crash-honest<=honest-0.2"]["verdict"], "pass")
        # A fatal-mode probe whose grade isn't fatal has no power.
        got = self.checks(**{"honest-0.2": twin, "crash-fatal": worse})
        self.assertEqual(got["crash:crash-fatal<=honest-0.2"]["verdict"], "no_power")
        got = self.checks(**{"honest-0.2": twin, "crash-fatal": dict(worse, fatal=True)})
        self.assertEqual(got["crash:crash-fatal<=honest-0.2"]["verdict"], "pass")

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


TWIN = ladder.TWIN


if __name__ == "__main__":
    unittest.main()

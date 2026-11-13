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


def v2_report(bot, ref, ceiling, pnl=None, breached=0, floor=0.1, fatal=False, raw=None):
    """A grade with a v2: per-session weighted terms, as the grader writes them."""
    sessions = [{"bot_usd": b, "reference_usd": r, "ceiling_usd": c}
                for b, r, c in zip(bot, ref, ceiling)]
    terms = {"raw": 0.0 if raw is None else raw, "anchor_floor": floor,
             "breached_sessions": breached, "sessions": sessions}
    if fatal:
        terms["fatal"] = "refused"
    return {"v2": terms, "scenarios": [{"pnl_usd": p} for p in (pnl or bot)]}


class ProbeGate(unittest.TestCase):
    N = 40

    def ladder(self, **bots):
        """One set of N sessions: the reference earns 10 each, the ceiling 40;
        `bots` maps a rung to its per-session weighted PnL."""
        ref = [10.0] * self.N
        ceiling = [40.0] * self.N
        reports = {"reference": v2_report(ref, ref, ceiling)}
        for name, spec in bots.items():
            if isinstance(spec, dict):
                bot = spec.pop("bot")
                reports[name] = v2_report(bot, ref, ceiling, **spec)
            else:
                reports[name] = v2_report(spec, ref, ceiling)
        return [reports]

    def test_raw_is_half_at_the_reference_and_one_at_the_ceiling(self):
        terms = ladder.v2_terms(v2_report([10.0] * 4, [10.0] * 4, [40.0] * 4))
        self.assertAlmostEqual(ladder.v2_raw(terms), 0.5)
        terms = ladder.v2_terms(v2_report([40.0] * 4, [10.0] * 4, [40.0] * 4))
        self.assertAlmostEqual(ladder.v2_raw(terms), 1.0)
        terms = ladder.v2_terms(v2_report([-5.0] * 4, [10.0] * 4, [40.0] * 4))
        self.assertAlmostEqual(ladder.v2_raw(terms), -0.25)

    def test_a_weak_reference_is_floored(self):
        terms = ladder.v2_terms(v2_report([2.0] * 4, [1.0] * 4, [40.0] * 4))
        # The floor is 10% of 160 = 16 against a measured 4.
        self.assertAlmostEqual(ladder.v2_raw(terms), 0.5 * 8 / 16)

    def test_a_fatal_grade_keeps_the_raw_it_was_given(self):
        terms = ladder.v2_terms(v2_report([30.0] * 4, [10.0] * 4, [40.0] * 4, fatal=True, raw=-1.0))
        self.assertEqual(ladder.v2_raw(terms, [0, 1]), -1.0)

    def test_a_gambler_that_loses_passes_and_one_that_wins_by_a_lot_fails(self):
        sets = self.ladder(loser=[-3.0, 1.0] * (self.N // 2), lucky=[8.0, -1.0] * (self.N // 2))
        got = {c["id"]: c for c in ladder.probe_checks({"zero_edge": ["loser", "lucky"]}, sets, self.N)}
        self.assertEqual(got["zero_edge:loser"]["verdict"], "pass")
        self.assertEqual(got["zero_edge:lucky"]["verdict"], "fail")

    def test_a_violation_the_sample_cannot_confirm_is_inconclusive(self):
        # Mean 0.5 a session against a spread of 40: raw is barely over the limit.
        bot = [40.0, -39.0] * (self.N // 2)
        sets = self.ladder(noisy=bot)
        got = ladder.probe_checks({"zero_edge": ["noisy"]}, sets, self.N)
        self.assertEqual(got[0]["verdict"], "inconclusive")

    def test_a_breacher_must_score_under_its_twin_and_must_have_breached(self):
        twin = [10.0] * self.N
        worse = {"bot": [10.0] * (self.N - 4) + [-30.0] * 4, "breached": 4}
        probes = {"pairs": [["breacher", "reference"]], "breaches": ["breacher"]}
        got = ladder.probe_checks(probes, self.ladder(breacher=worse), self.N)
        self.assertEqual(got[0]["verdict"], "pass")
        # One that never breached, and so equals its twin, proves nothing.
        got = ladder.probe_checks(probes, self.ladder(breacher={"bot": twin, "breached": 0}), self.N)
        self.assertEqual(got[0]["verdict"], "no_power")
        # One that breached and still beats its twin fails.
        better = {"bot": [20.0] * self.N, "breached": 2}
        got = ladder.probe_checks(probes, self.ladder(breacher=better), self.N)
        self.assertEqual(got[0]["verdict"], "fail")

    def test_leverage_is_held_to_m_times_the_base_and_never_pays_for_a_breach(self):
        probes = {"levered": [{"rung": "x2", "m": 2, "base": "reference"}]}
        ok = ladder.probe_checks(probes, self.ladder(x2=[18.0] * self.N), self.N)
        self.assertEqual(ok[0]["verdict"], "pass")
        too_much = ladder.probe_checks(probes, self.ladder(x2=[25.0] * self.N), self.N)
        self.assertEqual(too_much[0]["verdict"], "fail")
        pays = {"bot": [16.0] * (self.N - 1) + [12.0], "breached": 1, "pnl": [16.0] * self.N}
        # Beats the reference (16 > 10) despite a breach.
        paying = ladder.probe_checks(probes, self.ladder(x2=pays), self.N)
        self.assertEqual(paying[0]["verdict"], "fail")

    def test_an_honest_bot_over_the_pay_line_must_beat_doing_nothing(self):
        probes = {"honest": [{"rung": "h1", "sharpe": 0.1}, {"rung": "h3", "sharpe": 0.3}]}
        sets = self.ladder(h1=[-5.0] * self.N, h3=[-5.0] * self.N)
        got = {c["id"]: c for c in ladder.probe_checks(probes, sets, self.N)}
        self.assertEqual(got["honest:h1"]["verdict"], "pass")
        self.assertEqual(got["honest:h3"]["verdict"], "fail")

    def test_a_probe_rung_missing_from_the_ladder_is_an_error(self):
        with self.assertRaises(SystemExit):
            ladder.probe_checks({"zero_edge": ["ghost"]}, self.ladder(), self.N)

    def test_a_probe_rung_knows_its_sessions(self):
        with tempfile.TemporaryDirectory() as tmp:
            rung = {"name": "b", "dir": "bench/checks/probes", "env": {"MTX_PROBE": "coin-flip"}}
            os.makedirs(os.path.join(ladder.ROOT, rung["dir"]), exist_ok=True)
            path = os.path.join(ladder.rung_bot(rung, tmp, 72), "run.sh")
            with open(path) as f:
                self.assertIn("MTX_PROBE_SESSIONS=72", f.read())


if __name__ == "__main__":
    unittest.main()

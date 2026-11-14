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


class ScoringV2(unittest.TestCase):
    @staticmethod
    def report(pnls, drawdowns=None):
        drawdowns = drawdowns or [0.0] * len(pnls)
        return {"scenarios": [
            {"scenario": f"hidden-{i + 1}", "pnl_usd": p, "max_drawdown_usd": d}
            for i, (p, d) in enumerate(zip(pnls, drawdowns))
        ]}

    def test_the_anchors_are_the_reference_and_informed_sessions_weighted_as_v2_weighs(self):
        reports = {"reference": self.report([40.0, -20.0], [5.0, 30.0]),
                   "informed": self.report([100.0, -10.0], [8.0, 12.004])}
        anchors, e_ref, e_inf = ladder.v2_anchors(reports, "informed")
        self.assertEqual(anchors["hidden-2"], {
            "reference_pnl_usd": -20.0, "reference_max_drawdown_usd": 30.0,
            "informed_pnl_usd": -10.0, "informed_max_drawdown_usd": 12.0,
        })
        # Losses count 1.5 times.
        self.assertEqual(e_ref, 40.0 - 30.0)
        self.assertEqual(e_inf, 100.0 - 15.0)

    def test_order_rules_drop_the_hindsight_bound(self):
        self.assertEqual(ladder.v2_rule("reference < informed < hindsight"), "reference < informed")
        self.assertEqual(ladder.v2_rule("dip-buyer < reference"), "dip-buyer < reference")
        self.assertIsNone(ladder.v2_rule("informed < hindsight"))

    def check(self, raw, e_ref=100.0, e_inf=250.0, floored=False):
        config = {"rungs": [{"name": n} for n in raw],
                  "rules": ["dip < reference < informed < hindsight"]}
        reports = {n: {"v2": {"raw": r, "anchor_floored": floored}} for n, r in raw.items()}
        failed = []
        summary = ladder.check_v2(config, [{"e_ref": e_ref, "e_inf": e_inf, "reports": reports}],
                                  "informed", ladder.V2_BAND, failed)
        return summary, failed

    def test_anchors_band_and_order_that_hold_pass(self):
        summary, failed = self.check({"nop": 0.0, "dip": 0.2, "reference": 0.5, "informed": 1.0})
        self.assertEqual(failed, [])
        self.assertEqual(summary["sets"][0]["reference_share"], 0.4)
        self.assertEqual(summary["rules"], [{"rule": "dip < reference < informed", "ok": True,
                                             "sets_holding": 1, "sets": 1}])

    def test_a_reference_outside_its_band_fails(self):
        _, failed = self.check({"nop": 0.0, "dip": 0.2, "reference": 0.5, "informed": 1.0},
                               e_ref=200.0)
        self.assertTrue(any("band" in f for f in failed), failed)

    def test_an_informed_bot_that_doesnt_beat_the_reference_fails(self):
        _, failed = self.check({}, e_ref=300.0)
        self.assertTrue(any("refuses" in f for f in failed), failed)

    def test_an_anchor_off_its_point_or_a_broken_order_fails(self):
        _, failed = self.check({"nop": 0.0, "dip": 0.6, "reference": 0.5, "informed": 0.9})
        self.assertTrue(any("informed scores 0.900" in f for f in failed), failed)
        self.assertTrue(any("v2 order" in f for f in failed), failed)


if __name__ == "__main__":
    unittest.main()

"""Tests for ladder.py: `python3 -I bench/checks/ladder/test_ladder.py`."""

import gzip
import importlib.util
import json
import os
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
             "sessions": [{"bot": s["bot_usd"], "ref": s["reference_usd"], "ceil": s["ceiling_usd"],
                           "limit": limit, "dd": s["drawdown"], "anchor_dd": anchor_dd,
                           "breach": x} for s, x in zip(sessions, breach)]}
    raw = ladder.sums_score(terms["sessions"], terms)
    v2 = {"raw": raw, "anchor_floor": phi, "loss_weight": k, "break_even_sharpe": 0.16, "risk": {"weight": lam, "anchor": anchor},
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
    def breach_ladder(self, floor=True, bot_gain=100.0, breaches=6, free=300.0, twin=None):
        """Twin (the clipped lever) makes 10 a session; the breacher breaches
        in `breaches` sessions, where the session itself made `free` (its PnL):
        the grader floors it to -k * limit (`floor`) or, without the floor,
        scores it as it made; `bot_gain` in the rest. Built through `grade`,
        so it is scored as the ladder scores."""
        breach = [True] * breaches + [False] * (self.N - breaches)
        pnl = [free] * breaches + [bot_gain] * (self.N - breaches)
        scored = [weighted_loss(free) if not floor else min(-1.5 * 600.0, weighted_loss(free))
                  ] * breaches + [bot_gain] * (self.N - breaches)
        return self.ladder(**{TWIN: twin or [10.0] * self.N,
                              "breacher": {"bot": scored, "pnl": pnl, "breach": breach}})

    def breacher(self, sets):
        got = {c["id"]: c for c in ladder.probe_checks({}, sets, self.N)}
        return {k: v for k, v in got.items() if k.startswith("breacher:")}

    def test_a_breacher_with_its_floor_and_cap_passes_every_gate_at_every_b(self):
        got = self.breacher(self.breach_ladder())
        self.assertEqual(sorted(got), sorted(
            f"breacher:b{b}:{g}" for b in (1, 2, 3) for g in ("run_cap", "floor")))
        for check, c in got.items():
            self.assertEqual(c["verdict"], "pass", (check, c["detail"]))
        # b = 1 is a test: one breach session is composed, not two.
        self.assertEqual(got["breacher:b1:floor"]["breach_sessions"], 1)

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

    def test_a_check_that_does_not_bind_is_no_power_never_a_pass(self):
        """Flash-crash's shape: it breaches in nearly every session and the
        one clean session is flat, so even uncapped the run can't beat its twin
        and the floor only takes back a loss: neither rule is exercised."""
        got = self.breacher(self.breach_ladder(breaches=11, bot_gain=0.0, free=-700.0))
        for b in (1, 2, 3):
            self.assertEqual(got[f"breacher:b{b}:run_cap"]["verdict"], "no_power", b)
            self.assertIn("not exercised", got[f"breacher:b{b}:run_cap"]["detail"])
            self.assertEqual(got[f"breacher:b{b}:floor"]["verdict"], "no_power", b)
            self.assertIn("not exercised", got[f"breacher:b{b}:floor"]["detail"])

    def test_the_floor_is_tested_on_the_sessions_where_it_binds(self):
        """Sessions that lost more than the floor are not where it binds: the
        composition picks the ones where breaching paid over the twin."""
        sets = self.breach_ladder(breaches=6)
        for i in (0, 1, 2):
            row = sets[0]["breacher"]["v2"]["sessions"][i]
            row["bot_usd"] = -1500.0
            sets[0]["breacher"]["scenarios"][i]["pnl_usd"] = -1000.0
        got = self.breacher(sets)
        for b in (1, 2, 3):
            self.assertEqual(got[f"breacher:b{b}:floor"]["verdict"], "pass", b)

    def test_removing_the_per_session_floor_fails_the_gate(self):
        """The grader that no longer floors a breach session at -k * limit
        scores it as the session made: the swap run beats its twin."""
        got = self.breacher(self.breach_ladder(floor=False))
        for b in (1, 2, 3):
            self.assertEqual(got[f"breacher:b{b}:floor"]["verdict"], "fail", b)

    def test_the_run_cap_check_fails_a_breacher_the_cap_does_not_hold(self):
        """The ladder's copy of the scorer is checked against the grader on
        every grade, and the grader's own cap against its own tests
        (scoring_v2.rs, the_run_cap_holds_a_bot_that_levers_everywhere). Here:
        a grade that gains where the cap binds, scored without it, fails."""
        got = self.breacher(self.breach_ladder())
        for b in (1, 2, 3):
            self.assertEqual(got[f"breacher:b{b}:run_cap"]["verdict"], "pass", b)
            self.assertGreater(got[f"breacher:b{b}:run_cap"]["raw_without_cap"],
                               got[f"breacher:b{b}:run_cap"]["other_raw"])
        original = ladder.sums_score
        ladder.sums_score = lambda rows, terms, capped=True: original(rows, terms, capped=False)
        try:
            got = self.breacher(self.breach_ladder())
        finally:
            ladder.sums_score = original
        for b in (1, 2, 3):
            self.assertEqual(got[f"breacher:b{b}:run_cap"]["verdict"], "fail", b)

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


TWIN = ladder.TWIN


if __name__ == "__main__":
    unittest.main()

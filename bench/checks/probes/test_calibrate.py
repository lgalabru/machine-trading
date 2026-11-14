"""Tests for calibrate.py: `python3 -I bench/checks/probes/test_calibrate.py`."""

import importlib.util
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("calibrate", os.path.join(HERE, "calibrate.py"))
calibrate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(calibrate)


class Blend(unittest.TestCase):
    ref = (60.0, 60.0**2 + 120.0**2)
    gamble = (0.0, 150.0**2)

    def test_the_ends_are_the_two_bots(self):
        self.assertAlmostEqual(calibrate.sharpe(1.0, self.ref, self.gamble), 0.5)
        self.assertAlmostEqual(calibrate.sharpe(0.0, self.ref, self.gamble), 0.0)

    def test_the_blend_hits_its_target(self):
        for target in (0.1, 0.2, 0.3, 0.4):
            q = calibrate.blend_for(target, self.ref, self.gamble)
            self.assertAlmostEqual(calibrate.sharpe(q, self.ref, self.gamble), target, places=6)

    def test_a_target_over_the_references_sharpe_is_unreachable(self):
        self.assertIsNone(calibrate.blend_for(0.6, self.ref, self.gamble))


if __name__ == "__main__":
    unittest.main()

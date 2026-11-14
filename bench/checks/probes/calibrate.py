"""Pick the `MTX_PROBE_BLEND` that gives the honest probe a target per-session
Sharpe on a task, from a finished ladder's reports.

An honest probe follows the task's reference in a share `q` of sessions (a
seeded draw per session) and holds a coin-flip at the full cap in the rest.
Sessions are independent draws, so the probe's session PnL is a mixture: its
mean is q·m_r + (1−q)·m_g and its second moment q·s_r + (1−q)·s_g, where
(m, s) are the mean and the second moment of the reference's and of the
coin-flip's session PnL. Its per-session Sharpe, mean over standard
deviation, is a function of `q` alone; this solves it for each target by
bisection (it rises with `q` while the reference has the edge).

Usage: python3 -I calibrate.py <ladder out>/<task> [0.1 0.2 0.3 0.5]

It reads `set-*/reference.json` and `set-*/coin-flip-full.json`: run the
ladder with both rungs first. The blends belong in the task's ladder file; the
ladder reports the Sharpe each honest rung actually achieved, so a blend that
drifts shows there.
"""

import glob
import json
import math
import os
import sys


def session_pnls(root, rung):
    out = []
    for path in sorted(glob.glob(os.path.join(root, "set-*", f"{rung}.json"))):
        with open(path) as f:
            out += [s["pnl_usd"] for s in json.load(f)["scenarios"]]
    if not out:
        raise SystemExit(f"no {rung}.json under {root}/set-*")
    return out


def moments(xs):
    n = len(xs)
    return sum(xs) / n, sum(x * x for x in xs) / n


def sharpe(q, ref, gamble):
    mean = q * ref[0] + (1 - q) * gamble[0]
    second = q * ref[1] + (1 - q) * gamble[1]
    var = second - mean * mean
    return mean / math.sqrt(var) if var > 0 else 0.0


def blend_for(target, ref, gamble):
    """The `q` in [0, 1] whose mixture has Sharpe `target`, or None when the
    reference's own Sharpe is under it."""
    if sharpe(1.0, ref, gamble) < target:
        return None
    lo, hi = 0.0, 1.0
    for _ in range(60):
        mid = (lo + hi) / 2
        if sharpe(mid, ref, gamble) < target:
            lo = mid
        else:
            hi = mid
    return (lo + hi) / 2


def main():
    root = sys.argv[1]
    targets = [float(t) for t in sys.argv[2:]] or [0.1, 0.2, 0.3, 0.5]
    ref = moments(session_pnls(root, "reference"))
    gamble = moments(session_pnls(root, "coin-flip-full"))
    print(f"reference: mean ${ref[0]:.1f}, Sharpe {sharpe(1.0, ref, gamble):.2f}; "
          f"coin-flip: mean ${gamble[0]:.1f}, Sharpe {sharpe(0.0, ref, gamble):.2f}")
    for target in targets:
        q = blend_for(target, ref, gamble)
        print(f"  Sharpe {target}: " + (f"MTX_PROBE_BLEND={q:.3f}" if q is not None
                                         else "unreachable: the reference is under it"))


if __name__ == "__main__":
    main()

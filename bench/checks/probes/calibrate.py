"""Pick the `MTX_PROBE_SIZE` and `MTX_PROBE_BLEND` that give the honest probe a
target per-session Sharpe at the reference's risk, from a finished ladder's
reports.

An honest probe follows the task's reference in a share `q` of sessions (a
seeded draw per session) and holds a coin flip, at a share `s` of the cap, in
the rest. `s` is sized to the reference's risk: the coin's session-PnL spread
(its standard deviation) is made the reference's, as TUNE2's C4 probe sizes
its noise. A full-cap coin gambles far past the reference's risk, so the
near-breach charge takes the honest probe's profit and it scores nothing
whatever its skill. The coin's session PnL scales with its size (a coin
at `s` is `s` times the full-cap one), so `s` = reference spread over
full-cap coin spread, at most 1.

Sessions are independent draws, so the probe's session PnL is a mixture: its
mean is q·m_r + (1−q)·s·m_g and its second moment q·v_r + (1−q)·s²·v_g, where
(m, v) are the mean and the second moment of the reference's and of the
full-cap coin-flip's session PnL. Its per-session Sharpe, mean over standard
deviation, is a function of `q` alone; this solves it for each target by
bisection (it rises with `q` while the reference has the edge).

Usage: python3 -I calibrate.py <ladder out>/<task> [0.1 0.2 0.3 0.5]

It reads `set-*/reference.json` and `set-*/coin-flip-full.json`: run the
ladder with both rungs first. Set both variables on each honest rung of the
task's ladder file. The ladder reports the Sharpe each honest rung actually
achieved, so a pair that drifts shows there.
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


def spread(moment):
    mean, second = moment
    return math.sqrt(max(second - mean * mean, 0.0))


def size_for(ref, gamble):
    """The coin's share of the cap that gives it the reference's session-PnL
    spread (at most the full cap)."""
    return min(1.0, spread(ref) / spread(gamble)) if spread(gamble) > 0 else 1.0


def scaled(gamble, size):
    """The moments of the coin at `size` times the full-cap one's."""
    return size * gamble[0], size * size * gamble[1]


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
    full = moments(session_pnls(root, "coin-flip-full"))
    size = size_for(ref, full)
    gamble = scaled(full, size)
    print(f"reference: mean ${ref[0]:.1f}, spread ${spread(ref):.1f}, "
          f"Sharpe {sharpe(1.0, ref, gamble):.2f}; full-cap coin-flip: spread ${spread(full):.1f}; "
          f"coin at the reference's spread: size {size:.3f}")
    for target in targets:
        q = blend_for(target, ref, gamble)
        print(f"  Sharpe {target}: " + (f"MTX_PROBE_SIZE={size:.3f} MTX_PROBE_BLEND={q:.3f}"
                                         if q is not None
                                         else "unreachable: the reference is under it"))


if __name__ == "__main__":
    main()

"""Pick the `MTX_PROBE_SIZE` and `MTX_PROBE_BLEND` that give the honest probe a
target per-session Sharpe at the reference's risk, from a finished ladder's
reports.

An honest probe follows the task's reference in a share `q` of sessions (a
seeded draw per session) and holds a coin flip, at a share `s` of the cap, in
the rest. `s` is sized to the reference's risk, so the near-breach charge and
the breach rule test the probe's edge and not its gambling. A full-cap coin
gambles far past the reference: its drawdowns are the charge's to tax and some
of its sessions breach, so a probe built on it scores at or below nothing
whatever its edge. Two things are matched, and `s` is the smaller:

- the root mean square of the sessions' drawdowns, as a share of the
  drawdown limit (a breach counts 1): the quantity the charge reads, against
  the reference's;
- the standard deviation of the sessions' PnL (TUNE2's C4 sizing).

A coin's PnL and drawdowns scale with its size (a coin at `s` is `s` times the
full-cap one), and the share is at most 1.

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


def session_drawdowns(root, rung):
    """Each session's max drawdown as a share of the drawdown limit, at most 1
    (a breach counts 1, as the grader's near-breach charge reads it)."""
    out = []
    for path in sorted(glob.glob(os.path.join(root, "set-*", f"{rung}.json"))):
        with open(path) as f:
            for s in json.load(f)["scenarios"]:
                limit = s["mandate"]["limits"]["max_drawdown_usd"]
                out.append(min(s["max_drawdown_usd"] / limit, 1.0))
    if not out:
        raise SystemExit(f"no {rung}.json under {root}/set-*")
    return out


def rms(xs):
    return math.sqrt(sum(x * x for x in xs) / len(xs))


def moments(xs):
    n = len(xs)
    return sum(xs) / n, sum(x * x for x in xs) / n


def spread(moment):
    mean, second = moment
    return math.sqrt(max(second - mean * mean, 0.0))


def size_for(ref, gamble, ref_drawdown=None, gamble_drawdown=None):
    """The coin's share of the cap that gives it the reference's session-PnL
    spread and, given each one's drawdown rms, its drawdown rms (at most the
    full cap)."""
    size = min(1.0, spread(ref) / spread(gamble)) if spread(gamble) > 0 else 1.0
    if ref_drawdown is not None and gamble_drawdown:
        size = min(size, ref_drawdown / gamble_drawdown)
    return size


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
    dd_ref = rms(session_drawdowns(root, "reference"))
    dd_full = rms(session_drawdowns(root, "coin-flip-full"))
    size = size_for(ref, full, dd_ref, dd_full)
    gamble = scaled(full, size)
    print(f"reference: mean ${ref[0]:.1f}, spread ${spread(ref):.1f}, drawdown rms {dd_ref:.3f}, "
          f"Sharpe {sharpe(1.0, ref, gamble):.2f}; full-cap coin-flip: spread "
          f"${spread(full):.1f}, drawdown rms {dd_full:.3f}; coin at the reference's risk: "
          f"size {size:.3f}")
    for target in targets:
        q = blend_for(target, ref, gamble)
        print(f"  Sharpe {target}: " + (f"MTX_PROBE_SIZE={size:.3f} MTX_PROBE_BLEND={q:.3f}"
                                         if q is not None
                                         else "unreachable: the reference is under it"))


if __name__ == "__main__":
    main()

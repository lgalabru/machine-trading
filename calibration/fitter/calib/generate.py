"""Session generators for the D4 demo.

- `calibrated`: a minimal D2-style fair-value process read from a calibration
  file: two-state regime per session, GARCH(1,1) volatility on 10 s blocks,
  Student-t innovations, 1 s sub-steps. A sketch of what the engine's market
  model would do, not the engine.
- `engine_making_replica`: today's MakingFamily::spcx walk (Gaussian steps of
  U(0.02, 0.08)% every U(3, 9) slots, 25% calm / 50% jumps / 25% trend),
  sampled each second, to show what the checks catch in the current model.
"""

from __future__ import annotations

import math
import random

SLOT_S = 0.4


def _t(rng: random.Random, df: float) -> float:
    """Student-t scaled to unit variance."""
    if df >= 100:
        return rng.gauss(0, 1)
    z = rng.gauss(0, 1)
    chi = sum(rng.gauss(0, 1) ** 2 for _ in range(int(round(df))))
    return z / math.sqrt(chi / round(df)) * math.sqrt((df - 2) / df)


def calibrated(cal: dict, n_sessions: int, seconds: int, seed: int = 1) -> list[list[float]]:
    fv = cal["fair_value"]
    g = fv["vol_clustering"]["at_10s"]
    reg = fv["regimes"]
    df = max(4.5, fv["tails"]["student_t_df_10s"])
    a, b = g["alpha"], g["beta"]
    sd10 = fv["horizons_seconds"]["10s"]["sd"]
    rng = random.Random(seed)
    out = []
    for _ in range(n_sessions):
        stressed = rng.random() < reg["share"][1]
        mult = reg["stressed_vol_mult"] if stressed else reg["calm_vol_mult"]
        mult *= math.exp(rng.gauss(0, 0.3))  # within-regime dispersion
        var = (sd10 * mult) ** 2
        w = var * (1 - a - b)
        s2 = var * math.exp(rng.gauss(0, 0.5))  # start off the mean
        r1 = []
        for _ in range(seconds // 10):
            # The 10 s return is Student-t (the fitted horizon); its ten 1 s
            # pieces are a Gaussian bridge that sums to it.
            e = math.sqrt(s2) * _t(rng, df)
            z = [rng.gauss(0, 1) for _ in range(10)]
            zm = sum(z) / 10
            sub = [e / 10 + math.sqrt(s2 / 10) * (v - zm) for v in z]
            r1.extend(sub)
            s2 = w + a * e * e + b * s2
        out.append(r1)
    return out


def engine_making_replica(n_sessions: int, seconds: int, seed: int = 2) -> list[list[float]]:
    rng = random.Random(seed)
    out = []
    slots = int(seconds / SLOT_S)
    for _ in range(n_sessions):
        u = rng.random()
        regime = "calm" if u < 0.25 else ("jumps" if u < 0.75 else "trend")
        vol = rng.uniform(0.02, 0.08)
        drift = rng.choice([-1, 1]) * rng.uniform(0.006, 0.018) if regime == "trend" else 0.0
        jumps = {}
        if regime == "jumps":
            for _ in range(rng.randint(1, 3)):
                jumps[rng.randint(60, max(61, slots - 60))] = rng.choice([-1, 1]) * rng.uniform(1.5, 4.0)
        level = [0.0] * (slots + 1)
        cur, nxt = 0.0, rng.randint(3, 9)
        pending = sorted(jumps)
        for s in range(1, slots + 1):
            if s == nxt:
                if pending and pending[0] <= s:
                    cur += jumps[pending.pop(0)]
                else:
                    cur += drift + vol * rng.gauss(0, 1)
                nxt = s + rng.randint(3, 9)
            level[s] = cur
        px = [1 + level[min(slots, int(t / SLOT_S))] / 100 for t in range(seconds + 1)]
        out.append([math.log(b / a) for a, b in zip(px, px[1:])])
    return out


def real_windows(days_1s: list[list[dict]], seconds: int, n: int, seed: int = 3) -> list[list[float]]:
    rng = random.Random(seed)
    wins = []
    for d in days_1s:
        c = [k["c"] for k in d]
        for i in range(0, len(c) - seconds - 1, seconds):
            w = c[i:i + seconds + 1]
            wins.append([math.log(b / a) for a, b in zip(w, w[1:])])
    rng.shuffle(wins)
    return wins[:n]


def ticked(sessions: list[list[float]], p0: float, tick: float) -> list[list[float]]:
    """Round each session's price path to the venue tick (real 1 s returns
    are ticked: many zeros and bounce), so 1 s statistics compare fairly."""
    out = []
    for r in sessions:
        lp = math.log(p0)
        px = [round(p0 / tick) * tick]
        for v in r:
            lp += v
            px.append(round(math.exp(lp) / tick) * tick)
        out.append([math.log(b / a) for a, b in zip(px, px[1:])])
    return out

"""D4 stylized-fact checks: compare generated sessions with the real market
cut into session-length windows.

Rule: every statistic is computed the same way, on windows of the same
length, for the real data and for the sessions. Short-window estimators are
biased (kurtosis and ACFs especially), and comparing like with like cancels
that bias instead of pretending the session is a long sample. Bands are
ratios to the real value, wide enough that a fair generator passes and a
Gaussian random walk does not.
"""

from __future__ import annotations

import math
from statistics import fmean, median

from . import stats as S


def per_session(sessions: list[list[float]]) -> dict:
    """Stats of 1 s returns per session, summarized across sessions."""
    k1, k10, a1, a5, sd, lag1 = [], [], [], [], [], []
    pooled10 = []
    for r in sessions:
        if len(r) < 60:
            continue
        m = S.moments(r)
        sd.append(m["sd"])
        k1.append(m["excess_kurtosis"])
        r10 = S.aggregate(r, 10)
        pooled10.extend(r10)
        if len(r10) > 8:
            k10.append(S.moments(r10)["excess_kurtosis"])
        lag1.append(S.acf(r10, [1]).get(1, 0.0))
    norm10 = []
    for r in sessions:
        r10 = S.aggregate(r, 10)
        s = math.sqrt(fmean(v * v for v in r10)) if r10 else 0
        if s > 0:
            norm10.extend(v / s for v in r10)
    cl = clustering(sessions, bar=5, lags=(1, 3, 6))
    pm = S.moments(pooled10)
    nm = S.moments(norm10)
    return dict(
        sessions=len(sd),
        sd_1s_median=median(sd),
        sd_1s_cv=S.moments(sd)["sd"] / fmean(sd),
        kurt_1s_median=median(k1),
        kurt_10s_median=median(k10) if k10 else float("nan"),
        pooled_10s_kurtosis=pm["excess_kurtosis"],
        within_session_10s_kurtosis=nm["excess_kurtosis"],
        hill_alpha_pooled_10s=S.hill_tail_index(pooled10, 0.025)["alpha"],
        acf_abs_5s_lag1=cl[1],
        acf_abs_5s_lag3=cl[3],
        acf_abs_5s_lag6=cl[6],
        acf_10s_lag1_mean=fmean(lag1),
    )


def clustering(sessions: list[list[float]], bar: int, lags: tuple[int, ...]) -> dict[int, float]:
    """ACF of |r| at `bar`-second returns, each session scaled to unit RMS
    (so differences in volatility between sessions don't count as
    clustering) and lag pairs taken within a session only."""
    xs = []
    for r in sessions:
        rb = S.aggregate(r, bar)
        s = math.sqrt(fmean(v * v for v in rb)) if rb else 0
        if s > 0:
            xs.append([abs(v) / s for v in rb])
    flat = [v for x in xs for v in x]
    m = fmean(flat)
    c0 = fmean((v - m) ** 2 for v in flat)
    out = {}
    for L in lags:
        num = [(x[i] - m) * (x[i + L] - m) for x in xs for i in range(len(x) - L)]
        out[L] = fmean(num) / c0 if num and c0 else 0.0
    return out


CHECKS = [
    # (name, stat, kind, band). ratio: gen/real in band; ratio_pos: same, and gen > 0 if real <= 0;
    # diff: |gen - real| <= band (for stats near zero, where ratios are noise); absolute: |gen| <= band.
    # Bands are first guesses: set them from real-vs-real spread (other weeks) before gating.
    ("volatility level", "sd_1s_median", "ratio", (0.5, 2.0)),
    ("vol varies across sessions (vol of vol)", "sd_1s_cv", "ratio", (0.6, 1.6)),
    ("fat tails within a session (10 s, session-normalized)", "within_session_10s_kurtosis", "ratio_pos", (0.33, 3.0)),
    ("fat tails pooled (10 s)", "pooled_10s_kurtosis", "ratio_pos", (0.33, 3.0)),
    ("tail index pooled (10 s)", "hill_alpha_pooled_10s", "ratio", (0.67, 1.5)),
    ("volatility clustering within a session (|r| ACF 5 s, lag 5 s)", "acf_abs_5s_lag1", "diff", 0.04),
    ("volatility clustering within a session (|r| ACF 5 s, lag 15 s)", "acf_abs_5s_lag3", "diff", 0.04),
    ("volatility clustering within a session (|r| ACF 5 s, lag 30 s)", "acf_abs_5s_lag6", "diff", 0.04),
    ("no linear predictability (r ACF lag 1, 10 s)", "acf_10s_lag1_mean", "absolute", 0.1),
]


def compare(real: dict, gen: dict) -> list[dict]:
    out = []
    for name, key, kind, band in CHECKS:
        rv, gv = real[key], gen[key]
        if kind == "diff":
            ok = abs(gv - rv) <= band
            detail = f"|gen - real| = {abs(gv - rv):.3g} <= {band}"
        elif kind == "absolute":
            ok = abs(gv) <= band
            detail = f"|{gv:.3g}| <= {band}"
        else:
            if kind == "ratio_pos" and rv <= 0:
                ok, detail = gv > 0, "real <= 0; gen must be > 0"
            else:
                ratio = gv / rv if rv else float("inf")
                ok = band[0] <= ratio <= band[1]
                detail = f"gen/real = {ratio:.2f} in [{band[0]}, {band[1]}]"
        out.append(dict(check=name, stat=key, real=rv, generated=gv, ok=ok, detail=detail))
    return out


def check_round_trips(trips: list[dict], min_share_losing: float = 0.95) -> dict:
    """Round trips must lose spread plus impact. A trip is an immediate buy
    then sell (or sell then buy) of the same size by an uninformed taker in
    a generated session: {"size": q, "open_px": p0, "close_px": p1,
    "side": "buy"|"sell", "fees": f}. Passes when the mean trip loses and
    at least `min_share_losing` of them do (allowing a few lucky fills from
    the fair value moving between legs)."""
    pnl = []
    for t in trips:
        sgn = 1 if t["side"] == "buy" else -1
        pnl.append(sgn * (t["close_px"] - t["open_px"]) * t["size"] - t.get("fees", 0.0))
    losing = sum(1 for p in pnl if p < 0) / len(pnl)
    return dict(ok=fmean(pnl) < 0 and losing >= min_share_losing, mean_pnl=fmean(pnl), share_losing=losing, n=len(pnl))


def check_impact_curve(gamma: float, lo: float = 0.3, hi: float = 1.2) -> dict:
    """Cost grows with size and concavely-to-linearly (square-root law ~0.5)."""
    return dict(ok=lo <= gamma <= hi, gamma=gamma, band=[lo, hi])


def check_quantiles(sample: list[float], q_real: dict[str, float], tol: float = 0.5) -> dict:
    """Spread or depth distribution: each listed quantile of the sample within
    `tol` (relative) of the calibration's."""
    res = {}
    for k, v in q_real.items():
        q = float(k.lstrip("p")) / 100
        g = S.quantile(sample, q)
        res[k] = dict(real=v, generated=g, ok=abs(g - v) <= tol * abs(v) if v else abs(g) < 1e-9)
    return dict(ok=all(r["ok"] for r in res.values()), quantiles=res)

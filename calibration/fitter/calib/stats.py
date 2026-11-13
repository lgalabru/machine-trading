"""Return statistics and simple fits, standard library only.

Every estimator here is one a reviewer can check by hand: moments, Hill's
tail index, autocorrelations, a variance-targeted GARCH(1,1) by Nelder-Mead,
a two-state Gaussian mixture on log realized volatility, Roll's spread.
"""

from __future__ import annotations

import math
import random
from statistics import fmean, median


def log_returns(prices: list[float]) -> list[float]:
    return [math.log(b / a) for a, b in zip(prices, prices[1:]) if a > 0 and b > 0]


def moments(x: list[float]) -> dict:
    n = len(x)
    m = fmean(x)
    d = [v - m for v in x]
    v2 = sum(e * e for e in d) / n
    v3 = sum(e ** 3 for e in d) / n
    v4 = sum(e ** 4 for e in d) / n
    sd = math.sqrt(v2)
    return dict(n=n, mean=m, sd=sd, skew=v3 / sd ** 3 if sd else 0.0, excess_kurtosis=v4 / v2 ** 2 - 3 if v2 else 0.0)


def quantile(x: list[float], q: float) -> float:
    s = sorted(x)
    i = q * (len(s) - 1)
    lo = int(math.floor(i))
    hi = min(lo + 1, len(s) - 1)
    return s[lo] + (s[hi] - s[lo]) * (i - lo)


def hill_tail_index(x: list[float], tail_share: float = 0.025) -> dict:
    """Hill estimator on |x| over the top `tail_share`. alpha ~ 2-4 for liquid
    crypto at intraday horizons; a Gaussian has no finite alpha (estimates
    drift up with k). Reported with its asymptotic standard error alpha/sqrt(k)."""
    a = sorted((abs(v) for v in x if v != 0), reverse=True)
    k = max(10, int(tail_share * len(a)))
    if len(a) <= k + 1:
        return dict(alpha=float("nan"), k=k, se=float("nan"))
    xk = a[k]
    alpha = 1.0 / fmean(math.log(a[i] / xk) for i in range(k))
    return dict(alpha=alpha, k=k, se=alpha / math.sqrt(k))


def acf(x: list[float], lags: list[int]) -> dict[int, float]:
    n = len(x)
    m = fmean(x)
    d = [v - m for v in x]
    c0 = sum(e * e for e in d) / n
    out = {}
    for L in lags:
        if L >= n:
            continue
        out[L] = sum(d[i] * d[i + L] for i in range(n - L)) / n / c0 if c0 else 0.0
    return out


def aggregate(r: list[float], k: int) -> list[float]:
    return [sum(r[i:i + k]) for i in range(0, len(r) - k + 1, k)]


def roll_spread(prices: list[float]) -> float | None:
    """Roll (1984): effective spread = 2 sqrt(-cov(dp_t, dp_t-1)), as a share
    of price. None when the covariance is positive (no bounce to read)."""
    r = log_returns(prices)
    if len(r) < 10:
        return None
    m = fmean(r)
    cov = sum((r[i] - m) * (r[i - 1] - m) for i in range(1, len(r))) / (len(r) - 1)
    return 2 * math.sqrt(-cov) if cov < 0 else None


# --- GARCH(1,1), variance targeted -------------------------------------------


def _nelder_mead(f, x0, step=0.05, iters=400, tol=1e-9):
    n = len(x0)
    pts = [list(x0)] + [[x0[j] + (step if j == i else 0) for j in range(n)] for i in range(n)]
    vals = [f(p) for p in pts]
    for _ in range(iters):
        order = sorted(range(n + 1), key=lambda i: vals[i])
        pts = [pts[i] for i in order]
        vals = [vals[i] for i in order]
        if abs(vals[-1] - vals[0]) < tol:
            break
        c = [sum(p[j] for p in pts[:-1]) / n for j in range(n)]
        xr = [c[j] + (c[j] - pts[-1][j]) for j in range(n)]
        fr = f(xr)
        if fr < vals[0]:
            xe = [c[j] + 2 * (c[j] - pts[-1][j]) for j in range(n)]
            fe = f(xe)
            pts[-1], vals[-1] = (xe, fe) if fe < fr else (xr, fr)
        elif fr < vals[-2]:
            pts[-1], vals[-1] = xr, fr
        else:
            xc = [c[j] + 0.5 * (pts[-1][j] - c[j]) for j in range(n)]
            fc = f(xc)
            if fc < vals[-1]:
                pts[-1], vals[-1] = xc, fc
            else:
                for i in range(1, n + 1):
                    pts[i] = [pts[0][j] + 0.5 * (pts[i][j] - pts[0][j]) for j in range(n)]
                    vals[i] = f(pts[i])
    return pts[0], vals[0]


def garch11(r: list[float]) -> dict:
    """Gaussian QMLE of sigma2_t = w + a r_{t-1}^2 + b sigma2_{t-1}, with
    w = var (1 - a - b). Returns a, b, persistence a+b, the half-life of a
    volatility shock in bars, and the excess kurtosis of r/sigma (fat tails
    left after clustering: feeds the Student-t df)."""
    m = fmean(r)
    e = [v - m for v in r]
    var = sum(v * v for v in e) / len(e)

    def unpack(p):
        # a, b in (0,1) with a + b < 1 via a logistic map.
        s = 1 / (1 + math.exp(-p[0]))  # persistence
        f = 1 / (1 + math.exp(-p[1]))  # share of persistence that is a
        return s * f, s * (1 - f)

    def nll(p):
        a, b = unpack(p)
        w = var * (1 - a - b)
        s2 = var
        ll = 0.0
        for v in e:
            ll += math.log(s2) + v * v / s2
            s2 = w + a * v * v + b * s2
            if s2 <= 0:
                return 1e300
        return 0.5 * ll

    best, _ = _nelder_mead(nll, [3.0, -2.5], step=0.5)
    a, b = unpack(best)
    w = var * (1 - a - b)
    s2 = var
    z = []
    for v in e:
        z.append(v / math.sqrt(s2))
        s2 = w + a * v * v + b * s2
    pers = a + b
    return dict(
        alpha=a, beta=b, persistence=pers,
        half_life_bars=math.log(0.5) / math.log(pers) if 0 < pers < 1 else float("inf"),
        std_resid_excess_kurtosis=moments(z)["excess_kurtosis"],
        student_t_df=student_t_df(moments(z)["excess_kurtosis"]),
    )


def student_t_df(excess_kurtosis: float) -> float:
    """Method of moments: a Student-t has excess kurtosis 6/(df-4)."""
    if excess_kurtosis <= 0.05:
        return 100.0
    return 4 + 6 / excess_kurtosis


# --- regimes: two-state mixture on log realized vol ---------------------------


def two_regimes(rv: list[float], iters: int = 200) -> dict:
    """EM for a 2-component Gaussian mixture on log(realized vol) per block,
    then hard labels, a Markov transition matrix and mean dwell times. Simple
    and explainable; a full Markov-switching fit is a later upgrade."""
    x = [math.log(v) for v in rv if v > 0]
    mu = [quantile(x, 0.3), quantile(x, 0.8)]
    sd = [max(1e-3, (quantile(x, 0.75) - quantile(x, 0.25)) / 1.35)] * 2
    pi = [0.6, 0.4]
    for _ in range(iters):
        resp = []
        for v in x:
            p = [pi[k] * math.exp(-0.5 * ((v - mu[k]) / sd[k]) ** 2) / sd[k] for k in range(2)]
            s = sum(p) or 1e-300
            resp.append([p[0] / s, p[1] / s])
        for k in range(2):
            nk = sum(r[k] for r in resp) or 1e-12
            mu[k] = sum(r[k] * v for r, v in zip(resp, x)) / nk
            sd[k] = max(1e-3, math.sqrt(sum(r[k] * (v - mu[k]) ** 2 for r, v in zip(resp, x)) / nk))
            pi[k] = nk / len(x)
    if mu[0] > mu[1]:
        mu, sd, pi = mu[::-1], sd[::-1], pi[::-1]
        resp = [r[::-1] for r in resp]
    lab = [0 if r[0] >= 0.5 else 1 for r in resp]
    T = [[0, 0], [0, 0]]
    for a, b in zip(lab, lab[1:]):
        T[a][b] += 1
    P = [[T[i][j] / max(1, sum(T[i])) for j in range(2)] for i in range(2)]
    dwell = [1 / max(1e-9, 1 - P[i][i]) for i in range(2)]
    return dict(
        calm_vol_mult=math.exp(mu[0]), stressed_vol_mult=math.exp(mu[1]),
        log_sd=sd, share=[lab.count(0) / len(lab), lab.count(1) / len(lab)],
        transition=P, mean_dwell_blocks=dwell, labels=lab,
    )


# --- jumps ---------------------------------------------------------------------


def jumps(r: list[float], window: int = 60, k: float = 5.0) -> dict:
    """Returns beyond k local sigmas (sigma from the median absolute return of
    the trailing window, robust to the jump itself)."""
    out = []
    for i in range(window, len(r)):
        mad = median(abs(v) for v in r[i - window:i])
        s = 1.4826 * mad
        if s > 0 and abs(r[i]) > k * s:
            out.append(r[i])
    return dict(count=len(out), per_bar=len(out) / max(1, len(r) - window),
                abs_size_median=median([abs(v) for v in out]) if out else 0.0,
                abs_size_p90=quantile([abs(v) for v in out], 0.9) if len(out) > 2 else 0.0)


def bootstrap_ci(x: list[float], stat, n: int = 200, block: int = 60, seed: int = 7) -> tuple[float, float]:
    """Moving-block bootstrap 90% interval (blocks keep the clustering)."""
    rng = random.Random(seed)
    nb = max(1, len(x) // block)
    vals = []
    for _ in range(n):
        s = []
        for _ in range(nb):
            i = rng.randrange(0, max(1, len(x) - block))
            s.extend(x[i:i + block])
        vals.append(stat(s))
    return quantile(vals, 0.05), quantile(vals, 0.95)

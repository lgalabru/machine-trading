"""Export schema 1.0 market-calibration files for the machine-trading repo.

Reads the schema 0.1 fits in out/calibration (written by run.py), refits the
few numbers schema 1.0 needs that 0.1 doesn't carry (SPCX's GARCH, t df,
session-level vol dispersion and background jumps on US regular-hours mark
returns), and computes the realism references: the stylized-fact statistics
of real session-length windows that `mtx market-cal check` compares
generated sessions against. Raw data comes from the download cache.

    python3 -I -m calib.export_v1      # from the infra-calib directory

Writes out/calibration/v1/<calibration_id>.toml and .inputs.json.
"""

from __future__ import annotations

import hashlib
import json
import math
import random
import tomllib
from datetime import datetime, timezone
from pathlib import Path
from statistics import fmean, median
from zoneinfo import ZoneInfo

from . import fetch, fit, sources, toml_out
from . import stats as S

OUT = Path(__file__).resolve().parent.parent / "out" / "calibration"
V01 = {"sol-usd": "sol-usd.20261111.19f35b87", "spcx": "spcx.20261111.ebab5e7c"}
NY = ZoneInfo("America/New_York")

# Return checks have no stated bands: each is the statistic's sampling
# spread (see spread()). Only the gap sizes, which rest on a few dozen opens,
# keep a stated band.
BANDS = {"gap_size": {"ratio": [0.67, 1.5]}}

# NYSE full-day closures, 2026 and 2027 (nyse.com/markets/hours-calendars).
HOLIDAYS = [
    "2026-01-01", "2026-01-19", "2026-02-16", "2026-04-03", "2026-05-25", "2026-06-19",
    "2026-07-03", "2026-09-07", "2026-11-26", "2026-12-25",
    "2027-01-01", "2027-01-18", "2027-02-15", "2027-03-26", "2027-05-31", "2027-06-18",
    "2027-07-05", "2027-09-06", "2027-11-25", "2027-12-24",
]


def _r(x: float, nd: int = 6) -> float:
    return float(f"{x:.{nd}g}")


def _scaled(x: list[float]) -> list[float]:
    s = math.sqrt(fmean(v * v for v in x)) if x else 0.0
    return [v / s for v in x] if s > 0 else []


def window_stats(windows: list[list[float]], p: dict) -> dict:
    """The stylized facts of a set of windows of per-bar returns; `p` holds
    the bar counts (vol_bars, agg_bars, cluster_bars, cluster_lags). The Rust
    port is mtx_grader::market::realism::window_stats; keep the two in step.

    vol_level and vol_of_vol read returns over vol_bars (above the tick);
    vol_of_vol is the spread of the windows' log sd (interquartile range
    over 1.349: an sd for a normal), which a few wild or ticked-flat windows
    can't move.
    The tail statistics read each window scaled to unit RMS, so they measure
    the shape of returns, not how volatility differs between windows."""
    sd, pooled, norm, lag1 = [], [], [], []
    for r in windows:
        sd.append(S.moments(S.aggregate(r, p["vol_bars"]))["sd"])
        ra = S.aggregate(r, p["agg_bars"])
        pooled.extend(ra)
        norm.extend(_scaled(ra))
        lag1.append(S.acf(ra, [1]).get(1, 0.0))
    xs = [[abs(v) for v in _scaled(S.aggregate(r, p["cluster_bars"]))] for r in windows]
    xs = [x for x in xs if x]
    flat = [v for x in xs for v in x]
    m = fmean(flat)
    c0 = fmean((v - m) ** 2 for v in flat)
    acf_abs = []
    for L in p["cluster_lags"]:
        num = [(x[i] - m) * (x[i + L] - m) for x in xs for i in range(len(x) - L)]
        acf_abs.append(_r(fmean(num) / c0 if num and c0 else 0.0, 4))
    a = [abs(v) for v in norm]
    return dict(
        vol_level=_r(median(sd)),
        vol_of_vol=_r(log_spread([math.log(max(v, 1e-300)) for v in sd]), 4),
        kurtosis_within=_r(S.moments(norm)["excess_kurtosis"], 4),
        kurtosis_pooled=_r(S.moments(pooled)["excess_kurtosis"], 4),
        tail_index=_r(S.hill_tail_index(pooled, 0.025)["alpha"], 4),
        clustering=acf_abs,
        predictability=_r(fmean(lag1), 4),
        tail_ratio=_r(S.quantile(a, 0.99) / S.quantile(a, 0.9), 4),
    )


def log_spread(x: list[float]) -> float:
    return (S.quantile(x, 0.75) - S.quantile(x, 0.25)) / 1.349


def nulls(windows: list[list[float]], p: dict) -> dict:
    """What the statistics read on the obviously wrong market each check must
    reject, built from the real windows: Gaussian returns at each window's
    own volatility (thin tails), at one common volatility (no vol of vol),
    and each window's returns shuffled (no clustering)."""
    rng = random.Random(20261112)
    sds = [S.moments(r)["sd"] for r in windows]
    common = median(sds)
    reps = 4
    gauss = [window_stats([[rng.gauss(0, s) for _ in r] for r, s in zip(windows, sds)], p) for _ in range(reps)]
    flat = [window_stats([[rng.gauss(0, common) for _ in r] for r in windows], p) for _ in range(reps)]
    shuffled = []
    for _ in range(reps):
        ws = [r[:] for r in windows]
        for w in ws:
            rng.shuffle(w)
        shuffled.append(window_stats(ws, p))
    return dict(
        source="Gaussian returns at each real window's sd (tails), at the windows' median sd (vol of vol), "
               "and each real window shuffled (clustering); mean of 4 draws",
        vol_of_vol=_r(fmean(d["vol_of_vol"] for d in flat), 4),
        tail_ratio=_r(fmean(d["tail_ratio"] for d in gauss), 4),
        kurtosis_within=_r(fmean(d["kurtosis_within"] for d in gauss), 4),
        clustering=[_r(fmean(d["clustering"][i] for d in shuffled), 4) for i in range(len(p["cluster_lags"]))],
    )


def simulate(df: float, alpha: float, beta: float, blocks: int, draws: int = 3000, seed: int = 7) -> list[list[float]]:
    """Windows of the market model's block returns at unit level: GARCH(1,1)
    variance starting at its target, unit-variance Student-t(df)
    innovations, `blocks` blocks a window. The Rust model draws the same
    process (mtx_grader::market::model)."""
    rng = random.Random(seed)
    scale = math.sqrt((df - 2) / df)
    out = []
    for _ in range(draws):
        h, w = 1.0, []
        for _ in range(blocks):
            e = math.sqrt(h) * rng.gauss(0, 1) / math.sqrt(rng.gammavariate(df / 2, 2) / df) * scale
            w.append(e)
            h = (1 - alpha - beta) + alpha * e * e + beta * h
        out.append(w)
    return out


def fit_shape(ref: dict, alpha: float, beta: float, blocks: int, real_log_sd_var: float | None = None) -> dict:
    """Fit the model's shape to the same window statistics the checks read
    (simulated moments):
    - df: the Student-t df at which simulated windows match the reference's
      kurtosis_within (bisection; kurtosis falls as df rises);
    - bias: the median window sd of simulated unit-level windows, which the
      base level divides out, so the median session reads the reference's
      vol_level;
    - level_log_sd: the session-to-session dispersion of the level, what is
      left of the real windows' log-sd variance after the variance the
      simulated windows show at one level (sampling and GARCH)."""
    def kw(df: float) -> float:
        ws = simulate(df, alpha, beta, blocks)
        return S.moments([v for w in ws for v in _scaled(w)])["excess_kurtosis"]

    lo, hi = 2.2, 60.0
    if kw(hi) >= ref["kurtosis_within"]:
        df = hi
    else:
        for _ in range(14):
            mid = math.sqrt(lo * hi)
            if kw(mid) > ref["kurtosis_within"]:
                lo = mid
            else:
                hi = mid
        df = math.sqrt(lo * hi)
    ws = simulate(df, alpha, beta, blocks)
    sds = [S.moments(w)["sd"] for w in ws]
    out = dict(df=df, bias=median(sds), kurtosis_within=kw(df))
    if real_log_sd_var is not None:
        noise = S.moments([math.log(v) for v in sds])["sd"] ** 2
        out["level_log_sd"] = math.sqrt(max(0.0, real_log_sd_var - noise))
    return out


def simulate_mark(df: float, alpha: float, beta: float, half_life: float, sigma: float, blocks: int,
                  block_seconds: float, sub: int = 12, draws: int = 3000, seed: int = 7) -> list[list[float]]:
    """Windows of the market model's block returns with a smoothed mark, as
    the Rust model draws them (mtx_grader::market::model), at sub-steps of
    block_seconds / sub instead of slots: each session's level a log-normal
    draw (median 1, log-sd sigma); GARCH(1,1) starting at its target;
    unit-variance Student-t(df) block innovations, the normal part bridged
    over the sub-steps and the t scale's excess in one of them; and the mark
    an exponential average of that path with the given half-life, updated
    each sub-step (the mark's update_seconds)."""
    rng = random.Random(seed)
    lam = 1 - 0.5 ** ((block_seconds / sub) / half_life) if half_life > 0 else 1.0
    out = []
    for _ in range(draws):
        level = math.exp(sigma * rng.gauss(0, 1))
        h, p, m, w = 1.0, 0.0, 0.0, []
        for _ in range(blocks):
            z = rng.gauss(0, 1)
            t_scale = math.sqrt((df - 2) / rng.gammavariate(df / 2, 2))
            diffusive = math.sqrt(h) * z
            excess = diffusive * (t_scale - 1)
            at = rng.randrange(sub)
            g = [rng.gauss(0, 1) for _ in range(sub)]
            gm = sum(g) / sub
            start = m
            for j in range(sub):
                p += level * (diffusive / sub + math.sqrt(h / sub) * (g[j] - gm) + (excess if j == at else 0.0))
                m += lam * (p - m)
            w.append(m - start)
            e = diffusive * t_scale
            h = (1 - alpha - beta) + alpha * e * e + beta * h
        out.append(w)
    return out


def bisect(f, target: float, lo: float, hi: float, rising: bool, steps: int = 12, log: bool = False) -> float:
    """The x in [lo, hi] where f(x) crosses target (f monotone)."""
    for _ in range(steps):
        mid = math.sqrt(lo * hi) if log else (lo + hi) / 2
        if (f(mid) < target) == rising:
            lo = mid
        else:
            hi = mid
    return math.sqrt(lo * hi) if log else (lo + hi) / 2


def fit_mark_shape(ref: dict, p: dict, persistence: float, df0: float, blocks: int, block_seconds: float) -> dict:
    """SPCX's shape by simulated moments, on the window statistics the checks
    read, under check_model's setup (sessions inside the regular session,
    GARCH from its target, `blocks` blocks each):
    - alpha and the persistence alpha + beta: the |r| ACF at lags 1 and 5;
    - the mark's half-life: the lag-1 return ACF (predictability);
    - df: the within-window kurtosis;
    - the session level's log-sd: vol of vol;
    - bias: the median window sd at unit median level, which the base level
      divides out.
    The first three interact a little, so they are fitted in turn three
    times."""
    def stats_p(df, alpha, hl, pers, sigma=0.0):
        return window_stats(simulate_mark(df, alpha, pers - alpha, hl, sigma, blocks, block_seconds, draws=2000), p)

    def stats(df, alpha, hl, sigma=0.0):
        return window_stats(simulate_mark(df, alpha, persistence - alpha, hl, sigma, blocks, block_seconds), p)

    i5 = p["cluster_lags"].index(5)
    df, alpha, hl = df0, 0.1, 2.0
    for _ in range(3):
        # alpha sets the lag-1 |r| ACF and the persistence how fast it
        # decays: fit persistence to lag 5, alpha to lag 1 within it.
        def lag5(pers: float) -> float:
            a = bisect(lambda a_: stats_p(df, a_, hl, pers)["clustering"][0], ref["clustering"][0], 0.01, pers - 0.01, True, steps=9)
            return stats_p(df, a, hl, pers)["clustering"][i5]

        persistence = bisect(lag5, ref["clustering"][i5], 0.5, 0.999, True, steps=9)
        alpha = bisect(lambda a: stats(df, a, hl)["clustering"][0], ref["clustering"][0], 0.01, persistence - 0.01, True)
        hl = bisect(lambda x: stats(df, alpha, x)["predictability"], ref["predictability"], 0.05, 120.0, True, log=True)
        df = bisect(lambda d: stats(d, alpha, hl)["kurtosis_within"], ref["kurtosis_within"], 2.2, 60.0, False, log=True)
    sigma = bisect(lambda s_: stats(df, alpha, hl, s_)["vol_of_vol"], ref["vol_of_vol"], 0.0, 1.5, True)
    ws = simulate_mark(df, alpha, persistence - alpha, hl, sigma, blocks, block_seconds)
    final = window_stats(ws, p)
    return dict(df=df, alpha=alpha, beta=persistence - alpha, persistence=persistence, half_life=hl, sigma=sigma,
                bias=median(S.moments(w)["sd"] for w in ws), fitted=final)


def fit_sol_shape(ref: dict, long_ref: dict, p: dict, long_p: dict, reg: dict, intraday: list[float],
                  qmle_persistence: float, blocks: int, block_seconds: float, tick: float) -> dict:
    """SOL's shape by simulated moments, drawn as check_model draws it and
    measured as the checks measure it.

    Short (120 s) windows: each session a regime (by share), an hour of the
    day and a level (the regime's multiplier times a log-normal draw times
    the hour's multiplier, over the mixture's median, as the Rust model
    normalizes it); GARCH-t blocks from their target, bridged over 1 s
    sub-steps with the t scale's excess in one; a mark trailing that path
    by an exponential average updated each second; the mark's price rounded
    to the tick at every block boundary, as the real closes are. Long (1 h)
    windows: the same at block resolution (the 1 s mark and the tick don't
    reach 1 m bars), the hour's multiplier per block and regimes switching
    at their hourly rates.

    Fits in turn, twice: alpha and persistence (the long windows' |r| ACF
    at lags 1 and 5), then df (within-window kurtosis), the mark's
    half-life (lag-1 ACF), k, a common scale on the regimes' within-state
    log-sds (vol of vol), and the base per-second sd (median window sd)."""
    rng = random.Random(11)
    mults = [reg["calm_vol_mult"], reg["stressed_vol_mult"]]
    pick = lambda u: 0 if u < reg["share"][0] else 1  # noqa: E731
    draws = [(pick(rng.random()), rng.randrange(24), rng.gauss(0, 1)) for _ in range(3000)]
    big = [(pick(rng.random()), rng.randrange(24), rng.gauss(0, 1)) for _ in range(100_000)]
    pb = dict(p, vol_bars=1, agg_bars=1, cluster_bars=1)
    sub = 10

    def norm(k: float) -> float:
        lv = sorted(mults[r] * math.exp(k * reg["log_sd_within"][r] * z) * intraday[h] for r, h, z in big)
        return lv[len(lv) // 2]

    def short(df: float, alpha: float, beta: float, hl: float, k: float, base: float) -> list[list[float]]:
        rs_ = random.Random(7)
        lam = 1 - 0.5 ** (1.0 / hl)
        n = norm(k)
        out = []
        for r, hr, z in draws:
            sd = base * math.sqrt(block_seconds) * mults[r] * math.exp(k * reg["log_sd_within"][r] * z) * intraday[hr] / n
            h, path, mark = 1.0, 0.0, 0.0
            prev, rets = round(1 / tick) * tick, []
            for _ in range(blocks):
                zz = rs_.gauss(0, 1)
                t_scale = math.sqrt((df - 2) / rs_.gammavariate(df / 2, 2))
                diffusive = math.sqrt(h) * zz
                excess = diffusive * (t_scale - 1)
                at = rs_.randrange(sub)
                g = [rs_.gauss(0, 1) for _ in range(sub)]
                gm = sum(g) / sub
                for j in range(sub):
                    path += sd * (diffusive / sub + math.sqrt(h / sub) * (g[j] - gm) + (excess if j == at else 0.0))
                    mark += lam * (path - mark)
                px = round(math.exp(mark) / tick) * tick
                rets.append(math.log(px / prev))
                prev = px
                e = diffusive * t_scale
                h = (1 - alpha - beta) + alpha * e * e + beta * h
            out.append(rets)
        return out

    def long(df: float, alpha: float, beta: float, k: float, draws: int = 4000) -> list[list[float]]:
        rs_ = random.Random(5)
        per = long_p["window_bars"] * round(long_p["bar_seconds"] / block_seconds)
        bar = round(long_p["bar_seconds"] / block_seconds)
        switch = [1 - reg["transition_per_hour"][i][i] for i in range(2)]
        scale = math.sqrt((df - 2) / df)
        out = []
        for _ in range(draws):
            r = pick(rs_.random())
            levels = [mults[i] * math.exp(k * reg["log_sd_within"][i] * rs_.gauss(0, 1)) for i in range(2)]
            hour0 = rs_.random() * 24
            h, acc, w = 1.0, 0.0, []
            for b_ in range(per):
                hr = int(hour0 + b_ * block_seconds / 3600) % 24
                e = math.sqrt(h) * rs_.gauss(0, 1) / math.sqrt(rs_.gammavariate(df / 2, 2) / df) * scale
                acc += e * levels[r] * intraday[hr]
                if b_ % bar == bar - 1:
                    w.append(acc)
                    acc = 0.0
                h = (1 - alpha - beta) + alpha * e * e + beta * h
                if rs_.random() < switch[r] * block_seconds / 3600:
                    r = 1 - r
            out.append(w)
        return out

    def long_acf(df: float, alpha: float, pers: float, k: float) -> list[float]:
        return window_stats(long(df, alpha, pers - alpha, k), long_p)["clustering"]

    df, hl, k, base = 4.5, 1.0, 1.0, ref["vol_level"] / math.sqrt(block_seconds)
    pers, alpha = qmle_persistence, 0.04
    target = long_ref["reference"]

    def fit_long(alpha: float, pers: float, rounds: int) -> tuple[float, float]:
        # alpha sets the |r| ACF at lag 1 and the GARCH persistence its
        # decay (lag 5): fitted in turn.
        for _ in range(rounds):
            alpha = bisect(lambda a_: long_acf(df, a_, pers, k)[0], target[0], 0.002, pers - 0.001, True, steps=8)
            pers = bisect(lambda p_: long_acf(df, alpha, p_, k)[1], target[1], alpha + 0.01, 0.9995, True, steps=8)
        return alpha, pers

    for _ in range(3):
        alpha, pers = fit_long(alpha, pers, 2)
        beta = pers - alpha
        df = bisect(lambda d: window_stats(short(d, alpha, beta, hl, k, base), pb)["kurtosis_within"], ref["kurtosis_within"],
                    2.2, 60.0, False, log=True)
        hl = bisect(lambda x: window_stats(short(df, alpha, beta, x, k, base), pb)["predictability"], ref["predictability"],
                    0.05, 60.0, True, log=True)
        k = bisect(lambda k_: window_stats(short(df, alpha, beta, hl, k_, base), pb)["vol_of_vol"], ref["vol_of_vol"], 0.05, 4.0, True)
        base = bisect(lambda b_: window_stats(short(df, alpha, beta, hl, k, b_), pb)["vol_level"], ref["vol_level"], base / 3,
                      base * 3, True, log=True)
    # The long statistics last, so they match at the final df.
    alpha, pers = fit_long(alpha, pers, 2)
    return dict(df=df, alpha=alpha, beta=pers - alpha, half_life=hl, k=k, base=base,
                fitted=window_stats(short(df, alpha, pers - alpha, hl, k, base), pb),
                fitted_long=long_acf(df, alpha, pers, k))


def long_reference(windows: list[list[float]], lp: dict) -> dict:
    """A longer-window clustering reference: the |r| ACF within windows,
    its sampling spread over 300 windows, and the shuffled-window null."""
    rng = random.Random(20261113)
    ref = window_stats(windows, lp)["clustering"]
    draws = [window_stats([rng.choice(windows) for _ in range(SPREAD_WINDOWS)], lp)["clustering"] for _ in range(200)]
    shuffled = []
    for _ in range(4):
        ws = [r[:] for r in windows]
        for w in ws:
            rng.shuffle(w)
        shuffled.append(window_stats(ws, lp)["clustering"])
    n = len(lp["cluster_lags"])
    return dict(
        reference=ref,
        spread_windows=SPREAD_WINDOWS,
        spread_sd=[_r(S.moments([d[i] for d in draws])["sd"], 4) for i in range(n)],
        null=[_r(fmean(d[i] for d in shuffled), 4) for i in range(n)],
    )


# The sampling spread of each statistic: windows drawn with replacement,
# SPREAD_WINDOWS at a time. Ratio statistics get the sd of their log; the
# others (near zero) the sd itself. The check scales it to its own window
# count by sqrt(SPREAD_WINDOWS / n).
SPREAD_WINDOWS = 300
SPREAD_DRAWS = 400


# The chance a fair generator fails any of a file's gated return checks:
# each band is wide enough (Sidak) that the family stays under it.
FAMILY_FALSE_FAIL = 0.01


def band_sigmas(gated: int) -> float:
    from statistics import NormalDist

    return NormalDist().inv_cdf(1 - (1 - (1 - FAMILY_FALSE_FAIL) ** (1 / gated)) / 2)


def spread(windows: list[list[float]], p: dict, extra_gated: int = 0) -> dict:
    rng = random.Random(20261112)
    draws = [window_stats([rng.choice(windows) for _ in range(SPREAD_WINDOWS)], p)
             for _ in range(SPREAD_DRAWS)]

    def sd(xs: list[float]) -> float:
        return _r(S.moments(xs)["sd"], 4)

    return dict(
        windows=SPREAD_WINDOWS,
        draws=SPREAD_DRAWS,
        # vol_level, vol_of_vol, kurtosis_within, tail_ratio, predictability,
        # the clustering lags, and any long-window lags.
        sigmas=_r(band_sigmas(5 + len(p["cluster_lags"]) + extra_gated), 4),
        sigmas_source=(f"Sidak: each of the {5 + len(p['cluster_lags']) + extra_gated} gated return checks at "
                       f"{FAMILY_FALSE_FAIL:.0%} family-wise false fails"),
        log_sd=dict(
            vol_level=sd([math.log(d["vol_level"]) for d in draws]),
            vol_of_vol=sd([math.log(d["vol_of_vol"]) for d in draws]),
            tail_ratio=sd([math.log(d["tail_ratio"]) for d in draws]),
        ),
        sd=dict(
            kurtosis_within=sd([d["kurtosis_within"] for d in draws]),
            clustering=[sd([d["clustering"][i] for d in draws]) for i in range(len(p["cluster_lags"]))],
            predictability=sd([d["predictability"] for d in draws]),
        ),
    )


def code_hash() -> str:
    h = hashlib.sha256()
    for p in sorted(Path(__file__).resolve().parent.glob("*.py")):
        h.update(p.read_bytes())
    return h.hexdigest()[:12]


FIT_DATE = "20261112"


def meta(asset: str, symbol: str, inputs: list[dict], derived_from: str, fitted_at: str,
         sources_: list[str], limits: list[str], windows: dict) -> dict:
    code = code_hash()
    digest = hashlib.sha256("".join(i["sha256"] for i in inputs).encode()).hexdigest()[:16]
    version = hashlib.sha256((digest + code).encode()).hexdigest()[:8]
    cid = f"{asset}.{FIT_DATE}.{version}"
    return dict(
        schema_version="1.0",
        calibration_id=cid,
        asset=asset,
        symbol=symbol,
        fitted_at=fitted_at,
        exported_at=datetime.now(timezone.utc).isoformat(timespec="seconds"),
        fitter="infra-calib (calib/fit.py, calib/export_v1.py)",
        fitter_code_sha256_12=code,
        derived_from=derived_from,
        inputs_file=f"{cid}.inputs.json",
        inputs_digest=digest,
        inputs_count=len(inputs),
        sources=sources_,
        limits=limits,
        windows=windows,
    )


def book_from_power_law(k_bps: float, gamma: float) -> list[float]:
    """Depth within d bp of mid, from an average-cost curve cost = k q^gamma
    (marginal cost (1 + gamma) k q^gamma)."""
    return [_r((d / ((1 + gamma) * k_bps)) ** (1 / gamma), 4) for d in (5, 10, 25, 50, 100)]


def sol(old: dict) -> tuple[dict, list[dict]]:
    fv = old["fair_value"]
    g = fv["vol_clustering"]["at_10s"]
    reg = fv["regimes"]
    ph = old["venues"]["phoenix_perp"]
    jup = old["liquidity"]["jupiter_routed_sol_usdc"]
    k = (jup["sell_sol"]["power_law"]["k_bps"] + jup["buy_sol"]["power_law"]["k_bps"]) / 2
    gamma = (jup["sell_sol"]["power_law"]["gamma"] + jup["buy_sol"]["power_law"]["gamma"]) / 2

    # Realism reference: the held-out days (2026-11-08, -09), 1 s closes,
    # cut into 120 s windows (480 slots at 0.25 s: the shortest engine
    # session), every window.
    hold = sources.sol_1s()[-2:]
    windows = []
    for d in hold:
        c = [k_["c"] for k_ in d]
        for i in range(0, len(c) - 121, 120):
            w = c[i:i + 121]
            windows.append([math.log(b / a) for a, b in zip(w, w[1:])])
    # 1 s closes sit below the tick ($0.01 against a 1 s sd of ~0.8 bp), so
    # every statistic reads 10 s returns: 12 to a window.
    p = dict(vol_bars=10, agg_bars=10, cluster_bars=10, cluster_lags=[1, 2, 3])
    ref = window_stats(windows, p)
    p0 = fmean(c)
    # Long reference: 1 h windows of consecutive 1 m closes over the 6
    # months, where volatility clustering within a session shows (120 s
    # windows are too short for it).
    lp = dict(vol_bars=1, agg_bars=1, cluster_bars=1, cluster_lags=[1, 5, 15], bar_seconds=60, window_bars=60)
    m1 = sources.sol_1m()
    long_windows, run = [], []
    for a, b in zip(m1, m1[1:]):
        if b["t"] - a["t"] != 60:
            run = []
            continue
        run.append(math.log(b["c"] / a["c"]))
        if len(run) == lp["window_bars"]:
            long_windows.append(run)
            run = []
    long_ref = long_reference(long_windows, lp)
    levels = fit_sol_shape(ref, long_ref, p, lp, reg, fv["intraday_vol_mult_utc"], g["alpha"] + g["beta"], 12, 10.0,
                           0.01 / p0)
    df = levels["df"]
    print("sol shape fit:", levels)
    inputs = json.loads((OUT / f"{old['meta']['calibration_id']}.inputs.json").read_text())

    return dict(
        meta=meta(
            "sol-usd", "SOL", inputs, old["meta"]["calibration_id"], datetime.now(timezone.utc).isoformat(timespec="seconds"),
            ["Binance SOLUSDT 1 s and 1 m klines (data.binance.vision)", "Phoenix perp API /exchange and /candles",
             "Jupiter lite-api quotes", "Solana RPC getRecentPrioritizationFees", "Jito tip floor and stake-pool stats"],
            ["Fair value is a CEX proxy (Binance SOLUSDT), not Pyth.",
             ("The seconds-scale parameters are fitted to the 2 held-out days' realism windows (below), not to the 5 days "
              "before them that the 0.1 prototype used; regimes and jumps come from 6 months of 1 m bars that hold no crisis "
              "(calm/stressed differ by 1.5x)."),
             "Book depth and impact are derived from one Jupiter routed-quote snapshot, not from a recorded Phoenix book.",
             "Phoenix trade sizes are per-minute means, not per-trade sizes.",
             ("Fitted to the realism windows themselves (in-sample checks): per_second_sd (vol_level), df (kurtosis_within), the "
              "regimes' log-sd scale (vol_of_vol) and the mark's half-life (predictability), on the 2 held-out days; GARCH alpha "
              "and persistence (long_clustering_lag_1 and _5), on the 6 months of 1 m bars."),
             ("Out of fit: tail_ratio, clustering_lag_1 to _3 and round_trips. The regimes' multipliers, "
              "shares and transitions come from 6 months of hourly realized vol; the jumps from 6 months of 1 m bars."),
             ("long_clustering_lag_15 is reported, not gated: real 1 m volatility clustering decays more slowly than one "
              "GARCH(1,1) with regimes can follow (long memory), and the model reads about 0.01 below it."),
             "Realism bands are each statistic's sampling spread over the real windows of these days; other weeks would add week-to-week spread."],
            old["meta"]["windows"],
        ),
        fair_value=dict(
            model="regime_garch_t",
            reference=fv["reference"],
            per_second_sd=_r(levels["base"]),
            per_second_sd_source=("the median session's level, by simulated moments: the model's 120 s windows, ticked as the real "
                                  "closes are, match the realism windows' median 10 s sd"),
            block_seconds=10,
            intraday_vol_mult_utc=fv["intraday_vol_mult_utc"],
            tails=dict(
                model="student_t",
                df=_r(df, 4),
                df_source=(f"simulated moments: the df at which the model's 120 s windows of 10 s returns, ticked as the real closes "
                           f"are, match the realism windows' within-window kurtosis ({ref['kurtosis_within']}); the 0.1 fit's "
                           f"GARCH-standardized estimate was {fv['tails']['student_t_df_10s']}"),
                hill_alpha_1m=fv["tails"]["hill_alpha_1m"],
                hill_alpha_1m_ci90=fv["tails"]["hill_alpha_1m_ci90"],
                excess_kurtosis_by_horizon={k_: v["excess_kurtosis"] for k_, v in fv["horizons_seconds"].items()},
            ),
            clustering=dict(model="garch11", alpha=_r(levels["alpha"], 4), beta=_r(levels["beta"], 4),
                            half_life_seconds=_r(math.log(0.5) / math.log(levels["alpha"] + levels["beta"]) * 10, 4),
                            source=(f"alpha and alpha + beta by simulated moments to the long realism windows' |r| ACF at lags 1 "
                                    f"and 5 ({long_ref['reference'][0]}, {long_ref['reference'][1]}); Gaussian QMLE on 10 s "
                                    f"returns gave alpha {g['alpha']}, beta {g['beta']}")),
            mark=dict(model="ema", half_life_seconds=_r(levels["half_life"], 4), update_seconds=1.0,
                      source=("a reduced form for the short-lag momentum in Binance's 1 s closes: every second the price moves "
                              "toward the fair path, an exponential average with this half-life, by simulated moments to the "
                              f"realism windows' lag-1 ACF of 10 s returns ({ref['predictability']})")),
            regimes=dict(
                model="markov",
                names=["calm", "stressed"],
                vol_mult=[reg["calm_vol_mult"], reg["stressed_vol_mult"]],
                log_sd_within=[_r(levels["k"] * v, 4) for v in reg["log_sd_within"]],
                share=reg["share"],
                transition_per_hour=reg["transition_per_hour"],
                source=(f"2-component mixture on log hourly realized vol, 6 months of 1 m bars; the within-state log-sds "
                        f"({reg['log_sd_within'][0]}, {reg['log_sd_within'][1]}) scaled by {levels['k']:.4f}, by simulated moments "
                        f"to the realism windows' vol of vol ({ref['vol_of_vol']}), drawn and ticked as check_model draws and "
                        f"measures (sessions at any hour, GARCH from its target)"),
            ),
            jumps=dict(model="poisson_lognormal", per_hour=fv["jumps"]["per_hour"],
                       abs_size_median_pct=fv["jumps"]["abs_size_median_pct"],
                       abs_size_p90_pct=fv["jumps"]["abs_size_p90_pct"], rule=fv["jumps"]["rule"]),
        ),
        calendar=dict(model="continuous", note="SOL trades 24/7; intraday_vol_mult_utc shapes the day."),
        venues=dict(phoenix_perp=dict(
            market=ph["market"], taker_fee=ph["taker_fee"], maker_fee=ph["maker_fee"], tick_size=ph["tick_size"],
            base_lots_decimals=ph["base_lots_decimals"],
            trades_per_minute=ph["trades_per_minute_mean"], trades_dispersion_index=ph["trades_per_minute_dispersion_index"],
            trade_size=dict(model="lognormal", median=ph["mean_trade_size_per_minute_lognormal"]["median"],
                            sigma=ph["mean_trade_size_per_minute_lognormal"]["sigma"], unit="base",
                            source="per-minute mean trade size from Phoenix candles"),
            premium_bps=dict(p10=_r(ph["last_trade_vs_mark_bps"]["p10"], 3), median=ph["last_trade_vs_mark_bps"]["median"],
                             p90=ph["last_trade_vs_mark_bps"]["p90"], source="last trade vs mark per minute (not mark vs index)"),
            funding=dict(interval_seconds=ph["funding"]["interval_seconds"], period_seconds=ph["funding"]["period_seconds"],
                         max_rate_per_interval=ph["funding"]["max_rate_per_interval"],
                         max_rate_per_interval_pct=ph["funding"]["max_rate_per_interval_pct"], source="Phoenix /exchange"),
        )),
        book=dict(
            model="defaults",
            half_spread_bps=1.0,
            half_spread_source="default: CEX Roll spread is 0.35 bp; Phoenix's own book is not recorded yet",
            depth_bps=[5.0, 10.0, 25.0, 50.0, 100.0],
            depth_base=book_from_power_law(k, gamma),
            depth_source="derived from the Jupiter routed cost curve (all Solana DEX liquidity), one snapshot",
            impact=dict(model="power_law", k_bps=_r(k, 4), gamma=_r(gamma, 4), size_unit="base",
                        permanent_share=0.5, decay_half_life_seconds=30.0,
                        source="k and gamma: Jupiter routed quotes (mean of buy and sell); permanent_share and decay: defaults, no fill tapes yet"),
        ),
        realism=dict(
            source=f"Binance SOLUSDT 1 s closes on held-out days {', '.join(old['meta']['windows']['seconds_holdout'])}, "
                   "non-overlapping 120 s windows, every window",
            windows=len(windows),
            bar_seconds=1,
            window_bars=120,
            **p,
            price_tick_rel=_r(0.01 / p0, 4),
            round_trip_min_losing_share=0.95,
            reference=ref,
            spread=spread(windows, p, extra_gated=2),
            null=nulls(windows, p),
            bands=BANDS,
            fitted=["vol_level", "vol_of_vol", "kurtosis_within", "predictability", "long_clustering_lag_1",
                    "long_clustering_lag_5"],
            out_of_fit=["tail_ratio", "clustering_lag_1", "clustering_lag_2", "clustering_lag_3", "round_trips"],
            long=dict(
                source=("Binance SOLUSDT 1 m closes, 2026-05 to 2026-10, non-overlapping 1 h windows of consecutive minutes"),
                windows=len(long_windows),
                bar_seconds=lp["bar_seconds"],
                window_bars=lp["window_bars"],
                cluster_lags=lp["cluster_lags"],
                info_lags=[15],
                **long_ref,
            ),
        ),
    ), inputs


SPCX_FROM = datetime(2026, 6, 15, tzinfo=timezone.utc)  # the first Monday after the 2026-06-12 IPO
SPCX_TO = datetime(2026, 11, 11, tzinfo=timezone.utc)


def spcx(old: dict) -> tuple[dict, list[dict]]:
    """SPCX refitted on Phoenix's whole 1 m mark history since the IPO week
    (the prototype's 0.1 fit used 30 days): regular-hours returns for the
    fair value, and every hour for the closed-hours ratio and the open gap.
    Venue numbers stay the 0.1 fit's (30 days)."""
    ph = old["venues"]["phoenix_perp"]
    c = [k for k in fetch.phoenix_candles("SPCX", "1m", datetime(2026, 6, 12, tzinfo=timezone.utc), SPCX_TO)
         if k["time"] >= SPCX_FROM.timestamp() * 1000]

    def is_open(t: datetime) -> bool:
        return t.weekday() < 5 and (t.hour, t.minute) >= (9, 31) and t.hour < 16

    runs, run, hours, closed = [], [], {}, []
    for a, b in zip(c, c[1:]):
        if not (a["markClose"] and b["markClose"] and b["time"] - a["time"] == 60_000):
            if run:
                runs.append(run)
            run = []
            continue
        t = datetime.fromtimestamp(b["time"] / 1000, NY)
        r = math.log(b["markClose"] / a["markClose"])
        if not is_open(t):
            if (t.hour, t.minute) != (9, 30):
                closed.append(r)
            if run:
                runs.append(run)
            run = []
            continue
        run.append(r)
        hours.setdefault((t.date(), t.hour), []).append(r)
    if run:
        runs.append(run)
    r_open = [v for r in runs for v in r]
    g = S.garch11(r_open)
    rv = [math.sqrt(sum(v * v for v in h)) for h in hours.values() if len(h) >= 50]
    log_sd = S.moments([math.log(v) for v in rv if v > 0])["sd"]
    jumps = S.jumps(r_open)
    open_sd, closed_sd = S.moments(r_open)["sd"], S.moments(closed)["sd"]
    closed_mult = closed_sd / open_sd

    # The open: the 09:30 New York minute's move, and the move over the
    # closed hours before it (the last 15:59 bar to the bar before 09:30).
    by_t = {k["time"]: k for k in c}
    gaps, moves = [], []
    for k in c:
        t = datetime.fromtimestamp(k["time"] / 1000, NY)
        if t.weekday() < 5 and (t.hour, t.minute) == (9, 30):
            prev = by_t.get(k["time"] - 60_000)
            if prev and prev["markClose"] and k["markClose"]:
                gaps.append(abs(math.log(k["markClose"] / prev["markClose"])) * 100)
                back = k["time"] - 60_000
                while back in by_t:
                    tb = datetime.fromtimestamp(back / 1000, NY)
                    if tb.weekday() < 5 and (tb.hour, tb.minute) == (15, 59):
                        if by_t[back]["markClose"]:
                            moves.append(abs(math.log(prev["markClose"] / by_t[back]["markClose"])) * 100)
                        break
                    back -= 60_000

    # Realism reference: 30-minute windows of consecutive regular-hours
    # minutes. Every statistic reads 1 m returns.
    L = 30
    windows = [r[i:i + L] for r in runs for i in range(0, len(r) - L + 1, L)]
    p = dict(vol_bars=1, agg_bars=1, cluster_bars=1, cluster_lags=[1, 5, 10])
    ref = window_stats(windows, p)
    shape = fit_mark_shape(ref, p, g["alpha"] + g["beta"], g["student_t_df"], L, 60.0)
    df, bias = shape["df"], shape["bias"]
    print("spcx shape fit:", {k: v for k, v in shape.items()})

    names = [n for n in sorted(fetch._manifest()) if n.startswith(("phoenix/candles_SPCX", "phoenix/exchange", "rpc/prio_", "rpc/sigs_SPCX", "jito/tip"))]
    inputs = fetch.provenance(names)
    first, last = SPCX_FROM.date().isoformat(), SPCX_TO.date().isoformat()
    sessions = [
        dict(kind="pre", days=["mon", "tue", "wed", "thu", "fri"], start="04:00", end="09:30", vol_mult=_r(closed_mult, 4)),
        dict(kind="regular", days=["mon", "tue", "wed", "thu", "fri"], start="09:30", end="16:00", vol_mult=1.0),
        dict(kind="post", days=["mon", "tue", "wed", "thu", "fri"], start="16:00", end="20:00", vol_mult=_r(closed_mult, 4)),
        dict(kind="overnight", days=["sun", "mon", "tue", "wed", "thu"], start="20:00", end="04:00", vol_mult=_r(closed_mult, 4)),
    ]
    return dict(
        meta=meta(
            "spcx", "SPCX", inputs, old["meta"]["calibration_id"], datetime.now(timezone.utc).isoformat(timespec="seconds"),
            ["Phoenix perp API /exchange and /candles (SPCX mark, 1 m, since the IPO week)",
             "Solana RPC getRecentPrioritizationFees and getSignaturesForAddress", "Jito tip floor", "NYSE holiday calendar"],
            [f"Fair value is Phoenix's SPCX mark (the oracle plus the perp's premium), {first} to {last}, not the underlying's tape.",
             "Only regular hours vs everything else is fitted: pre, post, overnight and weekend share the closed-hours volatility.",
             ("Fitted to the realism windows themselves (in-sample checks): per_second_sd (vol_level), df (kurtosis_within), "
              "the level's log-sd (vol_of_vol), GARCH alpha and persistence (clustering_lag_1 and _5) and the mark's half-life "
              "(predictability); the gap sizes are the fitted open gaps (gap_size). The realism windows are the history the model "
              "is fitted on."),
             "Out of fit: tail_ratio, clustering_lag_10 and round_trips.",
             "Venue numbers (trades, sizes, premium) come from 30 days (2026-10-12 to 2026-11-11).",
             "Book depth and impact are defaults: no SPCX book or fill tape has been recorded.",
             "Realism bands are each statistic's sampling spread over these windows; other periods would add period-to-period spread."],
            dict(phoenix_fair_value=[first, last], phoenix_venue=["2026-10-12", "2026-11-11"]),
        ),
        fair_value=dict(
            model="regime_garch_t",
            reference="Phoenix SPCX mark (tracks Pyth Equity.US.SPCX/USD plus the perp's premium)",
            per_second_sd=_r(ref["vol_level"] / math.sqrt(60) / bias),
            per_second_sd_source=(f"the median session's level: the realism windows' median 1 m sd / sqrt(60), over {bias:.4f}, "
                                  f"the median window sd of the model's own {L}-block windows at unit median level (simulated)"),
            block_seconds=60,
            tails=dict(
                model="student_t",
                df=_r(df, 4),
                df_source=(f"simulated moments: the df at which the model's 30-minute windows of 1 m mark returns match the realism "
                           f"windows' within-window kurtosis ({ref['kurtosis_within']}); the GARCH-standardized estimate is "
                           f"{g['student_t_df']:.3f}"),
                hill_alpha_1m=_r(S.hill_tail_index(r_open)["alpha"], 4),
                excess_kurtosis_by_horizon={"60s": _r(S.moments(r_open)["excess_kurtosis"], 4)},
            ),
            clustering=dict(model="garch11", alpha=_r(shape["alpha"], 4), beta=_r(shape["beta"], 4),
                            half_life_seconds=_r(math.log(0.5) / math.log(shape["alpha"] + shape["beta"]) * 60, 4),
                            source=(f"alpha and alpha + beta by simulated moments to the realism windows' |r| ACF at lags 1 and 5 "
                                    f"({ref['clustering'][0]}, {ref['clustering'][1]}); variance-targeted Gaussian QMLE on 1 m "
                                    f"regular-hours mark returns gave alpha {g['alpha']:.4f}, beta {g['beta']:.4f}")),
            mark=dict(model="ema", half_life_seconds=_r(shape["half_life"], 4), update_seconds=5.0,
                      source=("Phoenix's mark is a smoothed price: every update_seconds it moves toward the fair path, an "
                              "exponential average with this half-life, "
                              f"by simulated moments to the realism windows' lag-1 return ACF ({ref['predictability']})")),
            regimes=dict(
                model="markov",
                names=["base"],
                vol_mult=[1.0],
                log_sd_within=[_r(shape["sigma"], 4)],
                share=[1.0],
                transition_per_hour=[[1.0]],
                source=(f"one state: the session level's log-sd by simulated moments to the realism windows' vol of vol "
                        f"({ref['vol_of_vol']}), drawn as check_model draws (30-minute regular-session sessions, GARCH from "
                        f"its target); the sd of log hourly realized vol is {log_sd:.3f}"),
            ),
            jumps=dict(model="poisson_lognormal", per_hour=_r(jumps["per_bar"] * 60, 4),
                       abs_size_median_pct=_r(jumps["abs_size_median"] * 100, 4),
                       abs_size_p90_pct=_r(jumps["abs_size_p90"] * 100, 4),
                       rule="1 m regular-hours return beyond 5 robust sigmas of the trailing hour"),
        ),
        calendar=dict(
            model="sessions",
            timezone="America/New_York",
            sessions=sessions,
            closed_vol_mult=_r(closed_mult, 4),
            closed_source="1 m mark sd outside regular hours over the regular-hours sd; weekends and holidays are closed",
            holidays=HOLIDAYS,
            draw_range=["2027-01-04", "2027-12-31"],
            gaps=[dict(into="regular", n=len(gaps), abs_median_pct=_r(median(gaps), 4),
                       abs_p90_pct=_r(S.quantile(gaps, 0.9), 4),
                       source="1 m mark move at the 09:30 New York open")],
            closed_hours_move=dict(n=len(moves), abs_median_pct=_r(median(moves), 4),
                                   abs_p90_pct=_r(S.quantile(moves, 0.9), 4),
                                   source="16:00 close to 09:30 open, nights and weekends together"),
        ),
        venues=dict(phoenix_perp=dict(
            market=ph["market"], taker_fee=ph["taker_fee"], maker_fee=ph["maker_fee"], tick_size=ph["tick_size"],
            base_lots_decimals=ph["base_lots_decimals"],
            trades_per_minute=ph["trades_per_minute_mean"], trades_dispersion_index=ph["trades_per_minute_dispersion_index"],
            trade_size=dict(model="lognormal", median=ph["mean_trade_size_per_minute_lognormal"]["median"],
                            sigma=ph["mean_trade_size_per_minute_lognormal"]["sigma"], unit="base",
                            source="per-minute mean trade size from Phoenix candles"),
            trade_share_by_hour=old["flow"]["trade_share_by_ny_hour"],
            premium_bps=dict(p10=ph["last_trade_vs_mark_bps"]["p10"], median=ph["last_trade_vs_mark_bps"]["median"],
                             p90=ph["last_trade_vs_mark_bps"]["p90"], source="last trade vs mark per minute (not mark vs index)"),
            funding=dict(interval_seconds=ph["funding"]["interval_seconds"], period_seconds=ph["funding"]["period_seconds"],
                         max_rate_per_interval=ph["funding"]["max_rate_per_interval"],
                         max_rate_per_interval_pct=ph["funding"]["max_rate_per_interval_pct"], source="Phoenix /exchange"),
        )),
        book=dict(
            model="defaults",
            half_spread_bps=5.0,
            half_spread_source="default: half the p90 trade-vs-mark gap (9.6 bp); no SPCX book recorded",
            depth_bps=[5.0, 10.0, 25.0, 50.0, 100.0],
            depth_base=[2.0, 5.0, 15.0, 35.0, 80.0],
            depth_source="default; no SPCX book recorded",
            impact=dict(model="power_law", k_bps=1.0, gamma=0.5, size_unit="base", permanent_share=0.5,
                        decay_half_life_seconds=30.0, source="defaults (square-root law); no fill tapes yet"),
        ),
        realism=dict(
            source=f"Phoenix SPCX mark 1 m returns in US regular hours (09:31-16:00 New York), {first} to {last}, "
                   "non-overlapping 30-minute windows of consecutive minutes",
            windows=len(windows),
            bar_seconds=60,
            window_bars=L,
            **p,
            session_kind="regular",
            round_trip_min_losing_share=0.95,
            reference=ref,
            spread=spread(windows, p),
            null=nulls(windows, p),
            bands=BANDS,
            fitted=["vol_level", "vol_of_vol", "kurtosis_within", "predictability", "clustering_lag_1", "clustering_lag_5",
                    "gap_size_median", "gap_size_p90"],
            out_of_fit=["tail_ratio", "clustering_lag_10", "round_trips"],
        ),
    ), inputs


def write(cal: dict, inputs: list[dict], old_id: str) -> Path:
    cid = cal["meta"]["calibration_id"]
    out = OUT / "v1"
    out.mkdir(parents=True, exist_ok=True)
    path = out / f"{cid}.toml"
    header = (f"# Market calibration {cid} (schema 1.0). Written by infra-calib/calib/export_v1.py\n"
              f"# from {old_id}; never edited by hand. A refit is a new file with a new id.\n"
              f"# Every input (url, sha256, bytes, fetch time) is in {cid}.inputs.json.\n\n")
    path.write_text(header + toml_out.dumps(cal) + "\n")
    tomllib.loads(path.read_text())
    (out / f"{cid}.inputs.json").write_text(json.dumps(inputs, indent=1) + "\n")
    return path


def main() -> None:
    for asset, build in (("sol-usd", sol), ("spcx", spcx)):
        old = tomllib.loads((OUT / f"{V01[asset]}.toml").read_text())
        cal, inputs = build(old)
        print(write(cal, inputs, V01[asset]))


if __name__ == "__main__":
    main()

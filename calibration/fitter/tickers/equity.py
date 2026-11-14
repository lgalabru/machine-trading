"""Fit a calibration file for a Phoenix equity perp (NVDA, TSLA, SPY, QQQ) from
its 1 m mark history: the SPCX path in calib/export_v1.py, driven by an asset
config (assets/<asset>.toml) instead of SPCX's constants.

What differs from SPCX, all stated in each file's `limits`:
- the fit window is the market's own history, from its first full week;
- with at least `regime_min_days` of history the volatility level has two
  states (calm, stressed): the lower and upper halves of the days by their
  realized vol with the time-of-day pattern removed; with less it keeps one,
  as SPCX does;
- with fewer than `min_sessions` regular sessions the open gap is SPCX's,
  scaled by the ratio of the two assets' volatility levels;
- the book is the same labelled defaults SPCX has (no book or fill tape has
  been recorded).

`calib/` is the frozen SPCX/SOL fitter and is not edited: its code hash is part
of the shipped files' ids. This module calls its estimators and simulators.
"""

from __future__ import annotations

import hashlib
import json
import math
import random
import tomllib
from collections import defaultdict
from datetime import datetime, timedelta, timezone
from pathlib import Path
from statistics import fmean, median
from zoneinfo import ZoneInfo

from calib import export_v1 as E
from calib import fetch, toml_out
from calib import stats as S

HERE = Path(__file__).resolve().parent
CALIB = HERE.parent / "calib"
MARKET = HERE.parent.parent / "market"
NY = ZoneInfo("America/New_York")
_r = E._r

# Defaults the SPCX file carries (calib/export_v1.py), for a book nobody has recorded.
BOOK_DEFAULTS = dict(half_spread_bps=5.0, depth_bps=[5.0, 10.0, 25.0, 50.0, 100.0], depth_base=[2.0, 5.0, 15.0, 35.0, 80.0])


def code_hash() -> str:
    """SHA-256 over calib/*.py and tickers/*.py, 12 hex."""
    h = hashlib.sha256()
    for d in (CALIB, HERE):
        for p in sorted(d.glob("*.py")):
            h.update(p.read_bytes())
    return h.hexdigest()[:12]


def utc_day(s: str) -> datetime:
    return datetime.fromisoformat(s).replace(tzinfo=timezone.utc)


def pull(cfg: dict) -> list[dict]:
    """The asset's 1 m candles (cached, and recorded in the manifest) and the venue's /exchange."""
    fetch.phoenix_exchange()
    return fetch.phoenix_candles(cfg["symbol"], "1m", utc_day(cfg["pull_from"]), utc_day(cfg["pull_to"]))


def input_names(cfg: dict) -> list[str]:
    m = fetch._manifest()
    sym = cfg["symbol"]
    return sorted(n for n in m if n == "phoenix/exchange.json" or n.startswith(f"phoenix/candles_{sym}_1m_"))


def is_open(t: datetime) -> bool:
    return t.weekday() < 5 and (t.hour, t.minute) >= (9, 31) and t.hour < 16


# --- regimes ---------------------------------------------------------------


def fit_regimes(hours: dict) -> dict:
    """Two states on each day's regular-hours volatility: the median over the
    day's hours (50+ bars each, at least 4 of them) of the log hourly
    realized vol with that clock hour's mean removed, since the model's
    sessions carry no time-of-day shape. Days at or below the median of the
    days are the calm state, the others the stressed one. A two-component
    mixture on these levels (or on the hours) finds no separate component, only
    a scale mixture around one level, so the states split the days in halves;
    what persists is a day's level (the hourly values correlate at about 0.8),
    so the transitions are between consecutive trading days, spread over the
    model's 24 clock hours."""
    keys = [k for k, v in hours.items() if len(v) >= 50]
    lg = {}
    for k in keys:
        rv = math.sqrt(sum(v * v for v in hours[k]))
        if rv > 0:
            lg[k] = math.log(rv)
    by_hour = defaultdict(list)
    for (d, h), v in lg.items():
        by_hour[h].append(v)
    mean_h = {h: fmean(v) for h, v in by_hour.items()}
    by_day = defaultdict(list)
    for (d, h), v in lg.items():
        by_day[d].append(v - mean_h[h])
    days = sorted(d for d, v in by_day.items() if len(v) >= 4)
    level = [median(by_day[d]) for d in days]
    cut = median(level)
    lab = [0 if x <= cut else 1 for x in level]
    mu = [fmean(x for x, y in zip(level, lab) if y == i) for i in range(2)]
    sd = [math.sqrt(fmean((x - mu[i]) ** 2 for x, y in zip(level, lab) if y == i)) for i in range(2)]
    share = [lab.count(i) / len(lab) for i in range(2)]
    T = [[0, 0], [0, 0]]
    for a, b in zip(lab, lab[1:]):
        T[a][b] += 1
    # +1 pseudo-count to each cell: a state never left in a short sample stays a valid row.
    daily = [[(T[i][j] + 1) / (sum(T[i]) + 2) for j in range(2)] for i in range(2)]
    stay = [daily[i][i] ** (1 / 24) for i in range(2)]
    gm = math.exp(share[0] * mu[0] + share[1] * mu[1])
    return dict(vol_mult=[math.exp(mu[0]) / gm, math.exp(mu[1]) / gm], log_sd=sd, share=share,
                transition=[[stay[0], 1 - stay[0]], [1 - stay[1], stay[1]]], daily=daily, days=len(days),
                day_pairs=sum(map(sum, T)), level_sd=S.moments(level)["sd"])


def simulate_mark_reg(df: float, alpha: float, beta: float, half_life: float, reg: dict, k: float, blocks: int,
                      block_seconds: float, sub: int = 12, draws: int = 3000, seed: int = 7) -> list[list[float]]:
    """E.simulate_mark with the two states the Rust model draws: a state by
    share; each state's level its multiplier times a log-normal draw (log-sd
    k times the state's), over the median level of the mixture; the state
    switching after each block at the hourly rate scaled to the block."""
    z0 = random.Random(11)
    big = []
    for _ in range(20000):
        i = 0 if z0.random() < reg["share"][0] else 1
        big.append(reg["vol_mult"][i] * math.exp(k * reg["log_sd"][i] * z0.gauss(0, 1)))
    norm = sorted(big)[len(big) // 2]
    rng = random.Random(seed)
    lam = 1 - 0.5 ** ((block_seconds / sub) / half_life) if half_life > 0 else 1.0
    leave = [(1 - reg["transition"][i][i]) * block_seconds / 3600 for i in range(2)]
    out = []
    for _ in range(draws):
        r = 0 if rng.random() < reg["share"][0] else 1
        lv = [reg["vol_mult"][i] * math.exp(k * reg["log_sd"][i] * rng.gauss(0, 1)) / norm for i in range(2)]
        h, p, m, w = 1.0, 0.0, 0.0, []
        for _ in range(blocks):
            level = lv[r]
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
            if rng.random() < leave[r]:
                r = 1 - r
        out.append(w)
    return out


def fit_mark_shape_reg(ref: dict, p: dict, persistence: float, df0: float, blocks: int, block_seconds: float, reg: dict) -> dict:
    """E.fit_mark_shape with the regimes held at their fitted shares, multipliers
    and transitions: alpha, persistence, the mark's half-life and df as there,
    then a common scale k on the states' log-sds to the windows' vol of vol."""
    def stats(df, alpha, hl, pers, k=1.0, draws=3000):
        return E.window_stats(simulate_mark_reg(df, alpha, pers - alpha, hl, reg, k, blocks, block_seconds, draws=draws), p)

    i5 = p["cluster_lags"].index(5)
    df, alpha, hl = df0, 0.1, 2.0
    for _ in range(3):
        def lag5(pers: float) -> float:
            a = E.bisect(lambda a_: stats(df, a_, hl, pers, draws=2000)["clustering"][0], ref["clustering"][0], 0.01, pers - 0.01, True, steps=9)
            return stats(df, a, hl, pers, draws=2000)["clustering"][i5]

        persistence = E.bisect(lag5, ref["clustering"][i5], 0.5, 0.999, True, steps=9)
        alpha = E.bisect(lambda a: stats(df, a, hl, persistence)["clustering"][0], ref["clustering"][0], 0.01, persistence - 0.01, True)
        hl = E.bisect(lambda x: stats(df, alpha, x, persistence)["predictability"], ref["predictability"], 0.05, 120.0, True, log=True)
        df = E.bisect(lambda d: stats(d, alpha, hl, persistence)["kurtosis_within"], ref["kurtosis_within"], 2.2, 60.0, False, log=True)
    k = E.bisect(lambda k_: stats(df, alpha, hl, persistence, k_)["vol_of_vol"], ref["vol_of_vol"], 0.05, 4.0, True)
    ws = simulate_mark_reg(df, alpha, persistence - alpha, hl, reg, k, blocks, block_seconds)
    return dict(df=df, alpha=alpha, beta=persistence - alpha, persistence=persistence, half_life=hl, sigma=k,
                bias=median(S.moments(w)["sd"] for w in ws), fitted=E.window_stats(ws, p))


# --- the fit ---------------------------------------------------------------


def fit(cfg: dict, fitted_at: str, fit_date: str) -> tuple[dict, list[dict]]:
    sym, asset = cfg["symbol"], cfg["asset"]
    fit_from, fit_to = utc_day(cfg["fit_from"]), utc_day(cfg["pull_to"])
    all_c = pull(cfg)
    c = [k for k in all_c if fit_from.timestamp() * 1000 <= k["time"] < fit_to.timestamp() * 1000]
    ex = fetch.phoenix_exchange()
    mk = next(m for m in ex["markets"] if m["symbol"] == sym)

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
    jumps = S.jumps(r_open)
    open_sd, closed_sd = S.moments(r_open)["sd"], S.moments(closed)["sd"]
    closed_mult = closed_sd / open_sd

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

    L = 30
    windows = [r[i:i + L] for r in runs for i in range(0, len(r) - L + 1, L)]
    p = dict(vol_bars=1, agg_bars=1, cluster_bars=1, cluster_lags=[1, 5, 10])
    ref = E.window_stats(windows, p)
    days = (fit_to - fit_from).days
    sessions_n = len({d for d, _ in hours})
    two_state = days >= cfg["regime_min_days"]
    if two_state:
        reg = fit_regimes(hours)
        shape = fit_mark_shape_reg(ref, p, g["alpha"] + g["beta"], g["student_t_df"], L, 60.0, reg)
    else:
        reg = None
        shape = E.fit_mark_shape(ref, p, g["alpha"] + g["beta"], g["student_t_df"], L, 60.0)
    df, bias = shape["df"], shape["bias"]
    print(f"{asset} shape fit:", {k: v for k, v in shape.items() if k != "fitted"}, flush=True)
    log_sd_hourly = S.moments([math.log(v) for v in (math.sqrt(sum(x * x for x in h)) for h in hours.values() if len(h) >= 50) if v > 0])["sd"]

    # The open gap: fitted, or scaled from SPCX's when the history is short.
    first, last = fit_from.date().isoformat(), (fit_to - timedelta(days=1)).date().isoformat()
    own = dict(n=len(gaps), abs_median_pct=median(gaps), abs_p90_pct=S.quantile(gaps, 0.9))
    if len(gaps) >= cfg["min_sessions"]:
        gap = dict(into="regular", n=own["n"], abs_median_pct=_r(own["abs_median_pct"], 4), abs_p90_pct=_r(own["abs_p90_pct"], 4),
                   source=f"1 m mark move at the 09:30 New York open, {own['n']} opens")
        gap_limit = f"The open gap is fitted on {own['n']} opens."
    else:
        base_path = MARKET / f"{cfg['gap_fallback']}.toml"
        base = tomllib.loads(base_path.read_text())
        ratio = ref["vol_level"] / base["realism"]["reference"]["vol_level"]
        b = base["calendar"]["gaps"][0]
        gap = dict(into="regular", n=0, abs_median_pct=_r(b["abs_median_pct"] * ratio, 4), abs_p90_pct=_r(b["abs_p90_pct"] * ratio, 4),
                   source=(f"scaled from {cfg['gap_fallback']} (file SHA-256 {hashlib.sha256(base_path.read_bytes()).hexdigest()[:12]}): its fitted open gap "
                           f"(median {b['abs_median_pct']}%, p90 {b['abs_p90_pct']}%) times {ratio:.3f}, the ratio of the two assets' median 30-minute "
                           f"1 m volatility; this asset has only {own['n']} opens (median {own['abs_median_pct']:.3f}%, p90 {own['abs_p90_pct']:.3f}%)"))
        gap_limit = (f"The open gap is {cfg['gap_fallback']}'s scaled by the volatility ratio {ratio:.3f}, not fitted: only {own['n']} regular "
                     f"sessions of history (under {cfg['min_sessions']}).")

    names = input_names(cfg)
    inputs = fetch.provenance(names)
    digest = hashlib.sha256("".join(i["sha256"] for i in inputs).encode()).hexdigest()[:16]
    code = code_hash()
    version = hashlib.sha256((digest + code).encode()).hexdigest()[:8]
    cid = f"{asset}.{fit_date}.{version}"

    v_from, v_to = utc_day(cfg["venue_from"]), fit_to
    vc = [k for k in all_c if v_from.timestamp() * 1000 <= k["time"] < v_to.timestamp() * 1000]
    traded = [k for k in vc if k["tradeCount"] > 0]
    tpm = [k["tradeCount"] for k in vc]
    mean_tpm = fmean(tpm)
    disp = S.moments([float(x) for x in tpm])["sd"] ** 2 / mean_tpm
    lsz = [math.log(s) for s in (k["volume"] / k["tradeCount"] for k in traded) if s > 0]
    basis = [(k["close"] - k["markClose"]) / k["markClose"] * 1e4 for k in traded if k["markClose"]]
    hourly = defaultdict(int)
    for k in vc:
        hourly[datetime.fromtimestamp(k["time"] / 1000, NY).hour] += k["tradeCount"]
    tot = sum(hourly.values()) or 1
    v_first, v_last = v_from.date().isoformat(), (v_to - timedelta(days=1)).date().isoformat()

    sessions = [
        dict(kind="pre", days=["mon", "tue", "wed", "thu", "fri"], start="04:00", end="09:30", vol_mult=_r(closed_mult, 4)),
        dict(kind="regular", days=["mon", "tue", "wed", "thu", "fri"], start="09:30", end="16:00", vol_mult=1.0),
        dict(kind="post", days=["mon", "tue", "wed", "thu", "fri"], start="16:00", end="20:00", vol_mult=_r(closed_mult, 4)),
        dict(kind="overnight", days=["sun", "mon", "tue", "wed", "thu"], start="20:00", end="04:00", vol_mult=_r(closed_mult, 4)),
    ]
    if two_state:
        regimes = dict(
            model="markov", names=["calm", "stressed"],
            vol_mult=[_r(x, 4) for x in reg["vol_mult"]],
            log_sd_within=[_r(shape["sigma"] * x, 4) for x in reg["log_sd"]],
            share=[_r(x, 4) for x in reg["share"]],
            transition_per_hour=[[_r(x, 4) for x in row] for row in reg["transition"]],
            source=(f"two states, the lower and upper halves of {reg['days']} trading days by the median over each day's regular hours of "
                    f"the log hourly realized vol (hours with 50+ bars) with each clock hour's mean removed (a mixture on these finds no separate "
                    f"component); shares from the halves; the daily transitions of {reg['day_pairs']} consecutive-day pairs (+1 pseudo-count a cell), "
                    f"daily stay chances {reg['daily'][0][0]:.3f} and {reg['daily'][1][1]:.3f}, as hourly rates (the 24th root); multipliers relative to "
                    f"their share-weighted geometric mean; the within-state log-sds are the days' own spread in each state times {shape['sigma']:.3f}, by "
                    f"simulated moments to the realism windows' vol of vol ({ref['vol_of_vol']}), drawn as check_model draws (30-minute "
                    f"regular-session sessions, GARCH from its target)"))
        regime_limit = (f"Two volatility states on {reg['days']} trading days: halves of the days by their level, not separate clusters (the levels are "
                        f"unimodal), so they carry day-to-day calm and stress, not the time-of-day shape (that stays in the within-state spread); the "
                        f"hourly switching rates are the daily ones spread over 24 hours.")
    else:
        regimes = dict(
            model="markov", names=["base"], vol_mult=[1.0], log_sd_within=[_r(shape["sigma"], 4)], share=[1.0], transition_per_hour=[[1.0]],
            source=(f"one state: only {days} days of history (under {cfg['regime_min_days']}); the session level's log-sd by simulated moments to the "
                    f"realism windows' vol of vol ({ref['vol_of_vol']}), drawn as check_model draws (30-minute regular-session sessions, GARCH from "
                    f"its target); the sd of log hourly realized vol is {log_sd_hourly:.3f}"))
        regime_limit = f"One volatility state: {days} days of history is too short to fit two, so there is no calm/stressed switching."

    ph = dict(
        market=mk["marketPubkey"], taker_fee=mk["takerFee"], maker_fee=mk["makerFee"], tick_size=mk["tickSize"], base_lots_decimals=mk["baseLotsDecimals"],
        trades_per_minute=_r(mean_tpm, 4), trades_dispersion_index=_r(disp, 4),
        trade_size=dict(model="lognormal", median=_r(math.exp(median(lsz)), 4), sigma=_r(S.moments(lsz)["sd"], 3), unit="base",
                        source="per-minute mean trade size from Phoenix candles"),
        trade_share_by_hour=[_r(hourly[h] / tot, 3) for h in range(24)],
        premium_bps=dict(p10=_r(S.quantile(basis, 0.1), 3), median=_r(median(basis), 3), p90=_r(S.quantile(basis, 0.9), 3),
                         source="last trade vs mark per minute (not mark vs index)"),
        funding=dict(interval_seconds=mk["fundingIntervalSeconds"], period_seconds=mk["fundingPeriodSeconds"],
                     max_rate_per_interval=mk["maxFundingRatePerInterval"],
                     max_rate_per_interval_pct=_r(mk["maxFundingRatePerIntervalPercentage"], 4), source="Phoenix /exchange"),
    )
    fills = sum(k["tradeCount"] for k in all_c)
    cal = dict(
        meta=dict(
            schema_version="1.0", calibration_id=cid, asset=asset, symbol=sym, fitted_at=fitted_at, exported_at=fitted_at,
            fitter="calibration/fitter (tickers/equity.py on calib/)", fitter_code_sha256_12=code,
            inputs_file=f"{cid}.inputs.json", inputs_digest=digest, inputs_count=len(inputs),
            sources=[f"Phoenix perp API /exchange and /candles ({sym} mark, 1 m, since its first full week)", "NYSE holiday calendar"],
            limits=[
                f"Fair value is Phoenix's {sym} mark (the oracle plus the perp's premium), {first} to {last}, not the underlying's tape. "
                "Pyth's Equity.US history is not keyless (benchmarks.pyth.network answers 404 and Hermes needs a key), so no longer underlying history was used.",
                f"The market's history is {days} days from its first full week ({sessions_n} regular sessions, {len(r_open)} regular-hours 1 m returns).",
                regime_limit,
                gap_limit,
                (f"Only regular hours vs everything else is fitted: pre, post, overnight and weekend share the closed-hours volatility "
                 f"({closed_mult:.3f} of the regular-hours 1 m sd)."
                 + (" That is above 1: Phoenix's mark moves more per minute overnight than in regular hours (the weekend is the quiet part)."
                    if closed_mult > 1 else "")),
                *([f"The GARCH persistence ({shape['persistence']:.4f}) is at the top of the search: the |r| ACF at lags 5 and 10 inside 30-minute windows "
                   "is slightly negative (a short-window bias), so persistence is weakly identified; vol_of_vol carries the clustering across sessions."]
                  if shape["persistence"] > 0.99 else []),
                *([f"The mark's half-life ({shape['half_life']:.3f} s) is at the bottom of the search: the real lag-1 return ACF ({ref['predictability']}) "
                   "is negative, which an exponential-average mark can't give, so the mark is effectively unsmoothed."]
                  if shape["half_life"] < 0.1 else []),
                ("Fitted to the realism windows themselves (in-sample checks): per_second_sd (vol_level), df (kurtosis_within), the level's log-sd "
                 "(vol_of_vol), GARCH alpha and persistence (clustering_lag_1 and _5) and the mark's half-life (predictability); the gap sizes are "
                 "the open gaps above (gap_size). The realism windows are the history the model is fitted on."),
                "Out of fit: tail_ratio, clustering_lag_10 and round_trips.",
                f"Venue numbers (trades, sizes, premium) come from 30 days ({v_first} to {v_last}); fees, tick, lots and funding caps are /exchange as of the pull.",
                f"Book depth and impact are defaults: no {sym} book or fill tape has been recorded ({fills} trades in the candles over the whole pull, "
                "not signed fills).",
                "Realism bands are each statistic's sampling spread over these windows; other periods would add period-to-period spread.",
            ],
            windows=dict(phoenix_fair_value=[first, last], phoenix_venue=[v_first, v_last]),
        ),
        fair_value=dict(
            model="regime_garch_t",
            reference=f"Phoenix {sym} mark (tracks Pyth Equity.US.{sym}/USD plus the perp's premium)",
            per_second_sd=_r(ref["vol_level"] / math.sqrt(60) / bias),
            per_second_sd_source=(f"the median session's level: the realism windows' median 1 m sd / sqrt(60), over {bias:.4f}, "
                                  f"the median window sd of the model's own {L}-block windows at unit median level (simulated)"),
            block_seconds=60,
            tails=dict(
                model="student_t", df=_r(df, 4),
                df_source=(f"simulated moments: the df at which the model's 30-minute windows of 1 m mark returns match the realism windows' "
                           f"within-window kurtosis ({ref['kurtosis_within']}); the GARCH-standardized estimate is {g['student_t_df']:.3f}"),
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
            regimes=regimes,
            jumps=dict(model="poisson_lognormal", per_hour=_r(jumps["per_bar"] * 60, 4),
                       abs_size_median_pct=_r(jumps["abs_size_median"] * 100, 4), abs_size_p90_pct=_r(jumps["abs_size_p90"] * 100, 4),
                       rule="1 m regular-hours return beyond 5 robust sigmas of the trailing hour"),
        ),
        calendar=dict(
            model="sessions", timezone="America/New_York", sessions=sessions,
            closed_vol_mult=_r(closed_mult, 4),
            closed_source="1 m mark sd outside regular hours over the regular-hours sd; weekends and holidays are closed",
            holidays=E.HOLIDAYS, draw_range=["2027-01-04", "2027-12-31"], gaps=[gap],
            closed_hours_move=dict(n=len(moves), abs_median_pct=_r(median(moves), 4), abs_p90_pct=_r(S.quantile(moves, 0.9), 4),
                                   source="16:00 close to 09:30 open, nights and weekends together"),
        ),
        venues=dict(phoenix_perp=ph),
        book=dict(
            model="defaults",
            half_spread_bps=BOOK_DEFAULTS["half_spread_bps"],
            half_spread_source=f"default, as for spcx: no {sym} book recorded",
            depth_bps=BOOK_DEFAULTS["depth_bps"], depth_base=BOOK_DEFAULTS["depth_base"],
            depth_source=f"default, as for spcx (in base units, not scaled to {sym}'s price); no {sym} book recorded",
            impact=dict(model="power_law", k_bps=1.0, gamma=0.5, size_unit="base", permanent_share=0.5, decay_half_life_seconds=30.0,
                        source="defaults (square-root law); no fill tapes yet"),
        ),
        realism=dict(
            source=f"Phoenix {sym} mark 1 m returns in US regular hours (09:31-16:00 New York), {first} to {last}, "
                   "non-overlapping 30-minute windows of consecutive minutes",
            windows=len(windows), bar_seconds=60, window_bars=L, **p, session_kind="regular", round_trip_min_losing_share=0.95,
            reference=ref, spread=E.spread(windows, p), null=E.nulls(windows, p), bands=E.BANDS,
            fitted=["vol_level", "vol_of_vol", "kurtosis_within", "predictability", "clustering_lag_1", "clustering_lag_5",
                    "gap_size_median", "gap_size_p90"],
            out_of_fit=["tail_ratio", "clustering_lag_10", "round_trips"],
        ),
    )
    return cal, inputs


def write(cal: dict, inputs: list[dict], out: Path) -> Path:
    cid = cal["meta"]["calibration_id"]
    out.mkdir(parents=True, exist_ok=True)
    path = out / f"{cid}.toml"
    header = (f"# Market calibration {cid} (schema 1.0). Written by calibration/fitter (tickers/equity.py)\n"
              f"# from the asset's config; never edited by hand. A refit is a new file with a new id.\n"
              f"# Every input (url, sha256, bytes, fetch time) is in {cid}.inputs.json.\n\n")
    path.write_text(header + toml_out.dumps(cal) + "\n")
    tomllib.loads(path.read_text())
    (out / f"{cid}.inputs.json").write_text(json.dumps(inputs, indent=1) + "\n")
    return path

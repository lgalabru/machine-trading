"""Fit a calibration (a plain dict, written as TOML by toml_out) from the
sources. Engine sessions last 480-800 slots (~3-5 minutes at 0.4 s a slot),
so the parameters that drive a session are the seconds-scale ones; the
minute/hour-scale ones set regimes and draw session-level volatility."""

from __future__ import annotations

import hashlib
import math
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path
from statistics import fmean, median

from . import fetch, sources
from . import stats as S

SLOT_S = 0.4
SECONDS_PER_YEAR = 365.25 * 86400
SCHEMA_VERSION = "0.1"
HORIZONS_S = [1, 10, 60, 300, 3600]


def code_hash() -> str:
    h = hashlib.sha256()
    for p in sorted((Path(__file__).parent).glob("*.py")):
        h.update(p.read_bytes())
    return h.hexdigest()[:12]


def _r(x: float, nd: int = 6) -> float:
    if x is None or (isinstance(x, float) and not math.isfinite(x)):
        return float("nan")
    return float(f"{x:.{nd}g}")


def per_second_returns(days: list[list[dict]]) -> list[list[float]]:
    """1 s log returns per day (no returns across day files)."""
    return [S.log_returns([k["c"] for k in d]) for d in days]


def returns_block(r1: list[float], bar_s: int) -> dict:
    """Moments, tails and clustering across horizons from bar_s-second returns."""
    out = {}
    for h in HORIZONS_S:
        if h < bar_s or h % bar_s:
            continue
        rh = S.aggregate(r1, h // bar_s) if h > bar_s else r1
        if len(rh) < 200:
            continue
        m = S.moments(rh)
        out[f"{h}s"] = dict(
            n=m["n"],
            sd=_r(m["sd"]),
            vol_annualized=_r(m["sd"] * math.sqrt(SECONDS_PER_YEAR / h), 4),
            skew=_r(m["skew"], 3),
            excess_kurtosis=_r(m["excess_kurtosis"], 4),
            hill_alpha=_r(S.hill_tail_index(rh)["alpha"], 3),
            zero_share=_r(sum(1 for v in rh if v == 0) / len(rh), 3),
        )
    return out


def fit_sol(holdout_days: int = 2) -> tuple[dict, dict]:
    days = sources.sol_1s()
    fit_days, hold_days = days[:-holdout_days], days[-holdout_days:]
    r1_days = per_second_returns(fit_days)
    r1 = [v for d in r1_days for v in d]
    m1 = sources.sol_1m()
    r60 = S.log_returns([k["c"] for k in m1])

    # Seconds scale (sessions).
    sec = returns_block(r1, 1)
    r10 = [v for d in r1_days for v in S.aggregate(d, 10)]
    g10 = S.garch11(r10)
    acf_abs_10 = S.acf([abs(v) for v in r10], [1, 2, 5, 10, 30, 60])
    acf_r_1 = S.acf(r1, [1, 2, 3])
    acf_r_10 = S.acf(r10, [1, 2, 3])
    sd1 = sec["1s"]["sd"]
    per_slot_sd = sd1 * math.sqrt(SLOT_S)

    # Minutes-hours scale (regimes, intraday shape, jumps).
    mins = returns_block(r60, 60)
    r300 = S.aggregate(r60, 5)
    g5 = S.garch11(r300)
    hourly_rv = [math.sqrt(sum(v * v for v in r60[i:i + 60])) for i in range(0, len(r60) - 59, 60)]
    reg = S.two_regimes(hourly_rv)
    mean_rv = fmean(hourly_rv)
    by_hour: dict[int, list[float]] = {h: [] for h in range(24)}
    for k, v in zip(m1[1:], r60):
        by_hour[datetime.fromtimestamp(k["t"], timezone.utc).hour].append(v * v)
    hv = {h: math.sqrt(fmean(x)) for h, x in by_hour.items()}
    hv_mean = fmean(hv.values())
    j = S.jumps(r60, window=60, k=5.0)
    alpha_ci = S.bootstrap_ci(r60[-60 * 24 * 30:], lambda x: S.hill_tail_index(x)["alpha"], n=60, block=240)
    kurt_ci = S.bootstrap_ci(r60[-60 * 24 * 30:], lambda x: S.moments(x)["excess_kurtosis"], n=60, block=240)

    # Microstructure from 1 s CEX bars: activity and its clustering.
    trades = [k["trades"] for d in fit_days for k in d]
    tps = fmean(trades)
    disp = (S.moments([float(t) for t in trades])["sd"] ** 2) / tps if tps else float("nan")
    size = [k["vol"] / k["trades"] for d in fit_days for k in d if k["trades"] > 0]
    lsize = [math.log(s) for s in size if s > 0]
    buy_share = [k["taker_buy"] / k["vol"] for d in fit_days for k in d if k["vol"] > 0]
    imbalance_acf = S.acf([b - 0.5 for b in buy_share], [1, 5, 30])
    roll = [S.roll_spread([k["c"] for k in d[i:i + 3600]]) for d in fit_days for i in range(0, 86400, 3600)]
    roll = [x for x in roll if x is not None]

    # Venue: Phoenix SOL perp (on-chain CLOB) from its public candles.
    ph = venue_block("SOL")
    # Aggregated DEX liquidity: Jupiter's routed price by size.
    jc = sources.jupiter_curve()
    liq = impact_curve(jc)
    fees = priority_fee_block()
    lst = sources.jitosol()
    hist = sources.jitosol_history()
    lr = S.log_returns([h["rate"] for h in hist])
    lst_drift = dict(days=len(hist), from_date=hist[0]["date"], to_date=hist[-1]["date"],
                     rate_first=_r(hist[0]["rate"], 8), rate_last=_r(hist[-1]["rate"], 8),
                     daily_growth_mean_bps=_r(fmean(lr) * 1e4, 4), daily_growth_sd_bps=_r(S.moments(lr)["sd"] * 1e4, 4),
                     annualized=_r(math.exp(fmean(lr) * 365) - 1, 4),
                     note="daily buckets straddle epochs (~2 days), so per-day growth is lumpy; fit per epoch in production")

    cal = dict(
        meta=meta("sol-usd", ["SOLUSDT 1s (Binance public bucket)", "SOLUSDT 1m", "Phoenix perp candles", "Jupiter quotes", "Solana RPC"],
                  windows=dict(seconds_fit=[d.isoformat() for d in fetch.days(sources.SOL_1S_END, sources.SOL_1S_DAYS)[:-holdout_days]],
                               seconds_holdout=[d.isoformat() for d in fetch.days(sources.SOL_1S_END, sources.SOL_1S_DAYS)[-holdout_days:]],
                               minutes=sources.SOL_1M_MONTHS,
                               phoenix=[sources.PHOENIX_START.date().isoformat(), sources.PHOENIX_END.date().isoformat()])),
        fair_value=dict(
            reference="CEX mid proxy (Binance SOLUSDT); Pyth SOL/USD aggregates the same venues. Swap in Pyth history when a Pyth Pro key exists.",
            per_second_sd=_r(sd1),
            per_slot_sd=_r(per_slot_sd),
            vol_annualized=_r(mins["60s"]["vol_annualized"], 4),
            horizons_seconds=sec,
            horizons_minutes=mins,
            tails=dict(
                model="student_t_innovations",
                student_t_df_10s=_r(g10["student_t_df"], 3),
                student_t_df_5m=_r(g5["student_t_df"], 3),
                hill_alpha_1m=_r(mins["60s"]["hill_alpha"], 3),
                hill_alpha_1m_ci90=[_r(alpha_ci[0], 3), _r(alpha_ci[1], 3)],
                excess_kurtosis_1m_ci90=[_r(kurt_ci[0], 3), _r(kurt_ci[1], 3)],
            ),
            vol_clustering=dict(
                model="garch11",
                at_10s=dict(alpha=_r(g10["alpha"], 4), beta=_r(g10["beta"], 4), persistence=_r(g10["persistence"], 5),
                            half_life_seconds=_r(g10["half_life_bars"] * 10, 4)),
                at_5m=dict(alpha=_r(g5["alpha"], 4), beta=_r(g5["beta"], 4), persistence=_r(g5["persistence"], 5),
                           half_life_seconds=_r(g5["half_life_bars"] * 300, 4)),
                acf_abs_10s={f"lag_{k}": _r(v, 3) for k, v in acf_abs_10.items()},
                acf_returns_1s={f"lag_{k}": _r(v, 3) for k, v in acf_r_1.items()},
                acf_returns_10s={f"lag_{k}": _r(v, 3) for k, v in acf_r_10.items()},
            ),
            regimes=dict(
                model="two_state_markov_on_hourly_rv",
                calm_vol_mult=_r(reg["calm_vol_mult"] / mean_rv, 3),
                stressed_vol_mult=_r(reg["stressed_vol_mult"] / mean_rv, 3),
                log_sd_within=[_r(x, 3) for x in reg["log_sd"]],
                share=[_r(x, 3) for x in reg["share"]],
                transition_per_hour=[[_r(x, 3) for x in row] for row in reg["transition"]],
                mean_dwell_hours=[_r(x, 3) for x in reg["mean_dwell_blocks"]],
            ),
            intraday_vol_mult_utc=[_r(hv[h] / hv_mean, 3) for h in range(24)],
            jumps=dict(
                rule="1m return beyond 5 robust sigmas of the trailing hour",
                per_hour=_r(j["per_bar"] * 60, 3),
                abs_size_median_pct=_r(j["abs_size_median"] * 100, 3),
                abs_size_p90_pct=_r(j["abs_size_p90"] * 100, 3),
            ),
        ),
        flow=dict(
            source="CEX 1s bars (trades are CEX prints, not Solana fills: shape only, rescale by venue activity)",
            trades_per_second_mean=_r(tps, 4),
            trades_per_second_dispersion_index=_r(disp, 4),
            mean_trade_size_per_second_lognormal=dict(median=_r(math.exp(median(lsize)), 4), sigma=_r(S.moments(lsize)["sd"], 3)),
            taker_buy_share_acf={f"lag_{k}s": _r(v, 3) for k, v in imbalance_acf.items()},
            roll_spread_bps_median=_r(median(roll) * 1e4, 3) if roll else float("nan"),
        ),
        venues=dict(phoenix_perp=ph),
        liquidity=dict(jupiter_routed_sol_usdc=liq),
        tx_fees=fees,
        lst=dict(jitosol=dict(pool_rate_sol=_r(lst["pool_rate_sol"], 8), market_rate_sol=_r(lst["market_rate_sol"], 8),
                              market_discount_bps=_r(lst["discount_bps"], 3), epoch=lst["last_update_epoch"], slot=lst["slot"],
                              note="one snapshot of the market discount; depeg tails need repeated quotes (see CALIBRATION.md)"),
                 jitosol_exchange_rate_drift=lst_drift),
        engine_mapping=engine_mapping(per_slot_sd, reg, mean_rv, j),
    )
    return cal, dict(holdout_days=hold_days, fit_r1=r1)


def venue_block(symbol: str) -> dict:
    ex = fetch.phoenix_exchange()
    mk = next(m for m in ex["markets"] if m["symbol"] == symbol)
    c = sources.phoenix(symbol)
    traded = [k for k in c if k["tradeCount"] > 0]
    tpm = [k["tradeCount"] for k in c]
    mean_tpm = fmean(tpm)
    disp = S.moments([float(x) for x in tpm])["sd"] ** 2 / mean_tpm if mean_tpm else float("nan")
    sz = [k["volume"] / k["tradeCount"] for k in traded]
    lsz = [math.log(s) for s in sz if s > 0]
    basis = [(k["close"] - k["markClose"]) / k["markClose"] * 1e4 for k in traded if k["markClose"]]
    mark_r = S.log_returns([k["markClose"] for k in c])
    mm = S.moments(mark_r)
    sigs = sources.market_activity(symbol)
    span_slots = sigs[0]["slot"] - sigs[-1]["slot"]
    return dict(
        market=mk["marketPubkey"],
        taker_fee=mk["takerFee"], maker_fee=mk["makerFee"], tick_size=mk["tickSize"], base_lots_decimals=mk["baseLotsDecimals"],
        funding=dict(interval_seconds=mk["fundingIntervalSeconds"], period_seconds=mk["fundingPeriodSeconds"],
                     max_rate_per_interval=mk["maxFundingRatePerInterval"],
                     max_rate_per_interval_pct=_r(mk["maxFundingRatePerIntervalPercentage"], 4)),
        trades_per_minute_mean=_r(mean_tpm, 4),
        trades_per_minute_dispersion_index=_r(disp, 4),
        minutes_with_trades_share=_r(len(traded) / len(c), 3),
        mean_trade_size_per_minute_lognormal=dict(median=_r(math.exp(median(lsz)), 4) if lsz else float("nan"),
                                                  sigma=_r(S.moments(lsz)["sd"], 3) if len(lsz) > 2 else float("nan")),
        last_trade_vs_mark_bps=dict(p10=_r(S.quantile(basis, 0.1), 3), median=_r(median(basis), 3), p90=_r(S.quantile(basis, 0.9), 3),
                                    abs_median=_r(median([abs(b) for b in basis]), 3)) if basis else {},
        mark_returns_1m=dict(sd=_r(mm["sd"]), excess_kurtosis=_r(mm["excess_kurtosis"], 4),
                             hill_alpha=_r(S.hill_tail_index(mark_r)["alpha"], 3),
                             acf_abs={f"lag_{k}": _r(v, 3) for k, v in S.acf([abs(v) for v in mark_r], [1, 5, 30]).items()}),
        txs_per_slot_touching_market=_r(len(sigs) / max(1, span_slots), 4),
        candles=dict(n=len(c), first_ms=c[0]["time"], last_ms=c[-1]["time"]),
    )


def impact_curve(jc: dict) -> dict:
    """Cost vs size from routed quotes, against the 1-SOL price of the same
    side. Fit cost_bps = k * size^gamma on the sizes with positive cost."""
    out = {}
    for side in ("sell_sol", "buy_sol"):
        rows = jc[side]
        p0 = rows[0]["px"]
        pts = []
        for r in rows:
            cost = (p0 - r["px"]) / p0 * 1e4 if side == "sell_sol" else (r["px"] - p0) / p0 * 1e4
            pts.append(dict(size_sol=r["size_sol"], cost_bps_vs_1sol=_r(cost, 4), jup_impact_pct=_r(r["impact_pct"], 4)))
        fit = [(math.log(p["size_sol"]), math.log(p["cost_bps_vs_1sol"])) for p in pts if p["cost_bps_vs_1sol"] > 0.05]
        if len(fit) >= 2:
            mx = fmean(a for a, _ in fit)
            my = fmean(b for _, b in fit)
            g = sum((a - mx) * (b - my) for a, b in fit) / sum((a - mx) ** 2 for a, _ in fit)
            k = math.exp(my - g * mx)
        else:
            g = k = float("nan")
        out[side] = dict(points=pts, power_law=dict(k_bps=_r(k, 4), gamma=_r(g, 3)))
    out["quoted_spread_bps_1sol"] = _r((jc["buy_sol"][0]["px"] - jc["sell_sol"][0]["px"]) / jc["sell_sol"][0]["px"] * 1e4, 3)
    out["note"] = ("single snapshot; buy and sell quotes are seconds apart so the 1-SOL spread can read negative. "
                   "Production: sample paired quotes every few minutes for a week and fit quantiles.")
    return out


def priority_fee_block() -> dict:
    pf = sources.priority_fees()
    v = [x["prioritizationFee"] for x in pf]
    nz = [x for x in v if x > 0]
    tip = sources.jito_tip_floor()
    return dict(
        jito_landed_tip_sol=dict(p25=tip["landed_tips_25th_percentile"], p50=tip["landed_tips_50th_percentile"],
                                 p75=tip["landed_tips_75th_percentile"], p95=tip["landed_tips_95th_percentile"],
                                 p99=tip["landed_tips_99th_percentile"], at=tip["time"]),
        source="getRecentPrioritizationFees over the Phoenix SPCX and SOL market accounts (micro-lamports per CU, per-slot minimum)",
        slots=len(v), share_slots_nonzero=_r(len(nz) / len(v), 3) if v else float("nan"),
        p50=S.quantile(v, 0.5) if v else 0, p90=S.quantile(v, 0.9) if v else 0, p99=S.quantile(v, 0.99) if v else 0,
        note="per-slot minimum understates what landing txs pay; production uses per-tx fees from getBlock samples",
    )


def engine_mapping(per_slot_sd: float, reg: dict, mean_rv: float, j: dict) -> dict:
    """What today's hand-set family constants would be if set from this fit.
    MakingFamily::spcx draws step_vol_pct in (0.02, 0.08) per step with
    step_gap (3, 9) slots."""
    gaps = (3, 9)
    lo = per_slot_sd * math.sqrt(gaps[0]) * 100
    hi = per_slot_sd * math.sqrt(gaps[1]) * 100
    calm = reg["calm_vol_mult"] / mean_rv
    stressed = reg["stressed_vol_mult"] / mean_rv
    return dict(
        making_step_vol_pct_calibrated=[_r(lo * calm, 3), _r(hi * stressed, 3)],
        making_step_vol_pct_current=[0.02, 0.08],
        making_jump_pct_current=[1.5, 4.0],
        jump_abs_pct_median_calibrated=_r(j["abs_size_median"] * 100, 3),
        jumps_per_session_calibrated=_r(j["per_bar"] * 60 * (640 * SLOT_S / 3600), 4),
        note=("per-step sd = per-slot sd * sqrt(gap slots) * regime multiplier. Jumps in "
              "families are scenario events (designed shocks), not background; the background rate shows how rare they are."),
    )


def fit_spcx() -> dict:
    """SPCX: only Phoenix's own candles are keyless. Pyth's Equity.US.SPCX/USD
    history needs a Pyth Pro key; the feed is listed (ids in CALIBRATION.md)."""
    ph = venue_block("SPCX")
    c = sources.phoenix("SPCX")
    # Split mark returns by the equity's session: open 09:30-16:00 New York.
    from zoneinfo import ZoneInfo

    ny = ZoneInfo("America/New_York")
    open_r, closed_r = [], []
    for a, b in zip(c, c[1:]):
        if not (a["markClose"] and b["markClose"]):
            continue
        r = math.log(b["markClose"] / a["markClose"])
        t = datetime.fromtimestamp(b["time"] / 1000, ny)
        is_open = t.weekday() < 5 and (t.hour, t.minute) >= (9, 31) and t.hour < 16
        (open_r if is_open else closed_r).append(r)
    blk = {}
    for name, r in (("us_session_open", open_r), ("us_session_closed", closed_r)):
        m = S.moments(r)
        blk[name] = dict(n=m["n"], sd_1m=_r(m["sd"]), vol_annualized_equiv=_r(m["sd"] * math.sqrt(SECONDS_PER_YEAR / 60), 4),
                         excess_kurtosis=_r(m["excess_kurtosis"], 4), hill_alpha=_r(S.hill_tail_index(r)["alpha"], 3),
                         zero_share=_r(sum(1 for v in r if v == 0) / len(r), 3))
    # The open: the 1 m mark move at 09:30 New York (the oracle reopening)
    # and the move over the closed hours before it (16:00 -> 09:30).
    by_t = {k["time"]: k for k in c}
    open_jump, closed_move = [], []
    for k in c:
        t = datetime.fromtimestamp(k["time"] / 1000, ny)
        if t.weekday() < 5 and (t.hour, t.minute) == (9, 30):
            prev = by_t.get(k["time"] - 60_000)
            if prev and prev["markClose"] and k["markClose"]:
                open_jump.append(math.log(k["markClose"] / prev["markClose"]))
            # last 16:00 bar before this open
            back = k["time"] - 60_000
            while back in by_t:
                tb = datetime.fromtimestamp(back / 1000, ny)
                if tb.weekday() < 5 and (tb.hour, tb.minute) == (15, 59):
                    if by_t[back]["markClose"] and prev and prev["markClose"]:
                        closed_move.append(math.log(prev["markClose"] / by_t[back]["markClose"]))
                    break
                back -= 60_000
    blk["open_jump_1m"] = dict(n=len(open_jump), abs_median_pct=_r(median([abs(v) for v in open_jump]) * 100, 3) if open_jump else float("nan"),
                               abs_p90_pct=_r(S.quantile([abs(v) for v in open_jump], 0.9) * 100, 3) if len(open_jump) > 2 else float("nan"))
    blk["closed_hours_move"] = dict(n=len(closed_move), abs_median_pct=_r(median([abs(v) for v in closed_move]) * 100, 3) if closed_move else float("nan"),
                                    abs_p90_pct=_r(S.quantile([abs(v) for v in closed_move], 0.9) * 100, 3) if len(closed_move) > 2 else float("nan"))
    per_slot = blk["us_session_open"]["sd_1m"] * math.sqrt(SLOT_S / 60)
    hourly = Counter()
    for k in c:
        hourly[datetime.fromtimestamp(k["time"] / 1000, ny).hour] += k["tradeCount"]
    tot = sum(hourly.values()) or 1
    return dict(
        meta=meta("spcx", ["Phoenix perp candles", "Solana RPC"],
                  windows=dict(phoenix=[sources.PHOENIX_START.date().isoformat(), sources.PHOENIX_END.date().isoformat()]),
                  prefixes=["phoenix/", "rpc/prio_", "rpc/sigs_SPCX", "jito/tip"]),
        fair_value=dict(
            reference="Phoenix SPCX mark (tracks Pyth Equity.US.SPCX/USD plus the perp's premium). Replace with Pyth Pro history when keyed.",
            by_session=blk,
        ),
        flow=dict(trade_share_by_ny_hour=[_r(hourly[h] / tot, 3) for h in range(24)]),
        venues=dict(phoenix_perp=ph),
        tx_fees=priority_fee_block(),
        engine_mapping=dict(
            per_slot_sd_us_open=_r(per_slot),
            making_step_vol_pct_calibrated=[_r(per_slot * math.sqrt(3) * 100, 3), _r(per_slot * math.sqrt(9) * 100, 3)],
            making_step_vol_pct_current=[0.02, 0.08],
            weekend_family_weekend_pct_current=[0.5, 12.0],
            note="per-slot sd from 1 m mark returns in US hours, assuming no microstructure noise at 1 m; weekend_pct compares with closed_hours_move (a weekend is ~4x an overnight)",
        ),
    )


def inputs_for(prefixes: list[str]) -> list[str]:
    return [n for n in sorted(fetch._manifest().keys()) if any(n.startswith(p) for p in prefixes)]


def meta(asset: str, srcs: list[str], windows: dict, prefixes: list[str] | None = None) -> dict:
    prefixes = prefixes or [""]
    wanted = [n for n in inputs_for(prefixes) if not n.endswith(".CHECKSUM")]
    prov = fetch.provenance(wanted)
    digest = hashlib.sha256("".join(p["sha256"] for p in prov).encode()).hexdigest()[:16]
    version = hashlib.sha256((digest + code_hash()).encode()).hexdigest()[:8]
    return dict(
        schema_version=SCHEMA_VERSION,
        calibration_id=f"{asset}.{datetime.now(timezone.utc):%Y%m%d}.{version}",
        asset=asset,
        fitted_at=datetime.now(timezone.utc).isoformat(timespec="seconds"),
        fitter="infra-calib",
        fitter_code_sha256_12=code_hash(),
        slot_seconds=SLOT_S,
        sources=srcs,
        windows=windows,
        inputs_prefixes=prefixes,
        inputs_digest=digest,
        inputs_count=len(prov),
    )

"""Measure Phoenix SPCX's weekend reopens from 1 m candles: how far the book
moved over each weekend, what the reopen kept of it, the weekend's prints,
and whether BTC's and SOL's weekend moved with SPCX's reopen gap.

    python3 -I calibration/fitter/weekend/reopens.py --fetch DIR    # download candles into DIR
    python3 -I calibration/fitter/weekend/reopens.py --candles DIR [--out calibration/weekend]

A weekend is Friday 20:00 to Sunday 20:00 New York (Phoenix prices SPCX's
index from external markets 24/5, Sunday 20:00 to Friday 20:00 ET). For
each Friday from 2026-06-12 to 2026-11-20, holiday weekends left out
(2026-06-19, 07-03: the market closed Thursday; 09-04: Labor Day Monday):

- F, the mark at Friday 19:59 (the last external price);
- the weekend's move, the book's last trade at Sunday 19:59 against F;
- the open, the mark at Sunday 20:05 against F; the gap, the open against
  the mark at Sunday 19:59;
- the weekend's prints: minutes holding $100 or more, and their dollars.

Raw candles are never committed: the output records each file's SHA-256.
Stdlib only, deterministic on the same candles.
"""

from __future__ import annotations

import argparse
import datetime as dt
import glob
import hashlib
import json
import math
import random
import statistics as st
import time
import urllib.request
from pathlib import Path

API = "https://perp-api.phoenix.trade/candles?symbol={sym}&timeframe=1m&startTime={a}&endTime={b}"
FIRST_FRIDAY = dt.date(2026, 6, 12)
LAST_FRIDAY = dt.date(2026, 11, 20)
HOLIDAYS = {"2026-06-19", "2026-07-03", "2026-09-04"}
SYMBOLS = ("SPCX", "BTC", "SOL")
MINUTE_MS = 60_000
PRINT_USD_MIN = 100.0
BOOT = 4000


def et_offset_hours(day: dt.date) -> int:
    return 4 if dt.date(2026, 3, 8) <= day < dt.date(2026, 11, 1) else 5


def at_ms(day: dt.date, hour: int, offset: int) -> int:
    t = dt.datetime(day.year, day.month, day.day, hour, tzinfo=dt.timezone.utc)
    return int((t + dt.timedelta(hours=offset)).timestamp() * 1000)


def fridays():
    fri = FIRST_FRIDAY
    while fri <= LAST_FRIDAY:
        yield fri
        fri += dt.timedelta(days=7)


def fetch(out: Path) -> None:
    out.mkdir(parents=True, exist_ok=True)
    for sym in SYMBOLS:
        for fri in fridays():
            off = et_offset_hours(fri)
            start, end = at_ms(fri, 15, off), at_ms(fri, 15, off) + 60 * 3600 * 1000
            rows, t = [], start
            while t < end:
                u = min(t + 1000 * MINUTE_MS, end)
                for _ in range(5):
                    try:
                        rows += json.load(urllib.request.urlopen(API.format(sym=sym, a=t, b=u), timeout=30))
                        break
                    except Exception:  # rate limits: wait and retry
                        time.sleep(3)
                t = u
                time.sleep(1.0)
            (out / f"{sym}_{fri.isoformat()}.json").write_text(json.dumps(rows))


def weekend(rows: dict, fri: dt.date) -> dict:
    off = et_offset_hours(fri)
    sun = fri + dt.timedelta(days=2)
    close, reopen = at_ms(fri, 20, off) - MINUTE_MS, at_ms(sun, 20, off)

    def last(t, key):
        ks = [k for k in rows if k <= t and rows[k].get(key)]
        return rows[max(ks)][key]

    f = last(close, "markClose")
    book, mark = last(reopen - MINUTE_MS, "close"), last(reopen - MINUTE_MS, "markClose")
    opened = last(reopen + 5 * MINUTE_MS, "markClose")
    inside = [r for k, r in rows.items() if close < k < reopen]
    prints = [r for r in inside if r.get("volumeQuote", 0) >= PRINT_USD_MIN]
    burst = [r for k, r in rows.items() if reopen <= k < reopen + 5 * MINUTE_MS]
    pct = lambda a, b: 100 * (a / b - 1)
    return {
        "friday": fri.isoformat(), "friday_mark": f,
        "weekend_move_pct": round(pct(book, f), 4), "mark_move_pct": round(pct(mark, f), 4),
        "open_pct": round(pct(opened, f), 4), "gap_pct": round(pct(opened, mark), 4),
        "print_minutes": len(prints), "print_usd": round(sum(r["volumeQuote"] for r in prints)),
        "reopen_5m_trades": sum(r.get("tradeCount", 0) for r in burst),
        "reopen_5m_usd": round(sum(r.get("volumeQuote", 0) for r in burst)),
    }


def corr(x, y):
    mx, my = st.mean(x), st.mean(y)
    return sum((a - mx) * (b - my) for a, b in zip(x, y)) / math.sqrt(
        sum((a - mx) ** 2 for a in x) * sum((b - my) ** 2 for b in y))


def study(candles: Path) -> dict:
    files, out = {}, {}
    for sym in SYMBOLS:
        rows_by = {}
        for fri in fridays():
            if fri.isoformat() in HOLIDAYS:
                continue
            path = candles / f"{sym}_{fri.isoformat()}.json"
            data = path.read_bytes()
            files[path.name] = hashlib.sha256(data).hexdigest()
            rows_by[fri] = weekend({r["time"]: r for r in json.loads(data)}, fri)
        out[sym] = rows_by
    spcx = list(out["SPCX"].values())
    x = [w["weekend_move_pct"] for w in spcx]
    y = [w["open_pct"] for w in spcx]
    n = len(spcx)
    keep = sum(a * b for a, b in zip(x, y)) / sum(a * a for a in x)
    resid = math.sqrt(sum((b - keep * a) ** 2 for a, b in zip(x, y)) / (n - 1))
    mx, my = st.mean(x), st.mean(y)
    slope = sum((a - mx) * (b - my) for a, b in zip(x, y)) / sum((a - mx) ** 2 for a in x)
    rng = random.Random(2)
    boot = []
    for _ in range(BOOT):
        idx = [rng.randrange(n) for _ in range(n)]
        boot.append(sum(x[i] * y[i] for i in idx) / sum(x[i] ** 2 for i in idx))
    boot.sort()
    gaps = sorted(abs(w["gap_pct"]) for w in spcx)
    moves = sorted(abs(v) for v in x)
    residual = [b - keep * a for a, b in zip(x, y)]
    cross = {}
    for sym in ("BTC", "SOL"):
        other = [out[sym][dt.date.fromisoformat(w["friday"])]["mark_move_pct"] for w in spcx]
        cross[sym] = {"r_with_spcx_gap": round(corr(other, [w["gap_pct"] for w in spcx]), 3),
                      "r_with_open_residual": round(corr(other, residual), 3)}
    q = lambda v, p: v[min(len(v) - 1, int(round(p * (len(v) - 1))))]
    return {
        "study": "phoenix-spcx-weekend-reopens", "version": 1, "weekends": n,
        "from": FIRST_FRIDAY.isoformat(), "through": LAST_FRIDAY.isoformat(), "left_out": sorted(HOLIDAYS),
        "source": "Phoenix perp API /candles, 1 m (mark and last trade)", "files_sha256": files,
        "reopen_keep_through_origin": round(keep, 3),
        "reopen_keep_95": [round(boot[int(0.025 * BOOT)], 2), round(boot[int(0.975 * BOOT)], 2)],
        "open_residual_sd_pct": round(resid, 3),
        "ols": {"slope": round(slope, 3), "intercept_pct": round(my - slope * mx, 3), "r": round(corr(x, y), 3)},
        "weekend_move_abs_pct": {"median": round(st.median(moves), 3), "p90": round(q(moves, 0.9), 3), "max": round(moves[-1], 3)},
        "weekend_moves_up": sum(v > 0 for v in x),
        "gap_abs_pct": {"median": round(st.median(gaps), 3), "p90": round(q(gaps, 0.9), 3), "max": round(gaps[-1], 3)},
        "print_minutes_median": st.median(w["print_minutes"] for w in spcx),
        "print_usd_median": st.median(w["print_usd"] for w in spcx),
        "cross_market": cross, "per_weekend": spcx,
    }


def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument("--fetch", type=Path)
    p.add_argument("--candles", type=Path)
    p.add_argument("--out", type=Path, default=Path("calibration/weekend"))
    a = p.parse_args()
    if a.fetch:
        fetch(a.fetch)
        return
    result = study(a.candles)
    a.out.mkdir(parents=True, exist_ok=True)
    path = a.out / f"spcx-reopens.{LAST_FRIDAY.strftime('%Y%m%d')}.json"
    path.write_text(json.dumps(result, indent=1, sort_keys=True) + "\n")
    print(path, json.dumps({k: result[k] for k in ("reopen_keep_through_origin", "reopen_keep_95", "open_residual_sd_pct", "ols", "weekend_move_abs_pct", "gap_abs_pct", "cross_market", "print_minutes_median", "print_usd_median")}, indent=1))


if __name__ == "__main__":
    main()

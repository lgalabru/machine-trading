"""Study a Phoenix perp's realised funding: what each hour's funding was,
the premium it implies, how long a premium lasts, and how often the cap
binds.

    python3 -I calibration/fitter/funding/study.py --symbol SOL \
        --from 2026-08-27 --to 2026-11-25T01:12 --cache <raw dir> [--out calibration/funding]

It writes `<out>/phoenix-funding.<symbol>.<date>.<hash>.json`, where the date
is the window's last day and the hash covers the raw responses' SHA-256 and
this file's code, as a calibration file's id does.

The source is Phoenix's public perp API (no key):
`https://perp-api.phoenix.trade/v1/funding/<symbol>/rates?startTime=<ms>&endTime=<ms>&limit=1000`,
the route `phoenix-rise-api` 0.10.2 calls `get_funding_rate_history`. Each
point is one funding interval's rate, in percent of the price, at the
interval's end (`fundingRatePercentage`). `/exchange` gives the market's
interval, period and cap; Phoenix's funding over an interval is the
interval's mean (mark - index) / index times interval / period, clamped to
the cap, so the interval's mean premium is the rate times period / interval.

Raw responses are kept in `--cache` (never committed): the study records
each request's URL with its body's SHA-256 and byte count, and a digest of
them all. A rerun on the cache gives the same file. Stdlib only.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import statistics
import time
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

API = "https://perp-api.phoenix.trade"
# One request covers this many hours: under the route's 1,000-point limit.
CHUNK_HOURS = 72
# The tape study's window (calibration/tape/phoenix-tape.20261125.fc443f1e.json).
TAPE_FROM_MS = 1795304758077
TAPE_TO_MS = 1795569128706
QUANTILES = (0.01, 0.1, 0.25, 0.5, 0.75, 0.9, 0.99)
ACF_LAGS = (1, 2, 3, 6, 12, 24)
# Session-sized windows: a session plays this many intervals of funding.
WINDOW_HOURS = (24, 72, 120)


def parse_time(text: str) -> int:
    for fmt in ("%Y-%m-%dT%H:%M", "%Y-%m-%d"):
        try:
            t = datetime.strptime(text, fmt).replace(tzinfo=timezone.utc)
            return int(t.timestamp() * 1000)
        except ValueError:
            continue
    raise SystemExit(f"bad time {text!r}: YYYY-MM-DD or YYYY-MM-DDTHH:MM, UTC")


def fetch(url: str, cache: Path, name: str) -> bytes:
    path = cache / name
    if path.exists():
        return path.read_bytes()
    for attempt in range(4):
        try:
            request = urllib.request.Request(url, headers={"User-Agent": "mtx-calibration/1"})
            with urllib.request.urlopen(request, timeout=60) as response:
                body = response.read()
            break
        except OSError:
            if attempt == 3:
                raise
            time.sleep(2 ** attempt)
    cache.mkdir(parents=True, exist_ok=True)
    path.write_bytes(body)
    return body


def pull(symbol: str, start_ms: int, end_ms: int, cache: Path):
    """Every interval's rate in [start, end), and each request's record."""
    inputs, rates = [], {}
    t = start_ms
    while t < end_ms:
        t2 = min(end_ms, t + CHUNK_HOURS * 3_600_000)
        url = f"{API}/v1/funding/{symbol}/rates?startTime={t}&endTime={t2}&limit=1000"
        body = fetch(url, cache, f"funding_{symbol}_{t}_{t2}.json")
        inputs.append({"url": url, "sha256": hashlib.sha256(body).hexdigest(), "bytes": len(body)})
        reply = json.loads(body)
        assert reply["symbol"] == symbol, reply.get("symbol")
        points = reply["rates"]
        assert len(points) < 1000, f"{url}: the route's limit was reached"
        for p in points:
            ts = int(p["timestamp"])
            if start_ms <= ts * 1000 < end_ms:
                rates[ts] = float(p["fundingRatePercentage"])
        t = t2
    exchange_url = f"{API}/exchange"
    body = fetch(exchange_url, cache, "exchange.json")
    inputs.append({"url": exchange_url, "sha256": hashlib.sha256(body).hexdigest(), "bytes": len(body)})
    market = next(m for m in json.loads(body)["markets"] if m["symbol"] == symbol)
    return sorted(rates.items()), market, inputs


def quantile(values, p):
    v = sorted(values)
    return v[min(len(v) - 1, int(p * len(v)))]


def acf(values, lag):
    n = len(values)
    if n <= lag + 2:
        return None
    m = statistics.fmean(values)
    var = sum((x - m) ** 2 for x in values)
    return sum((values[i] - m) * (values[i + lag] - m) for i in range(n - lag)) / var


def runs(values):
    """Lengths of runs of one sign, in intervals."""
    out, length, sign = [], 0, 0
    for x in values:
        s = 1 if x > 0 else -1 if x < 0 else 0
        if s == sign and s != 0:
            length += 1
        else:
            if length:
                out.append(length)
            sign, length = s, 1 if s else 0
    if length:
        out.append(length)
    return out


def describe(points, ratio, cap_pct):
    """A set of intervals: the rates (percent an interval) and the premium
    they imply (bp: the rate times period / interval)."""
    rates = [r for _, r in points]
    premium = [r * ratio * 100.0 for r in rates]
    absolute = [abs(p) for p in premium]
    lag1 = acf(premium, 1)
    half_life = math.log(0.5) / math.log(lag1) if lag1 and 0 < lag1 < 1 else None
    signs = runs(premium)
    gaps = [b[0] - a[0] for a, b in zip(points, points[1:])]
    return {
        "intervals": len(rates),
        "interval_gap_seconds_median": quantile(gaps, 0.5) if gaps else None,
        "rate_pct": {
            "mean": round(statistics.fmean(rates), 6),
            "sd": round(statistics.pstdev(rates), 6),
            "quantiles": {str(q): round(quantile(rates, q), 6) for q in QUANTILES},
        },
        "premium_bps": {
            "mean": round(statistics.fmean(premium), 3),
            "sd": round(statistics.pstdev(premium), 3),
            "quantiles": {str(q): round(quantile(premium, q), 3) for q in QUANTILES},
            "abs_median": round(quantile(absolute, 0.5), 3),
            "abs_p90": round(quantile(absolute, 0.9), 3),
            "abs_max": round(max(absolute), 3),
            "positive_share": round(sum(1 for p in premium if p > 0) / len(premium), 3),
        },
        "persistence": {
            "acf": {str(k): (round(a, 3) if a is not None else None) for k in ACF_LAGS for a in [acf(premium, k)]},
            "half_life_intervals": round(half_life, 2) if half_life else None,
            "sign_run_mean_intervals": round(statistics.fmean(signs), 2) if signs else None,
            "sign_run_p90_intervals": quantile(signs, 0.9) if signs else None,
        },
        "cap": {
            "cap_pct": cap_pct,
            "at_cap_share": round(sum(1 for r in rates if abs(r) >= 0.999 * cap_pct) / len(rates), 4),
            "max_abs_share_of_cap": round(max(abs(r) for r in rates) / cap_pct, 4),
        },
    }


def windows(points, ratio, hours):
    """Non-overlapping windows of `hours` consecutive intervals (a gap in the
    data ends a window): each window's mean premium (bp), and, pooled over
    the windows, the premium's spread about its window's mean and its lag-1
    autocorrelation there. What a session that plays `hours` intervals of
    funding sees: a level for the session, and a wander around it."""
    chunks, current = [], []
    for (t, r) in points:
        if current and abs(t - current[-1][0] - 3600) > 60:
            current = []
        current.append((t, r * ratio * 100.0))
        if len(current) == hours:
            chunks.append([p for _, p in current])
            current = []
    if len(chunks) < 4:
        return None
    means = [statistics.fmean(c) for c in chunks]
    dev = [[p - m for p in c] for c, m in zip(chunks, means)]
    within_sd = math.sqrt(statistics.fmean([d * d for c in dev for d in c]))
    num = sum(c[i] * c[i + 1] for c in dev for i in range(len(c) - 1))
    den = sum(d * d for c in dev for d in c)
    lag1 = num / den
    return {
        "hours": hours,
        "windows": len(chunks),
        "mean_bps": {
            "mean": round(statistics.fmean(means), 3),
            "sd": round(statistics.pstdev(means), 3),
            "quantiles": {str(q): round(quantile(means, q), 3) for q in QUANTILES},
            "negative_share": round(sum(1 for m in means if m < 0) / len(means), 3),
        },
        "within_sd_bps": round(within_sd, 3),
        "within_acf_1": round(lag1, 3),
        "within_half_life_intervals": round(math.log(0.5) / math.log(lag1), 2) if 0 < lag1 < 1 else None,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--symbol", default="SOL")
    ap.add_argument("--from", dest="start", required=True)
    ap.add_argument("--to", dest="end", required=True)
    ap.add_argument("--cache", type=Path, required=True)
    ap.add_argument("--out", type=Path, default=Path("calibration/funding"))
    a = ap.parse_args()
    start_ms, end_ms = parse_time(a.start), parse_time(a.end)
    assert start_ms < end_ms
    points, market, inputs = pull(a.symbol, start_ms, end_ms, a.cache)
    assert len(points) > 48, "a study needs days of intervals"
    interval = int(market["fundingIntervalSeconds"])
    period = int(market["fundingPeriodSeconds"])
    cap_pct = float(market["maxFundingRatePerIntervalPercentage"])
    ratio = period / interval
    tape = [(t, r) for t, r in points if TAPE_FROM_MS <= t * 1000 < TAPE_TO_MS]
    digest_in = hashlib.sha256("".join(i["sha256"] for i in inputs).encode()).hexdigest()
    code = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    digest = hashlib.sha256((digest_in + code).encode()).hexdigest()[:8]
    day = datetime.fromtimestamp(end_ms / 1000, tz=timezone.utc).strftime("%Y%m%d")
    out = {
        "study": "phoenix-funding",
        "version": 1,
        "id": f"phoenix-funding.{a.symbol.lower()}.{day}.{digest}",
        "symbol": a.symbol,
        "window": {"from_ms": start_ms, "to_ms": end_ms,
                   "first_interval": points[0][0], "last_interval": points[-1][0]},
        "market": {"interval_seconds": interval, "period_seconds": period,
                   "max_rate_per_interval_pct": round(cap_pct, 6),
                   "source": f"{API}/exchange, as fetched (the cap moves with the market's settings)"},
        "rules": {"rate": "fundingRatePercentage: one interval's funding, percent of the price, paid by longs when positive",
                  "premium": "the interval's mean (mark - index) / index, in bp: the rate times period / interval",
                  "half_life": "log 0.5 / log acf(1), in intervals"},
        "all": describe(points, ratio, cap_pct),
        "tape_window": describe(tape, ratio, cap_pct) if len(tape) > 24 else None,
        "windows": [w for h in WINDOW_HOURS for w in [windows(points, ratio, h)] if w],
        "inputs": inputs,
        "inputs_digest": digest_in,
        "code_sha256": code,
    }
    a.out.mkdir(parents=True, exist_ok=True)
    path = a.out / f"{out['id']}.json"
    path.write_text(json.dumps(out, indent=1, sort_keys=True) + "\n")
    print(path)


if __name__ == "__main__":
    main()

"""Study a Phoenix tape recorded on mainnet: who trades, how much, how often,
how informed, and what follows a jump.

    python3 -I calibration/fitter/tape/study.py --tape <tape.csv.gz> [--out calibration/tape] [--markets SPCX,NVDA]

It writes `<out>/phoenix-tape.<date>.<hash>.json`, where the date is the
tape's last day and the hash covers the tape's SHA-256 and this file's code,
as a calibration file's id does.

The tape is luthor's `history.sqlite` `tape` table, exported as CSV
(`at_ms,slot,symbol,taker,price,size`, one row per maker filled: a taker
order that crosses several makers is several rows in one slot). Raw data is
never committed: the study records the tape's SHA-256 and byte count.

Stdlib only, deterministic: the same tape gives the same file.
"""

from __future__ import annotations

import argparse
import bisect
import collections
import csv
import gzip
import hashlib
import json
import math
import random
import statistics
from datetime import datetime, timezone
from pathlib import Path

# A taker order under this notional is a pinger's print: on 2026-11-24 a bot
# traded a ~$3 lot about once a minute on every Phoenix market, 73% to 100%
# of the prints of most markets (RFD 0002 in luthor). It carries no news.
PINGER_USD_MAX = 5.0
HORIZONS_S = (1, 10, 60, 300)
BUCKETS_USD = (5.0, 100.0, 1_000.0, 10_000.0, math.inf)
JUMP_PCT_MIN = 1.5
JUMP_USD_MIN = 2_000.0  # as the shock watch's flow_usd_min (luthor RFD 0003)
AFTERMATH_MIN = (5, 15, 30)
BOOT = 400
# Markets whose takers' markouts and impact are studied (every market's flow
# and jumps are): the bench's market, the shock watch's, and the liquid
# ones a maker would compare them with.
MARKETS = ("SPCX", "NVDA", "ANT", "SNDK", "TSLA", "QQQ", "SOL", "BTC", "ETH", "MEGA")


def session_of(at_ms: int) -> str:
    """US equity sessions in UTC for winter time (EST): regular 14:30-21:00
    on weekdays, `weekend` from Friday 22:00 to Sunday 23:00, else `off`."""
    t = datetime.fromtimestamp(at_ms / 1000, tz=timezone.utc)
    minute = t.hour * 60 + t.minute
    wd = t.weekday()
    if (wd == 4 and minute >= 22 * 60) or wd == 5 or (wd == 6 and minute < 23 * 60):
        return "weekend"
    if wd < 5 and 14 * 60 + 30 <= minute < 21 * 60:
        return "regular"
    return "off"


def load(path: Path):
    fills = collections.defaultdict(list)
    with gzip.open(path, "rt") as f:
        reader = csv.reader(f)
        assert next(reader) == ["at_ms", "slot", "symbol", "taker", "price", "size"]
        for at, slot, sym, taker, px, sz in reader:
            assert taker in ("buy", "sell"), taker
            fills[sym].append((int(at), int(slot), 1 if taker == "buy" else -1, float(px), float(sz)))
    for v in fills.values():
        v.sort()
    return fills


def orders_of(fills):
    """Fills in one slot on one side are one taker order (the tape doesn't
    name the taker): its notional, VWAP and the worst price it reached."""
    out = []
    for (slot, side), group in _groupby(fills, key=lambda x: (x[1], x[2])):
        size = sum(g[4] for g in group)
        notional = sum(g[3] * g[4] for g in group)
        worst = max(g[3] for g in group) if side > 0 else min(g[3] for g in group)
        out.append({"at_ms": group[0][0], "slot": slot, "side": side, "size": size,
                    "usd": notional, "vwap": notional / size, "worst": worst, "fills": len(group)})
    out.sort(key=lambda o: (o["at_ms"], o["slot"]))
    return out


def _groupby(rows, key):
    groups = collections.OrderedDict()
    for r in rows:
        groups.setdefault(key(r), []).append(r)
    return groups.items()


def quantile(v, p):
    v = sorted(v)
    if not v:
        return None
    return v[min(len(v) - 1, int(p * len(v)))]


def mean_ci(v, seed):
    if len(v) < 10:
        return None
    rng = random.Random(seed)
    means = sorted(statistics.fmean(rng.choices(v, k=len(v))) for _ in range(BOOT))
    return {"n": len(v), "mean": round(statistics.fmean(v), 3),
            "low": round(means[int(0.025 * BOOT)], 3), "high": round(means[int(0.975 * BOOT)], 3)}


# A side's last print older than this doesn't say where the touch is.
MID_STALE_MS = 300_000


class Prices:
    """The book's mid at any time, as the tape can tell it: the mean of the
    last print a buyer took (the ask) and the last a seller took (the bid),
    the pinger's included, since it trades at the touch. A print-to-print
    price would bounce between bid and ask on every pinger print, which on
    a wide book reads as jumps that revert, and as takers who lose."""

    def __init__(self, fills):
        self.t, self.mid = [], []
        ask = bid = None
        ask_ms = bid_ms = -MID_STALE_MS
        for at, _, side, px, _ in fills:
            if side > 0:
                ask, ask_ms = px, at
            else:
                bid, bid_ms = px, at
            if ask is not None and bid is not None and abs(ask_ms - bid_ms) <= MID_STALE_MS and bid <= ask * 1.05:
                self.t.append(at)
                self.mid.append((ask + bid) / 2)

    def at(self, ms):
        i = bisect.bisect_right(self.t, ms) - 1
        return self.mid[i] if i >= 0 else None

    def last_ms(self):
        return self.t[-1] if self.t else 0


def markouts(orders, prices, seed):
    """What a taker knew: the mid's move h seconds after its order, from the
    mid just before it, in its direction, in bp (positive: it was right),
    and what it paid for crossing: its VWAP against that mid. A maker that
    filled it earns the cost and loses the markout."""
    res = {}
    for lo, hi in zip(BUCKETS_USD, BUCKETS_USD[1:]):
        bucket = [o for o in orders if lo <= o["usd"] < hi]
        row, cost = {}, []
        for o in bucket:
            before = prices.at(o["at_ms"] - 1)
            if before:
                cost.append(o["side"] * (o["vwap"] / before - 1) * 1e4)
        for h in HORIZONS_S:
            v = []
            for o in bucket:
                if o["at_ms"] + h * 1000 > prices.last_ms():
                    continue
                before, after = prices.at(o["at_ms"] - 1), prices.at(o["at_ms"] + h * 1000)
                if before and after:
                    v.append(o["side"] * (after / before - 1) * 1e4)
            row[f"{h}s"] = mean_ci(v, seed + h)
        res[f"{lo:g}-{hi:g}"] = {"orders": len(bucket), "markout_bps": row, "cost_bps": mean_ci(cost, seed)}
    return res


def impact(orders, prices):
    """How far an order moved the price: its worst fill against the last
    price before it, in bp, by size bucket (median and p90)."""
    res = {}
    for lo, hi in zip(BUCKETS_USD, BUCKETS_USD[1:]):
        v = []
        for o in orders:
            if not lo <= o["usd"] < hi:
                continue
            before = prices.at(o["at_ms"] - 1)
            if before:
                v.append(o["side"] * (o["worst"] / before - 1) * 1e4)
        # (against the mid: half the spread is in it)
        if len(v) >= 10:
            res[f"{lo:g}-{hi:g}"] = {"n": len(v), "median_bps": round(quantile(v, 0.5), 2),
                                     "p90_bps": round(quantile(v, 0.9), 2)}
    return res


def flow(orders, minutes_by_session):
    res = {}
    for s, minutes in minutes_by_session.items():
        v = [o for o in orders if session_of(o["at_ms"]) == s]
        usd = [o["usd"] for o in v]
        gaps = [(b["at_ms"] - a["at_ms"]) / 1000 for a, b in zip(v, v[1:])]
        same = sum(1 for a, b in zip(v, v[1:]) if b["side"] == a["side"]) / max(1, len(v) - 1)
        logs = [math.log(u) for u in usd]
        res[s] = {
            "minutes": round(minutes),
            "orders_per_minute": round(len(v) / minutes, 4) if minutes else None,
            "usd_per_hour": round(sum(usd) / minutes * 60, 1) if minutes else None,
            "usd_median": round(quantile(usd, 0.5), 2) if usd else None,
            "usd_p90": round(quantile(usd, 0.9), 2) if usd else None,
            "usd_p99": round(quantile(usd, 0.99), 2) if usd else None,
            "log_sd": round(statistics.pstdev(logs), 3) if len(logs) > 2 else None,
            "within_1s_share": round(sum(1 for g in gaps if g < 1) / len(gaps), 3) if gaps else None,
            "same_side_next": round(same, 3) if len(v) > 1 else None,
            "buy_share": round(sum(1 for o in v if o["side"] > 0) / len(v), 3) if v else None,
        }
    return res


def jumps(fills, prices):
    """1-minute bars of the mid; a jump is a bar whose close is
    JUMP_PCT_MIN or more from the last close, with non-pinger prints in it
    worth JUMP_USD_MIN. Its aftermath at each horizon: `revert` (back half
    its move or more), `extend` (on half its move again or more), else
    `stall`."""
    bars = collections.OrderedDict()
    flowed = collections.defaultdict(float)
    for t, m in zip(prices.t, prices.mid):
        bars[t // 60000] = m
    for at, _, _, px, sz in fills:
        if px * sz >= PINGER_USD_MAX:
            flowed[at // 60000] += px * sz
    keys = list(bars)
    out = []
    for prev, cur in zip(keys, keys[1:]):
        move = bars[cur] / bars[prev] - 1
        if abs(move) * 100 < JUMP_PCT_MIN or flowed[cur] < JUMP_USD_MIN:
            continue
        ev = {"at_ms": cur * 60000, "move_pct": round(move * 100, 2), "usd": round(flowed[cur]),
              "session": session_of(cur * 60000), "gap_minutes": cur - prev}
        for h in AFTERMATH_MIN:
            later = [bars[k] for k in keys if cur < k <= cur + h]
            if not later:
                ev[f"{h}m"] = None
                continue
            after = later[-1] / bars[cur] - 1
            ratio = after / move
            ev[f"{h}m"] = "revert" if ratio <= -0.5 else "extend" if ratio >= 0.5 else "stall"
        out.append(ev)
    return out


def study(tape: Path, markets):
    fills = load(tape)
    t0 = min(v[0][0] for v in fills.values())
    t1 = max(v[-1][0] for v in fills.values())
    minutes = collections.Counter(session_of(m * 60000) for m in range(t0 // 60000, t1 // 60000 + 1))
    per = {}
    all_jumps = []
    for i, sym in enumerate(sorted(fills)):
        f = fills[sym]
        orders = orders_of(f)
        real = [o for o in orders if o["usd"] >= PINGER_USD_MAX]
        pinger = [o for o in orders if o["usd"] < PINGER_USD_MAX]
        prices = Prices(f)
        js = jumps(f, prices)
        for j in js:
            j["symbol"] = sym
        all_jumps += js
        row = {
            "orders": len(orders), "pinger_orders": len(pinger),
            "pinger_share": round(len(pinger) / len(orders), 3),
            "flow": flow(real, minutes),
            "jumps": len(js),
        }
        if not markets or sym in markets:
            row["markouts"] = markouts(real, prices, seed=1000 * i)
            row["impact"] = impact(real, prices)
        per[sym] = row
    tally = collections.Counter()
    for j in all_jumps:
        for h in AFTERMATH_MIN:
            if j[f"{h}m"]:
                tally[(h, j[f"{h}m"])] += 1
    aftermath = {f"{h}m": {k: tally[(h, k)] for k in ("revert", "stall", "extend")} for h in AFTERMATH_MIN}
    body = tape.read_bytes()
    return {
        "study": "phoenix-tape",
        "version": 1,
        "tape": {"sha256": hashlib.sha256(body).hexdigest(), "bytes": len(body),
                 "from_ms": t0, "to_ms": t1, "days": round((t1 - t0) / 86400000, 3),
                 "minutes_by_session": dict(sorted(minutes.items())),
                 "source": "luthor history.sqlite on mtx-alpha, Phoenix trades read from the validator"},
        "rules": {"pinger_usd_max": PINGER_USD_MAX, "order": "fills in one slot on one side",
                  "jump_pct_min": JUMP_PCT_MIN, "jump_usd_min": JUMP_USD_MIN,
                  "price": "mid of the last buy and last sell print, each under 5 minutes old", "sessions": "US winter time in UTC: regular 14:30-21:00 weekdays; weekend Fri 22:00 to Sun 23:00"},
        "markets": per,
        "jumps": {"count": len(all_jumps), "aftermath": aftermath, "events": all_jumps},
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tape", type=Path, required=True)
    ap.add_argument("--out", type=Path, default=Path("calibration/tape"))
    ap.add_argument("--markets", default=",".join(MARKETS))
    a = ap.parse_args()
    markets = {m for m in a.markets.split(",") if m}
    out = study(a.tape, markets)
    code = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    digest = hashlib.sha256((out["tape"]["sha256"] + code).encode()).hexdigest()[:8]
    day = datetime.fromtimestamp(out["tape"]["to_ms"] / 1000, tz=timezone.utc).strftime("%Y%m%d")
    out["id"] = f"phoenix-tape.{day}.{digest}"
    out["code_sha256"] = code
    out["markets_studied"] = sorted(markets)
    a.out.mkdir(parents=True, exist_ok=True)
    path = a.out / f"{out['id']}.json"
    path.write_text(json.dumps(out, indent=1, sort_keys=True) + "\n")
    print(path)


if __name__ == "__main__":
    main()

"""Keyless public data sources. Every download is cached under data/raw and
recorded in data/raw/manifest.json (url, sha256, bytes, fetched_at) so a
calibration file can cite exactly what it was fitted on.

No source here needs a key. Sources that do (Pyth Pro history, Hermes price
updates, Birdeye, Dune, Flipside, Helius) are listed in CALIBRATION.md and not
called.
"""

from __future__ import annotations

import csv
import hashlib
import io
import json
import time
import urllib.error
import urllib.request
import zipfile
from datetime import date, datetime, timedelta, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RAW = ROOT / "data" / "raw"
MANIFEST = RAW / "manifest.json"
UA = {"User-Agent": "arcanum-calib/0.1 (research; low volume)"}
PAUSE_S = 0.35  # between requests to any one host

SOLANA_RPC = "https://api.mainnet-beta.solana.com"
PHOENIX_PERP_API = "https://perp-api.phoenix.trade"
JUP_QUOTE = "https://lite-api.jup.ag/swap/v1/quote"
BINANCE_VISION = "https://data.binance.vision/data/spot"

SOL_MINT = "So11111111111111111111111111111111111111112"
USDC_MINT = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
JITOSOL_MINT = "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn"
JITO_STAKE_POOL = "Jito4APyf642JPZPx3hGc6WWJ8zPKtRbRs4P815Awbb"


def _manifest() -> dict:
    if MANIFEST.exists():
        return json.loads(MANIFEST.read_text())
    return {}


def _record(name: str, url: str, body: bytes) -> None:
    m = _manifest()
    m[name] = {
        "url": url,
        "sha256": hashlib.sha256(body).hexdigest(),
        "bytes": len(body),
        "fetched_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
    }
    MANIFEST.write_text(json.dumps(m, indent=1, sort_keys=True))


def provenance(names: list[str]) -> list[dict]:
    m = _manifest()
    return [dict(name=n, **m[n]) for n in names if n in m]


def _get(url: str, data: bytes | None = None, headers: dict | None = None, tries: int = 4) -> bytes:
    h = dict(UA)
    h.update(headers or {})
    for attempt in range(tries):
        try:
            req = urllib.request.Request(url, data=data, headers=h)
            with urllib.request.urlopen(req, timeout=60) as r:
                body = r.read()
            time.sleep(PAUSE_S)
            return body
        except urllib.error.HTTPError as e:
            if e.code in (429, 500, 502, 503, 504) and attempt + 1 < tries:
                time.sleep(2.0 * (attempt + 1))
                continue
            raise
    raise RuntimeError("unreachable")


def cached(name: str, url: str, data: bytes | None = None, headers: dict | None = None) -> bytes:
    """Fetch once; later runs read the cache (pass --refresh to the CLI to drop it)."""
    RAW.mkdir(parents=True, exist_ok=True)
    path = RAW / name
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        return path.read_bytes()
    body = _get(url, data, headers)
    path.write_bytes(body)
    _record(name, url, body)
    return body


# --- Binance public data bucket (CEX reference for SOL fair value) ---------


def binance_klines(symbol: str, interval: str, day: date | None = None, month: str | None = None) -> list[dict]:
    """Klines from data.binance.vision (keyless, S3-backed, checksummed).

    The REST API (api.binance.com) is geo-blocked from US IPs (HTTP 451); the
    bucket is not. Columns: open_time, o, h, l, c, volume, close_time,
    quote_volume, trades, taker_buy_base, taker_buy_quote, ignore. Open times
    are microseconds for spot files since 2025, milliseconds before.
    """
    if day is not None:
        stem = f"{symbol}-{interval}-{day.isoformat()}"
        url = f"{BINANCE_VISION}/daily/klines/{symbol}/{interval}/{stem}.zip"
    else:
        stem = f"{symbol}-{interval}-{month}"
        url = f"{BINANCE_VISION}/monthly/klines/{symbol}/{interval}/{stem}.zip"
    body = cached(f"binance/{stem}.zip", url)
    expected = cached(f"binance/{stem}.zip.CHECKSUM", url + ".CHECKSUM").decode().split()[0]
    got = hashlib.sha256(body).hexdigest()
    if got != expected:
        raise ValueError(f"{stem}: checksum mismatch {got} != {expected}")
    with zipfile.ZipFile(io.BytesIO(body)) as z:
        text = z.read(z.namelist()[0]).decode()
    rows = []
    for r in csv.reader(io.StringIO(text)):
        if not r or not r[0].isdigit():
            continue
        t = int(r[0])
        t_s = t / 1e6 if t > 1e14 else t / 1e3
        rows.append(
            dict(t=t_s, o=float(r[1]), h=float(r[2]), l=float(r[3]), c=float(r[4]),
                 vol=float(r[5]), trades=int(r[8]), taker_buy=float(r[9]))
        )
    return rows


# --- Phoenix perps public API (Phoenix Eternal: SPCX, SOL, ...) -----------


def phoenix_exchange() -> dict:
    return json.loads(cached("phoenix/exchange.json", f"{PHOENIX_PERP_API}/exchange"))


def phoenix_candles(symbol: str, timeframe: str, start: datetime, end: datetime) -> list[dict]:
    """Phoenix perp candles: trade OHLC, mark OHLC, volume, tradeCount.

    `/candles?symbol=&timeframe=&startTime=&endTime=` (ms), at most ~1000
    bars per call; we page forward. Keyless, unpublished rate limit: we keep
    to ~3 req/s.
    """
    step = {"1m": 60, "5m": 300, "1h": 3600}[timeframe]
    out: dict[int, dict] = {}
    t = int(start.timestamp())
    stop = int(end.timestamp())
    while t < stop:
        t2 = min(stop, t + 1000 * step)
        name = f"phoenix/candles_{symbol}_{timeframe}_{t}_{t2}.json"
        url = f"{PHOENIX_PERP_API}/candles?symbol={symbol}&timeframe={timeframe}&startTime={t*1000}&endTime={t2*1000}"
        for c in json.loads(cached(name, url)):
            out[c["time"]] = c
        t = t2
    return [out[k] for k in sorted(out)]


# --- Solana RPC (public endpoint, ~40 req/10 s per IP, no archive guarantees) --


def rpc(name: str, method: str, params: list) -> dict:
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    raw = cached(f"rpc/{name}.json", SOLANA_RPC, data=body, headers={"Content-Type": "application/json"})
    d = json.loads(raw)
    if "error" in d:
        raise RuntimeError(f"rpc {method}: {d['error']}")
    return d["result"]


def jup_quote(name: str, input_mint: str, output_mint: str, amount: int) -> dict:
    url = f"{JUP_QUOTE}?inputMint={input_mint}&outputMint={output_mint}&amount={amount}&slippageBps=100"
    return json.loads(cached(f"jupiter/{name}.json", url))


def utc(y: int, m: int, d: int) -> datetime:
    return datetime(y, m, d, tzinfo=timezone.utc)


def days(end: date, n: int) -> list[date]:
    return [end - timedelta(days=i) for i in range(n - 1, -1, -1)]

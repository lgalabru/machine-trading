"""What one calibration run pulls. Kept small: ~25 MB, ~120 requests."""

from __future__ import annotations

from datetime import date, timedelta

from . import fetch

# Fixed windows so a calibration is reproducible from the manifest.
SOL_1S_END = date(2026, 11, 9)
SOL_1S_DAYS = 7  # fit on the first 5, hold out the last 2 for the D4 demo
SOL_1M_MONTHS = ["2026-05", "2026-06", "2026-07", "2026-08", "2026-09", "2026-10"]
PHOENIX_START = fetch.utc(2026, 10, 12)
PHOENIX_END = fetch.utc(2026, 11, 11)
JUP_SIZES_SOL = [1, 10, 100, 1_000, 10_000, 50_000]
PHOENIX_MARKETS = {"SPCX": "HCPybc8PFzS3gL2AHWXtT7jT1WDYSva7A6qf9aVGiLMf", "SOL": "71Si24E4uc3oCaPbPZTozC1ptSNNqygjjebxSmErSsC2"}


def sol_1s() -> list[list[dict]]:
    return [fetch.binance_klines("SOLUSDT", "1s", day=d) for d in fetch.days(SOL_1S_END, SOL_1S_DAYS)]


def sol_1m() -> list[dict]:
    rows = []
    for m in SOL_1M_MONTHS:
        rows.extend(fetch.binance_klines("SOLUSDT", "1m", month=m))
    return rows


def phoenix(symbol: str) -> list[dict]:
    return fetch.phoenix_candles(symbol, "1m", PHOENIX_START, PHOENIX_END)


def jupiter_curve() -> dict:
    """Aggregated Solana DEX liquidity for SOL/USDC as Jupiter routes it: the
    effective price for each size, both ways. One snapshot, so it shows the
    curve's shape, not its variation (that needs repeated sampling)."""
    out = {"sell_sol": [], "buy_sol": []}
    usd_per_sol = None
    for n in JUP_SIZES_SOL:
        q = fetch.jup_quote(f"sell_{n}", fetch.SOL_MINT, fetch.USDC_MINT, n * 10**9)
        px = int(q["outAmount"]) / 1e6 / n
        usd_per_sol = usd_per_sol or px
        out["sell_sol"].append(dict(size_sol=n, px=px, impact_pct=float(q["priceImpactPct"]) * 100, hops=len(q["routePlan"])))
    for n in JUP_SIZES_SOL:
        usd = int(n * usd_per_sol * 10**6)
        q = fetch.jup_quote(f"buy_{n}", fetch.USDC_MINT, fetch.SOL_MINT, usd)
        px = usd / 1e6 / (int(q["outAmount"]) / 1e9)
        out["buy_sol"].append(dict(size_sol=n, px=px, impact_pct=float(q["priceImpactPct"]) * 100, hops=len(q["routePlan"])))
    return out


def priority_fees() -> list[dict]:
    """Per-slot minimum non-zero priority fee paid by txs that write the given
    accounts, over the RPC node's recent window (~150 slots)."""
    return fetch.rpc("prio_phoenix", "getRecentPrioritizationFees", [list(PHOENIX_MARKETS.values())])


def market_activity(symbol: str) -> list[dict]:
    return fetch.rpc(f"sigs_{symbol}", "getSignaturesForAddress", [PHOENIX_MARKETS[symbol], {"limit": 1000}])


def jitosol() -> dict:
    """Stake-pool exchange rate (SPL stake pool: total_lamports and
    pool_token_supply at byte offsets 258 and 266) vs the market's price."""
    import base64
    import struct

    acc = fetch.rpc("jito_stake_pool", "getAccountInfo", [fetch.JITO_STAKE_POOL, {"encoding": "base64"}])
    raw = base64.b64decode(acc["value"]["data"][0])
    total, supply, last_epoch = struct.unpack_from("<QQQ", raw, 258)
    rate = total / supply
    q = fetch.jup_quote("jitosol_sell_100", fetch.JITOSOL_MINT, fetch.SOL_MINT, 100 * 10**9)
    mkt = int(q["outAmount"]) / 1e9 / 100
    return dict(pool_rate_sol=rate, market_rate_sol=mkt, discount_bps=(rate - mkt) / rate * 1e4,
                last_update_epoch=last_epoch, slot=acc["context"]["slot"])


def jitosol_history() -> list[dict]:
    """Daily jitoSOL pool stats from Jito's public API (keyless): the
    exchange rate is tvl (lamports) / 1e9 / supply."""
    import json as _json

    body = _json.dumps({"bucket_type": "Daily",
                        "range_filter": {"start": "2026-08-01T00:00:00Z", "end": "2026-11-10T00:00:00Z"},
                        "sort_by": {"field": "BlockTime", "order": "Asc"}}).encode()
    d = _json.loads(fetch.cached("jito/stake_pool_stats_2026-08-01_2026-11-10.json",
                                 "https://kobe.mainnet.jito.network/api/v1/stake_pool_stats", data=body,
                                 headers={"Content-Type": "application/json"}))
    sup = {x["date"][:10]: x["data"] for x in d["supply"]}
    return [dict(date=x["date"][:10], rate=x["data"] / 1e9 / sup[x["date"][:10]])
            for x in d["tvl"] if sup.get(x["date"][:10])]


def jito_tip_floor() -> dict:
    import json as _json

    return _json.loads(fetch.cached("jito/tip_floor.json", "https://bundles.jito.wtf/api/v1/bundles/tip_floor"))[0]

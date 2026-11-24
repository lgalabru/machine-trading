#!/usr/bin/env python3
"""List a Kamino lending market's obligations worth watching, for the live
runtime's `control.lending` (`live::lending`).

The runtime watches obligations it is told of; this finds them, once. It
asks the node for every obligation of one market (`getProgramAccounts`,
filtered by size and market, sliced to the four totals the program keeps),
keeps those with debt and a health near 1, and writes their addresses,
the least healthy first.

On a node without an account index the node scans every account for it:
minutes, and replay may lag while it runs. Run it when that is fine.

    scan-obligations.py --market 7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF \
        --out ~/mtx/obligations.json
"""
import argparse
import base64
import json
import sys
import time
import urllib.request

KLEND = "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD"
# klend's layout (`crates/runtime/src/venue/kamino/klend.rs`).
OBLIGATION_LEN = 3344
MARKET_AT = 32
# Four 16-byte fixed-point totals: debt at its borrow factor, debt at
# market, the borrow limit, and the liquidation limit.
TOTALS_AT = 2208
TOTALS_LEN = 64
SF = float(1 << 60)
# The most obligations written out: the runtime's own bound is lower.
OBLIGATIONS_MAX = 16384


def totals(data):
    """The debt at its borrow factor and the liquidation limit, in USD."""
    assert len(data) == TOTALS_LEN, len(data)
    read = lambda at: int.from_bytes(data[at:at + 16], "little") / SF
    return read(0), read(48)


def main():
    args = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    args.add_argument("--rpc", default="http://127.0.0.1:8899")
    args.add_argument("--market", required=True)
    args.add_argument("--out", required=True)
    args.add_argument("--debt-usd-min", type=float, default=1000.0,
                      help="the least debt worth a liquidator's fee")
    args.add_argument("--health-max", type=float, default=1.25,
                      help="liquidation limit over debt: 1 is liquidatable")
    args.add_argument("--timeout-s", type=float, default=900.0)
    args = args.parse_args()
    request = {
        "jsonrpc": "2.0", "id": 1, "method": "getProgramAccounts",
        "params": [KLEND, {
            "encoding": "base64",
            "commitment": "confirmed",
            "dataSlice": {"offset": TOTALS_AT, "length": TOTALS_LEN},
            "filters": [
                {"dataSize": OBLIGATION_LEN},
                {"memcmp": {"offset": MARKET_AT, "bytes": args.market}},
            ],
        }],
    }
    started = time.time()
    call = urllib.request.Request(
        args.rpc, data=json.dumps(request).encode(),
        headers={"content-type": "application/json"})
    with urllib.request.urlopen(call, timeout=args.timeout_s) as answer:
        reply = json.load(answer)
    if "error" in reply:
        sys.exit(f"the node refused: {reply['error']}")
    accounts = reply["result"]
    kept = []
    for account in accounts:
        debt, limit = totals(base64.b64decode(account["account"]["data"][0]))
        if debt >= args.debt_usd_min and limit / debt <= args.health_max:
            kept.append({"address": account["pubkey"], "debt_usd": round(debt, 2),
                         "health": round(limit / debt, 6)})
    kept.sort(key=lambda o: o["health"])
    kept = kept[:OBLIGATIONS_MAX]
    with open(args.out, "w") as out:
        json.dump({"market": args.market, "scanned": len(accounts),
                   "took_s": round(time.time() - started, 1),
                   "debt_usd_min": args.debt_usd_min, "health_max": args.health_max,
                   "obligations": kept}, out, indent=1)
    print(f"{len(accounts)} obligations scanned in {time.time() - started:.0f} s; "
          f"{len(kept)} kept in {args.out}")


if __name__ == "__main__":
    main()

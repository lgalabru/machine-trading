"""Checks a session's RFQ desk accounting against the chain's record.

Takes the report of a graded session with RFQ clients, `"record": true`,
and a bot that never trades the perp. Its account then changes only by its
desk: the asset it holds moving with the index, and each fill's swap. The
check rebuilds both from the tape and the desk's balances:

- every settled swap moved exactly the agreed amounts (the asset rounded
  down to its last unit, the USDC to its nearest), and the desk's balances
  at the end are the start's plus those moves;
- the session's PnL is the asset's move at the index, plus each fill's
  USDC less the asset it gave up, at the end's index: nothing counted
  twice, nothing left out.

With `--idle`, the bot must not have filled at all: its PnL is the index's
move on the asset it started with, and nothing else.

Usage: desk.py <report.json> [--idle]
"""

import argparse
import json
import math
import sys

DECIMALS = 6
# The report's values are floats summed from token units: a cent covers it.
TOLERANCE_USD = 0.01


def swapped(entry):
    """The bot's change in asset and USDC units from one settled swap."""
    scale = 10**DECIMALS
    base = math.floor(entry["size"] * scale + 1e-6)
    usdc = round(base * entry["price_usd"])
    # The client's side: it buys (`bid`) the asset from the bot.
    sign = -1 if entry["side"] == "bid" else 1
    return sign * base / scale, -sign * usdc / scale


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("report")
    parser.add_argument("--idle", action="store_true")
    args = parser.parse_args()
    session = json.load(open(args.report))["scenarios"][0]
    library = session["actors"]
    desk = library["desk"]
    clients = {w for a in library["actors"] if a["kind"] == "rfq-client" for w in a["wallets"]}
    fills = [e for e in library["tape"] if e["action"] == "swap" and e["ok"] and e["wallet"] in clients]
    base = sum(swapped(e)[0] for e in fills)
    usdc = sum(swapped(e)[1] for e in fills)
    index_move = desk["start_base"] * (desk["end_index_usd"] - desk["start_index_usd"])
    fills_usd = usdc + base * desk["end_index_usd"]
    expected = index_move + fills_usd
    found = {
        "fills": len(fills),
        "pnl_usd": round(session["pnl_usd"], 4),
        "expected_usd": round(expected, 4),
        "index_move_usd": round(index_move, 4),
        "fills_usd": round(fills_usd, 4),
        "desk": desk,
    }
    print(json.dumps(found, indent=1))
    problems = []
    if session["end_position"] != 0 or session.get("peak_position", 0) != 0:
        problems.append("the bot traded the perp")
    if abs(desk["end_base"] - desk["start_base"] - base) > 0.5 / 10**DECIMALS:
        problems.append(f"the asset moved {desk['end_base'] - desk['start_base']}, the swaps {base}")
    if abs(desk["end_usdc"] - desk["start_usdc"] - usdc) > 0.5 / 10**DECIMALS:
        problems.append(f"USDC moved {desk['end_usdc'] - desk['start_usdc']}, the swaps {usdc}")
    if abs(session["pnl_usd"] - expected) > TOLERANCE_USD:
        problems.append(f"PnL {session['pnl_usd']} isn't the desk's {expected}")
    if args.idle and fills:
        problems.append(f"an idle bot filled {len(fills)} requests")
    if not args.idle and not fills:
        problems.append("no request filled")
    if problems:
        sys.exit("desk: " + "; ".join(problems))


if __name__ == "__main__":
    main()

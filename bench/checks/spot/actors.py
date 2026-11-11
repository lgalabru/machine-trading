"""Checks the actors smoke (`just actors-smoke`) from its detail files.

    python3 bench/checks/spot/actors.py DETAILS_DIR

DETAILS_DIR holds one detail directory per scenario (actors-seller,
actors-path, actors-tape, actors-buyer, actors-buyer-ahead), as `mtx grade --details` writes them.
"""

import gzip
import json
import os
import sys


def report(details, name):
    with gzip.open(os.path.join(details, name, "bot", name + ".json.gz")) as f:
        detail = json.load(f)
    return dict(detail["report"], timeline=detail["timeline"])


def fills(r, side):
    return [f for f in r["spot"]["fills"] if f["side"] == side]


def average(fs):
    base = sum(f["base"] for f in fs)
    return sum(f["price"] * f["base"] for f in fs) / base if base else float("nan")


def check(ok, what):
    print(("ok   " if ok else "FAIL ") + what)
    return ok


def main(details):
    good = True
    seller = report(details, "actors-seller")
    actors = seller["actors"]
    start = seller["start_mark_usd"]
    maker = {m["venue"]: m for m in actors["makers"]}
    phoenix = maker["phoenix"]["steps"]
    sold = fills(seller, "sell")
    print(f"seller: {len(sold)} fills, {sum(f['base'] for f in sold):.2f} SOL at {average(sold):.4f}; "
          f"Pyth {start:.4f}; reference {actors['reference_usd'][0]:.4f} -> {actors['reference_usd'][-1]:.4f}")
    for venue, m in maker.items():
        print(f"  {venue} maker: cleared {m['cleared_bought']:.2f} bought / {m['cleared_sold']:.2f} sold; "
              f"traded {m['bought']:.2f} bought / {m['sold']:.2f} sold")
    print("  phoenix bid ladder by step: " + " ".join(f"{s['bid_base']:.1f}" for s in phoenix))
    print(f"  actions {actors['actions']}")
    good &= check(actors["actions"]["failed"] == 0, "no actor action failed")
    good &= check(abs(sum(f["base"] for f in sold) - 12.0) < 1e-6, "the seller sold its 12 SOL")
    good &= check(all(f["price"] > start * 0.99 for f in sold),
                  "every sale filled within 1% of the scenario's level, none on the stale book")
    good &= check(abs(phoenix[0]["bid_usd"] / start - 1 + 0.001) < 0.0005,
                  "the makers' first bid sits 10 bps under the level")
    good &= check(maker["phoenix"]["bought"] > 11.9, "Phoenix's maker bought what the seller sold")
    thinnest = min(s["bid_base"] for s in phoenix)
    good &= check(thinnest < 10.0 * 0.7, f"the bid ladder thinned while the seller sold (down to {thinnest:.2f} of 10)")
    good &= check(abs(phoenix[-1]["bid_base"] - 10.0) < 1e-9, "and refilled once it stopped")
    good &= check(actors["reference_usd"][-1] < actors["reference_usd"][0], "the selling moved the reference down")
    good &= check(seller["timeline"]["steps"][-1]["level_usd"] == actors["reference_usd"][-1],
                  "the timeline's level is the reference")

    moved = report(details, "actors-path")
    end = moved["spot"]["end"]["price_usd"]
    refs = moved["actors"]["reference_usd"]
    print(f"path: Pyth {moved['spot']['start']['price_usd']:.4f} -> {end:.4f}; reference {refs[0]:.4f} -> {refs[-1]:.4f}")
    good &= check(abs(end / (start * 0.96) - 1) < 1e-6, "Pyth followed the path to -4%")
    held = moved["spot"]["end"]
    good &= check(abs(moved["end_value_usd"] - (held["base"] * end + held["quote"])) < 0.01,
                  "the account is valued there")
    good &= check(refs[-1] < start * 0.961 and refs[0] > start * 0.999, "the makers' reference followed it")
    good &= check(moved["actors"]["actions"]["failed"] == 0, "no actor action failed")

    busy = report(details, "actors-tape")
    tape, acts = busy["actors"]["tape"], busy["actors"]["actions"]
    total = acts["placed"] + acts["cancelled"] + acts["took"] + acts["cranked"]
    ob = {m["venue"]: m for m in busy["actors"]["makers"]}["openbook"]
    print(f"tape: {tape['sent']} of {tape['orders']} orders sent, {tape['failed']} failed; "
          f"OpenBook's maker traded {ob['bought']:.1f} bought / {ob['sold']:.1f} sold; actions {acts}")
    good &= check(tape["sent"] == tape["orders"] and tape["failed"] == 0, "every tape order went out")
    good &= check(ob["bought"] + ob["sold"] > 100, "the tape filled OpenBook's maker many times over")
    full = all(len([1 for _ in m["steps"]]) > 50 for m in busy["actors"]["makers"])
    good &= check(full and acts["placed"] >= 2 * 24 * 50, "both makers re-placed 24 orders a step for 50+ steps")
    good &= check(acts["failed"] < 0.01 * total, "under 1% of the actors' actions failed")

    alone = report(details, "actors-buyer")
    ahead = report(details, "actors-buyer-ahead")
    bought_alone, bought_ahead = fills(alone, "buy"), fills(ahead, "buy")
    them = ahead["actors"]["counterparties"][0]
    print(f"buyer alone: {sum(f['base'] for f in bought_alone):.2f} SOL at {average(bought_alone):.4f}; "
          f"with a counterparty ahead: {sum(f['base'] for f in bought_ahead):.2f} SOL at {average(bought_ahead):.4f}")
    print(f"  counterparty: {len(them['trades'])} takes, bought {them['bought']:.2f}; "
          "at " + ", ".join(f"{t['at']}:{t['base']:.2f}" for t in them["trades"]))
    good &= check(ahead["actors"]["actions"]["failed"] == 0 and alone["actors"]["actions"]["failed"] == 0,
                  "no actor action failed")
    good &= check(not alone["actors"].get("counterparties"), "alone, no counterparty")
    good &= check(them["bought"] > 0 and them["trades"][0]["at"] > bought_ahead[0]["at"] - 4,
                  "the counterparty starts buying after the bot's first fill")
    good &= check(them["trades"][-1]["at"] <= bought_ahead[-1]["at"] + 2 * 2 + 2,
                  "and stands down once the bot stops")
    good &= check(average(bought_ahead) > average(bought_alone),
                  "the bot pays more with the counterparty ahead of it")
    print("ACTORS_SMOKE " + ("PASS" if good else "FAIL"))
    return 0 if good else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))

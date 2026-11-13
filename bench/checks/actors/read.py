"""A reader of the library's actors, from what the chain shows.

Takes a graded session of a scenario with `"record": true` (its report and
details) and reads the actors as anyone watching the chain could: every
wallet's orders, cancels, takes and swaps, the bot's position, and the
book's prices, step by step. Which wallet is which actor is used only to
score the reader, and to give the "told" strategy its answer key.

1. **Telling them apart.** Each wallet that acted is classified from its own
   record alone (orders that live and get pulled, swaps with the bot, takes
   that come in reversed pairs, takes that follow the bot, takes against the
   index, takes with the chart, takes that a later price rewards, a record
   that stops being rewarded), and the classes are checked against the
   truth. A wallet whose orders never landed shows nothing to read, and
   isn't counted.
2. **What reading pays.** Three strategies trade one unit every `--every`
   slots, on what the last `--lookback` slots showed, and are marked out
   `--horizon` slots later, net of the half spread then:
   - **told** follows the net flow of the wallets that are informed (the
     answer key);
   - **careful blind** reads no wallet: the better (by total) of following
     the market's net taker flow and following the price's move;
   - **reader** follows the net flow of the wallets whose takes, already
     marked out, have been rewarded lately: it learns them online.
   Reading pays when told makes at least twice what the careful blind does
   (`--require-edge` fails the check otherwise).
   Spoofing is read as support: a follower of walls buys one unit as a big
   bid goes up (sells as an ask does), marked out the same way. The blind
   one follows every wall; the reader fades the walls of wallets it has
   seen pull most of their orders. The edge is the reader's PnL over the
   blind one's.
   Every strategy decides from what was public before its decision.

Usage: read.py <report.json> <details.json.gz> [--horizon 40]
           [--lookback 40] [--every 4] [--require-edge]
"""

import argparse
import bisect
import gzip
import json
import statistics
import sys
from collections import defaultdict

TICK = 0.01


def load(report_path, details_path):
    report = json.load(open(report_path))
    session = report["scenarios"][0]
    with gzip.open(details_path) as f:
        details = json.load(f)
    steps = details["timeline"]["steps"]
    library = session["actors"]
    truth = {w: actor["kind"] for actor in library["actors"] for w in actor["wallets"]}
    return library["tape"], steps, truth


class Book:
    """The book's public prices by slot into the session."""

    def __init__(self, steps):
        self.ats = [s["at"] for s in steps]
        self.steps = steps

    def at(self, at):
        i = max(0, bisect.bisect_right(self.ats, at) - 1)
        return self.steps[i]

    def mid(self, at):
        s = self.at(at)
        bid, ask = s.get("best_bid_usd"), s.get("best_ask_usd")
        if bid and ask:
            return (bid + ask) / 2
        return s["mark_usd"]

    def half_spread_bps(self, at):
        s = self.at(at)
        bid, ask = s.get("best_bid_usd"), s.get("best_ask_usd")
        if bid and ask:
            return (ask - bid) / 2 / ((ask + bid) / 2) * 1e4
        return 0.0

    def bot_moved(self, at, back):
        """The bot's position change over the `back` slots before `at`."""
        return self.at(at)["position"] - self.at(max(0, at - back))["position"]

    def end(self):
        return self.ats[-1]


def moved_bps(book, at, back):
    """The price's move over the `back` slots to `at`."""
    before = book.mid(max(0, at - back))
    return (book.mid(at) - before) / before * 1e4


def markout_bps(book, at, side, horizon):
    sign = 1 if side == "bid" else -1
    now, later = book.mid(at), book.mid(at + horizon)
    return sign * (later - now) / now * 1e4


def features(entries, book, horizon):
    rests = [e for e in entries if e["action"] == "rest"]
    cancels = [e for e in entries if e["action"] == "cancel"]
    takes = [e for e in entries if e["action"] == "take"]
    swaps = [e for e in entries if e["action"] == "swap"]
    lives = []
    for r in rests:
        later = [c["at"] for c in cancels if c["at"] >= r["at"]]
        if later:
            lives.append(min(later) - r["at"])
    # Takes reversed later by an equal take the other way: the opening leg
    # of each pair, and whether it went the bot's way just before it.
    opens = []
    used = set()
    for i, t in enumerate(takes):
        if i in used:
            continue
        for j in range(i + 1, len(takes)):
            u = takes[j]
            if j in used or u["side"] == t["side"]:
                continue
            if abs(u["size"] - t["size"]) < 1e-9 and u["at"] > t["at"]:
                used.update((i, j))
                opens.append(t)
                break
    follows = [t for t in opens if book.bot_moved(t["at"], 6) != 0]
    with_bot = sum(
        1
        for t in follows
        if (book.bot_moved(t["at"], 6) > 0) == (t["side"] == "bid")
    )
    against_index = 0
    for t in takes:
        s = book.at(t["at"])
        off = book.mid(t["at"]) - s["index_usd"]
        if off != 0 and (off < 0) == (t["side"] == "bid"):
            against_index += 1
    trended = [t for t in takes if abs(moved_bps(book, t["at"], horizon)) > 2]
    with_trend = sum(1 for t in trended if (moved_bps(book, t["at"], horizon) > 0) == (t["side"] == "bid"))
    marked = [
        markout_bps(book, t["at"], t["side"], horizon)
        for t in takes
        if t["at"] + horizon <= book.end()
    ]
    hits = sum(1 for m in marked if m > 0)
    half = len(marked) // 2
    return {
        "swaps": len(swaps),
        "rests": len(rests),
        "cancels": len(cancels),
        "takes": len(takes),
        "life": statistics.mean(lives) if lives else None,
        "paired": 2 * len(opens) / len(takes) if takes else 0.0,
        "with_bot": with_bot / len(follows) if follows else 0.0,
        "follows": len(follows),
        "against_index": against_index / len(takes) if takes else 0.0,
        "with_trend": with_trend / len(trended) if trended else 0.0,
        "markout": statistics.mean(marked) if marked else 0.0,
        "markout_early": statistics.mean(marked[:half]) if half else 0.0,
        "markout_late": statistics.mean(marked[half:]) if half else 0.0,
        "hit_rate": hits / len(marked) if marked else 0.0,
        "marked": len(marked),
    }


def classify(f):
    if f["swaps"]:
        return "rfq-client"
    if f["rests"]:
        if f["cancels"] >= 0.8 * f["rests"]:
            return "spoofer"
    if not f["takes"]:
        return "idle"
    if f["paired"] >= 0.8 and f["follows"] >= 4 and f["with_bot"] >= 0.8:
        return "front-runner"
    if f["paired"] >= 0.8:
        return "stop-hunter"
    if f["against_index"] >= 0.9 and f["takes"] >= 5:
        return "arbitrageur"
    if f["marked"] >= 6 and f["markout_early"] >= 5 and f["markout_late"] < 2:
        return "decoy"
    if f["markout"] >= 5 and f["hit_rate"] >= 0.75:
        return "informed"
    if f["with_trend"] >= 0.9 and f["takes"] >= 5:
        return "chaser"
    return "noise"


def tell_apart(tape, book, truth, horizon):
    by_wallet = defaultdict(list)
    for e in tape:
        if e["ok"]:
            by_wallet[e["wallet"]].append(e)
    rows = []
    for wallet, entries in sorted(by_wallet.items()):
        f = features(entries, book, horizon)
        rows.append((truth.get(wallet, "?"), classify(f), wallet, f))
    return rows


def strategies(tape, book, truth, horizon, lookback, every):
    """Told, careful blind and reader: each one's PnL per decision, in bps
    of a unit, marked out `horizon` slots on net of the half spread."""
    takes = sorted(
        (e for e in tape if e["ok"] and e["action"] == "take"), key=lambda e: e["at"]
    )
    end = book.end()

    def signed(t):
        return t["size"] if t["side"] == "bid" else -t["size"]

    def pnl(at, net):
        side = "bid" if net > 0 else "ask"
        return markout_bps(book, at, side, horizon) - book.half_spread_bps(at)

    told, flow, momentum, reader = [], [], [], []
    for at in range(lookback, end - horizon + 1, every):
        window = [t for t in takes if at - lookback <= t["at"] < at]
        net = sum(signed(t) for t in window if truth.get(t["wallet"]) == "informed")
        if net:
            told.append(pnl(at, net))
        net = sum(signed(t) for t in window)
        if net:
            flow.append(pnl(at, net))
        moved = moved_bps(book, at, lookback)
        if abs(moved) > 2:
            momentum.append(pnl(at, moved))
        # The reader trusts a wallet whose last takes that have marked out
        # by now were, on average and mostly, rewarded.
        trusted = set()
        record = defaultdict(list)
        for t in takes:
            if t["at"] + horizon <= at:
                record[t["wallet"]].append(markout_bps(book, t["at"], t["side"], horizon))
        for wallet, marks in record.items():
            recent = marks[-READER_MEMORY:]
            hits = sum(1 for m in recent if m > 0)
            if len(recent) >= 3 and statistics.mean(recent) > 5 and hits >= 0.6 * len(recent):
                trusted.add(wallet)
        net = sum(signed(t) for t in window if t["wallet"] in trusted)
        if net:
            reader.append(pnl(at, net))
    blind = max((("flow", flow), ("momentum", momentum)), key=lambda p: sum(p[1]))
    return {
        "told": stats(told),
        "careful_blind": {"uses": blind[0], **stats(blind[1])},
        "blind_flow": stats(flow),
        "blind_momentum": stats(momentum),
        "reader": stats(reader),
    }


# How many of a wallet's latest marked-out takes the reader judges it by.
READER_MEMORY = 8


def walls(tape, book, horizon):
    """Followers of walls, blind and read: each one's PnL per wall."""
    takes = [e for e in tape if e["ok"] and e["action"] == "take"]
    orders = sorted(
        (e for e in tape if e["ok"] and e["action"] in ("rest", "cancel")),
        key=lambda e: e["at"],
    )
    sizes = [t["size"] for t in takes]
    big = 5 * statistics.median(sizes) if sizes else 0.0
    blind, reader = [], []
    for r in orders:
        if r["action"] != "rest" or r["size"] < big or r["at"] + horizon > book.end():
            continue
        def pnl(side):
            return markout_bps(book, r["at"], side, horizon) - book.half_spread_bps(r["at"])

        blind.append(pnl(r["side"]))
        past = [e for e in orders if e["wallet"] == r["wallet"] and e["at"] < r["at"]]
        placed = sum(1 for e in past if e["action"] == "rest")
        pulled = sum(1 for e in past if e["action"] == "cancel")
        spoofs = placed >= 3 and pulled >= 0.8 * placed
        reader.append(pnl(("ask" if r["side"] == "bid" else "bid") if spoofs else r["side"]))
    b, r = stats(blind), stats(reader)
    return {"blind": b, "reader": r, "edge_total_bps": round(r["total_bps"] - b["total_bps"], 1)}


def stats(xs):
    return {
        "trades": len(xs),
        "mean_bps": round(statistics.mean(xs), 2) if xs else 0.0,
        "total_bps": round(sum(xs), 1),
    }


def edge(table):
    """Whether told makes at least twice what the careful blind does."""
    told, blind = table["told"]["total_bps"], table["careful_blind"]["total_bps"]
    return {
        "told_over_blind": round(told / blind, 2) if blind > 0 else None,
        "pass": told > 0 and told >= 2 * max(blind, 0.0),
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("report")
    parser.add_argument("details")
    parser.add_argument("--horizon", type=int, default=40)
    parser.add_argument("--lookback", type=int, default=40)
    parser.add_argument("--every", type=int, default=4)
    parser.add_argument("--require-edge", action="store_true")
    args = parser.parse_args()
    tape, steps, truth = load(args.report, args.details)
    book = Book(steps)
    rows = tell_apart(tape, book, truth, args.horizon)
    right = sum(1 for kind, guess, _, _ in rows if kind == guess)
    by_kind = defaultdict(lambda: [0, 0])
    for kind, guess, _, _ in rows:
        by_kind[kind][1] += 1
        by_kind[kind][0] += kind == guess
    table = strategies(tape, book, truth, args.horizon, args.lookback, args.every)
    gate = edge(table)
    print(
        json.dumps(
            {
                "wallets": len(rows),
                "classified_right": right,
                "by_kind": {k: f"{v[0]}/{v[1]}" for k, v in sorted(by_kind.items())},
                "wallets_read": [
                    {"truth": kind, "read": guess, **{k: (round(v, 2) if isinstance(v, float) else v) for k, v in f.items()}}
                    for kind, guess, _, f in rows
                ],
                "pnl": table,
                "edge": gate,
                "spoofing": walls(tape, book, args.horizon),
            },
            indent=1,
        )
    )
    if args.require_edge and not gate["pass"]:
        sys.exit(f"reading doesn't pay: told {table['told']['total_bps']} bps, careful blind {table['careful_blind']['total_bps']} bps")


if __name__ == "__main__":
    main()

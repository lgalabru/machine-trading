"""A reader of the library's actors, from what the chain shows.

Takes a graded session of a scenario with `"record": true` (its report and
details) and reads the actors as anyone watching the chain could: every
wallet's orders, cancels and takes, the bot's position, and the book's
prices, step by step. Which wallet is which actor is used only to score the
reader, never by it.

1. **Telling them apart.** Each wallet that acted is classified from its own
   record alone (orders that live and get pulled, orders that last no slot,
   takes that come in reversed pairs, takes that follow the bot, takes
   against the index, takes that a later price rewards), and the classes are
   checked against the truth. A wallet whose orders never landed shows
   nothing to read, and isn't counted.
2. **The edge of reading.** Two followers trade one unit on every signal,
   marked out `--horizon` slots later net of the half spread:
   - informed flow: the blind follower copies every taker; the reader copies
     only wallets whose past takes, already marked out, were rewarded;
   - spoofing: the blind follower buys when a large bid appears near the
     touch, as support; the reader does so only for wallets it hasn't seen
     pull most of their orders.
   Both readers decide on each signal from what was public before it.

Usage: read.py <report.json> <details.json.gz> [--horizon 40]
"""

import argparse
import bisect
import gzip
import json
import statistics
from collections import defaultdict

TICK = 0.01


def load(report_path, details_path):
    report = json.load(open(report_path))
    session = report["scenarios"][0]
    with gzip.open(details_path) as f:
        details = json.load(f)
    steps = details["timeline"]["steps"]
    library = session["actors"]
    truth = {}
    for actor in library["actors"]:
        for wallet in actor["wallets"]:
            truth[wallet] = actor["kind"]
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


def markout_bps(book, at, side, horizon):
    sign = 1 if side == "bid" else -1
    now, later = book.mid(at), book.mid(at + horizon)
    return sign * (later - now) / now * 1e4


def features(entries, book, horizon):
    rests = [e for e in entries if e["action"] == "rest"]
    cancels = [e for e in entries if e["action"] == "cancel"]
    takes = [e for e in entries if e["action"] == "take"]
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
    marked = [
        markout_bps(book, t["at"], t["side"], horizon)
        for t in takes
        if t["at"] + horizon <= book.end()
    ]
    hits = sum(1 for m in marked if m > 0)
    return {
        "rests": len(rests),
        "cancels": len(cancels),
        "takes": len(takes),
        "life": statistics.mean(lives) if lives else None,
        "paired": 2 * len(opens) / len(takes) if takes else 0.0,
        "with_bot": with_bot / len(follows) if follows else 0.0,
        "follows": len(follows),
        "against_index": against_index / len(takes) if takes else 0.0,
        "markout": statistics.mean(marked) if marked else 0.0,
        "hit_rate": hits / len(marked) if marked else 0.0,
        "marked": len(marked),
    }


def classify(f):
    if f["rests"]:
        if f["life"] is not None and f["life"] == 0:
            return "rfq-client"
        if f["cancels"] >= 0.8 * f["rests"]:
            return "spoofer"
    if not f["takes"]:
        return "idle"
    if f["paired"] >= 0.8 and f["follows"] >= 4 and f["with_bot"] >= 0.8:
        return "front-runner"
    if f["paired"] >= 0.8:
        return "stop-hunter"
    if f["against_index"] >= 0.9 and f["markout"] < 5:
        return "arbitrageur"
    if f["markout"] >= 5 and f["hit_rate"] >= 0.75:
        return "informed"
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


def follow_edges(tape, book, horizon):
    """Reader vs blind followers of the informed flow and of the walls."""
    takes = sorted(
        (e for e in tape if e["ok"] and e["action"] == "take"), key=lambda e: e["at"]
    )
    rests = sorted(
        (e for e in tape if e["ok"] and e["action"] in ("rest", "cancel")),
        key=lambda e: e["at"],
    )
    end = book.end()

    def net(at, side):
        return markout_bps(book, at, side, horizon) - book.half_spread_bps(at)

    # Informed: a wallet is believed informed once its takes marked out
    # before now average over 5 bps across at least 3 of them.
    blind, reader = [], []
    for t in takes:
        if t["at"] + horizon > end:
            continue
        pnl = net(t["at"], t["side"])
        blind.append(pnl)
        known = [
            markout_bps(book, u["at"], u["side"], horizon)
            for u in takes
            if u["wallet"] == t["wallet"] and u["at"] + horizon <= t["at"]
        ]
        if len(known) >= 3 and statistics.mean(known) > 5:
            reader.append(pnl)
    informed = summary(blind, reader)

    # Spoofing: a bid order resting more than 2 ticks under the touch and
    # many times a take's size reads as support. A wallet is believed a
    # spoofer once at least 3 of its orders came and 80% of them went
    # without trading.
    sizes = [t["size"] for t in takes]
    big = 5 * statistics.median(sizes) if sizes else 0.0
    blind, reader = [], []
    for r in rests:
        if r["action"] != "rest" or r.get("side") != "bid" or r["size"] < big:
            continue
        if r["at"] + horizon > end:
            continue
        pnl = net(r["at"], "bid")
        blind.append(pnl)
        past = [e for e in rests if e["wallet"] == r["wallet"] and e["at"] < r["at"]]
        placed = sum(1 for e in past if e["action"] == "rest")
        pulled = sum(1 for e in past if e["action"] == "cancel")
        if not (placed >= 3 and pulled >= 0.8 * placed):
            reader.append(pnl)
    spoofing = summary(blind, reader)
    return informed, spoofing


def summary(blind, reader):
    def stats(xs):
        return {
            "trades": len(xs),
            "mean_bps": round(statistics.mean(xs), 2) if xs else 0.0,
            "total_bps": round(sum(xs), 1),
        }

    b, r = stats(blind), stats(reader)
    return {
        "blind": b,
        "reader": r,
        "edge_bps_per_trade": round(r["mean_bps"] - b["mean_bps"], 2),
        "edge_total_bps": round(r["total_bps"] - b["total_bps"], 1),
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("report")
    parser.add_argument("details")
    parser.add_argument("--horizon", type=int, default=40)
    args = parser.parse_args()
    tape, steps, truth = load(args.report, args.details)
    book = Book(steps)
    rows = tell_apart(tape, book, truth, args.horizon)
    right = sum(1 for kind, guess, _, _ in rows if kind == guess)
    by_kind = defaultdict(lambda: [0, 0])
    for kind, guess, _, _ in rows:
        by_kind[kind][1] += 1
        by_kind[kind][0] += kind == guess
    informed, spoofing = follow_edges(tape, book, args.horizon)
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
                "edge": {"informed": informed, "spoofing": spoofing},
            },
            indent=1,
        )
    )


if __name__ == "__main__":
    main()

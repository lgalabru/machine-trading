#!/usr/bin/env bash
# Check price impact (venues.perp.impact) end to end, on SPCX: the Surfpool
# scenario script that plays the path and keeps the book where trades left it.
#   - bots/first-strategy buys the 2% dip with 350 SPCX (about $49,000) and
#     sells the recovery. On smoke.json the buy walks the asks up, and the
#     book stays there: the makers are centred above the scenario's level by
#     what was walked, the index stays on the level, and half of the impact
#     fades. The sale walks it back. The same grade replayed twice agrees
#     (--replay 2).
#   - the same bot on control.json (no impact): the book is back on the
#     path's level at once, and no step carries an impact.
#   - bots/starter never trades: with impact and without, the book is the
#     same from the path's first move on, and the impact is 0 throughout.
#   - the same bot on noisy.json, four times: other traders trade around it,
#     differently each run. An odd lot they leave at the touch must not hide
#     the bot's sale: the book is below the level after it, every time.
#   bench/checks/impact/run.sh <mtx> <snapshot> <out>
# The bots must be built (just --justfile bots/<bot>/Justfile build). The
# snapshot is snapshots/phoenix-spcx-2026-10-29.
set -euo pipefail
mtx=$1 snapshot=$2 out=$3
here="$(cd "$(dirname "$0")" && pwd)"
root="$here/../../.."
rm -rf "$out"
mkdir -p "$out"
grade() { # name bot scenario [extra mtx flags]
  local name=$1 bot=$2 scenario=$3
  shift 3
  mkdir -p "$out/$name"
  "$mtx" grade --components-only --snapshot "$snapshot" --bot "$root/bots/$bot" --logs "$out/$name/logs" \
    --details "$out/$name/details" --report "$out/$name/report.json" "$@" "$here/$scenario.json" \
    >"$out/$name/stdout.log" 2>"$out/$name/stderr.log" || { echo "$name: the grade failed"; tail -20 "$out/$name/stderr.log"; exit 1; }
}
grade trade first-strategy smoke --replay 2
grade control first-strategy control
grade idle starter smoke
grade idle-control starter control
for run in 1 2 3 4; do grade "noisy-$run" first-strategy noisy; done
python3 -I - "$out" <<'PY'
import glob, gzip, json, sys
out = sys.argv[1]
TICK = 0.01
def session(name):
    return json.load(open(f"{out}/{name}/report.json"))["scenarios"][0]
def steps(name):
    (path,) = glob.glob(f"{out}/{name}/details/bot/*.json.gz")
    return json.load(gzip.open(path))["timeline"]["steps"]

trade, control = steps("trade"), steps("control")
assert session("trade")["valuation"] == "book" and session("control")["valuation"] == "mark"
assert all("impact_bps" in s for s in trade), "a step of the impact session carries no impact"
assert not any("impact_bps" in s for s in control), "a session without impact reports one"

bought = next(i for i, s in enumerate(trade) if s["position"] > 0)
sold = next(i for i, s in enumerate(trade) if i > bought and s["position"] == 0)
assert trade[bought]["position"] == 350 and control[bought]["position"] == 350, "the bot didn't buy 350 SPCX at the same step in both"
assert all(s["impact_bps"] == 0 for s in trade[:bought]), "an impact before anything traded"

# The buy walked the asks up, and the makers are centred where it ended: the
# ask is where the buy left it, as it is without impact, and the bids came up
# with it, which without impact they don't.
at_once = trade[bought]["impact_bps"]
print("the buy: impact", at_once, "bps; bid and ask", trade[bought]["best_bid_usd"], trade[bought]["best_ask_usd"],
      "against", control[bought]["best_bid_usd"], control[bought]["best_ask_usd"], "without impact")
assert at_once > 5, "350 SPCX walked the book by less than 5 bps"
assert abs(trade[bought]["best_ask_usd"] - control[bought]["best_ask_usd"]) <= TICK, "the ask is not where the buy left it"
lifted = trade[bought]["best_bid_usd"] - control[bought]["best_bid_usd"]
expected = trade[bought]["book_usd"] * at_once / 10_000
assert abs(lifted - expected) <= 3 * TICK, f"the bid is {lifted} above the control's, the impact says {expected}"
# Without impact the asks the buy took stay taken until the path's next move,
# which puts them all back.
assert control[sold - 2]["best_ask_usd"] == control[bought]["best_ask_usd"]
# Only the book moved: the index is the scenario's level.
for s in trade[bought:sold]:
    assert abs(s["index_usd"] - s["level_usd"]) <= TICK, "the impact moved the index"
    assert s["best_bid_usd"] > s["level_usd"], "the book fell back to the level while the impact stood"

# Half of it fades, and the rest stays while the bot holds.
held = trade[sold - 1]["impact_bps"]
print("held: impact", held, "bps, of", at_once)
assert abs(held - at_once / 2) <= 0.15 * at_once, "the impact did not fade to its permanent half"
impacts = [s["impact_bps"] for s in trade[bought:sold]]
assert all(a >= b - 1e-9 for a, b in zip(impacts, impacts[1:])), "the impact rose with nothing trading"

# The sale walks it back, past the level, and that fades too.
after = trade[sold]["impact_bps"]
print("the sale: impact", after, "bps; the end:", trade[-1]["impact_bps"])
assert after < held - 5, "selling 350 SPCX did not walk the book back"
assert abs(trade[-1]["impact_bps"]) < 1, "the impact of a closed round trip did not fade out"
assert max(abs(s["impact_bps"]) for s in trade) <= 150, "the impact passed its limit"

# Valued at the scenario's level, the bot gains nothing by pushing the book.
print("pnl with impact", session("trade")["pnl_usd"], "without", session("control")["pnl_usd"])
assert "replay" in json.load(open(f"{out}/trade/report.json")), "no replay section"

# A bot that never trades: no impact, and the same book once the path moves.
idle, plain = steps("idle"), steps("idle-control")
assert all(s["impact_bps"] == 0 for s in idle), "an impact with nothing trading"
first_move = next(i for i, s in enumerate(plain) if s["level_usd"] != plain[0]["level_usd"])
compared = 0
for a, b in zip(idle[first_move:], plain[first_move:]):
    # The schedule uncrosses a move two slots after it, the script in the
    # move's own slot: for that one step the scheduled book is still crossed.
    if b["best_bid_usd"] >= b["best_ask_usd"]:
        continue
    compared += 1
    assert (a["best_bid_usd"], a["best_ask_usd"], a["index_usd"]) == (b["best_bid_usd"], b["best_ask_usd"], b["index_usd"]), \
        f"step {a['k']}: the idle book differs with impact: {a} against {b}"
assert compared >= len(plain) - first_move - 2, "the scheduled book stayed crossed"
assert session("idle")["pnl_usd"] == 0 == session("idle-control")["pnl_usd"]
# Other traders around the bot: its trades still leave their impact.
for run in (1, 2, 3, 4):
    noisy = steps(f"noisy-{run}")
    held = next(i for i, s in enumerate(noisy) if s["position"] > 0)
    flat = next(i for i, s in enumerate(noisy) if i > held and s["position"] == 0)
    print(f"noisy {run}: impact after the buy", noisy[held]["impact_bps"], "after the sale", noisy[flat]["impact_bps"])
    assert noisy[held]["impact_bps"] > 5, "the buy left no impact among other traders"
    assert noisy[flat]["impact_bps"] < -5, "the sale left no impact among other traders"
    assert noisy[flat]["best_bid_usd"] < noisy[flat]["level_usd"], "the book is not below the level after the sale"
print("IMPACT_SMOKE_OK")
PY

#!/usr/bin/env bash
# Check the liquidation keeper (venues.perp.liquidator) end to end, on NVDA:
#   - bots/first-strategy buys the dip at about 14x its collateral and holds
#     through a fall that crosses maintenance margin. The keeper liquidates
#     it at Phoenix's own rules: a forced close with reason
#     liquidation/keeper, the account going on, not disqualified, and the
#     same grade replayed twice agrees (--replay 2).
#   - the same bot on control.json (no keeper) ends liquidatable and is
#     disqualified; its end value is the keeper's plus what the liquidation
#     cost.
#   - bots/starter never trades: the keeper sends nothing.
#   - bots/keeper-race cuts its size before maintenance margin: never
#     liquidated.
#   bench/checks/keeper/run.sh <mtx> <snapshot> <out>
# The bots must be built (just --justfile bots/<bot>/Justfile build). The
# snapshot is snapshots/phoenix-perps-2026-11-13 (it holds NVDA).
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
    --report "$out/$name/report.json" "$@" "$here/$scenario.json" \
    >"$out/$name/stdout.log" 2>"$out/$name/stderr.log" || { echo "$name: the grade failed"; tail -20 "$out/$name/stderr.log"; exit 1; }
}
grade hold first-strategy smoke --replay 2
grade control first-strategy control
grade idle starter smoke
grade race keeper-race smoke
python3 -I - "$out" <<'PY'
import json, sys
out = sys.argv[1]
def session(name):
    return json.load(open(f"{out}/{name}/report.json"))["scenarios"][0]

hold = session("hold")
closes = hold.get("forced_closes", [])
keeper = hold["liquidator"]
print("hold: pnl", hold["pnl_usd"], "end_position", hold["end_position"], "peak_position", hold["peak_position"],
      "disqualified", hold.get("disqualified"), "liquidator", json.dumps(keeper), "closes", json.dumps(closes))
assert closes, "the keeper never liquidated the bot"
assert all(c["reason"] == {"kind": "liquidation", "by": "keeper"} for c in closes), "a close that wasn't the keeper's"
assert all(o["ok"] for c in closes for o in c["orders"][:1]), "Phoenix refused the keeper's first liquidation"
for c in closes:
    assert c["value_after_usd"] < c["value_before_usd"], "a liquidation that cost nothing"
    assert c["position_before"] > 100, "the bot held no position to liquidate"
assert keeper["liquidations"] >= 1 and 2 <= keeper["delay_slots"] <= 12
cost = sum(c["value_before_usd"] - c["value_after_usd"] for c in closes)
assert abs(keeper["cost_usd"] - cost) < 1e-6 and cost > 0
assert hold["end_position"] < hold["peak_position"], "the liquidation closed nothing of the position"
assert hold.get("disqualified") is None, "a liquidated bot was disqualified"
assert not hold.get("crank_refusals"), hold.get("crank_refusals")
assert hold["turns"].get("stopped") is None and hold["turns"]["overran"] == 0, "the session did not go on after the liquidation"
assert "replay" in json.load(open(f"{out}/hold/report.json")), "no replay section"

control = session("control")
print("control: pnl", control["pnl_usd"], "end_position", control["end_position"], "disqualified", control.get("disqualified"))
assert control["liquidatable"] and "liquidatable" in (control.get("disqualified") or ""), "the bot without a keeper was not disqualified"
assert not control.get("forced_closes") and control.get("liquidator") is None
gap = control["end_value_usd"] - hold["end_value_usd"]
print("end value without the keeper less with it:", gap, "against the liquidation's cost", cost)
assert abs(gap - cost) <= 0.15 * cost + 25, "the keeper cost more or less than its liquidations did"

idle = session("idle")
print("idle:", json.dumps(idle["liquidator"]), idle.get("forced_closes"))
assert idle["liquidator"]["liquidations"] == 0 and not idle.get("forced_closes") and idle.get("disqualified") is None

race = session("race")
print("race: peak", race["peak_position"], "end", race["end_position"], json.dumps(race["liquidator"]), race.get("disqualified"))
assert race["peak_position"] > 100, "the racing bot never opened"
assert race["liquidator"]["liquidations"] == 0 and not race.get("forced_closes"), "the racing bot was liquidated"
assert race.get("disqualified") is None and not race["liquidatable"]
print("KEEPER_SMOKE_OK")
PY

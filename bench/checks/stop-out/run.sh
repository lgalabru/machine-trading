#!/usr/bin/env bash
# Check the risk desk's stop-out (scoring.stop_out): grade bots/first-strategy,
# which buys half its cap on a 1.5% dip, on bench/checks/stop-out/smoke.json,
# where the dip deepens past its drawdown limit, and check that the desk
# stopped the bot, closed the position flat through the book, priced the
# breach instead of disqualifying, and sent nothing for the bot after it.
# Then grade bots/starter, which never trades, and check nothing closed.
# Then grade bots/first-strategy again on smoke-tx.json, the same with the
# transaction layer on, and check nothing of the bot's lands after the
# trigger and the layer's ledger books the close's fees and explains the
# wallet.
#   bench/checks/stop-out/run.sh <mtx> <snapshot> <out>
# The bots must be built (just --justfile bots/<bot>/Justfile build).
set -euo pipefail
mtx=$1 snapshot=$2 out=$3
here="$(cd "$(dirname "$0")" && pwd)"
root="$here/../../.."
rm -rf "$out"
mkdir -p "$out"
for run in first-strategy:smoke starter:smoke first-strategy-tx:smoke-tx; do
  name=${run%%:*} bot=${name%-tx}
  mkdir -p "$out/$name"
  "$mtx" grade --components-only --snapshot "$snapshot" --bot "$root/bots/$bot" --logs "$out/$name/logs" \
    --details "$out/$name/sessions" --report "$out/$name/report.json" "$here/${run#*:}.json" \
    >"$out/$name/stdout.log" 2>"$out/$name/stderr.log" || true
done
python3 -I - "$out" <<'PY'
import glob, gzip, json, sys
out = sys.argv[1]
def detail(bot):
    path = glob.glob(f"{out}/{bot}/sessions/bot/*")[0]
    opener = gzip.open if path.endswith(".gz") else open
    with opener(path, "rt") as f:
        return json.load(f)
def stopped(name):
    session = json.load(open(f"{out}/{name}/report.json"))["scenarios"][0]
    closes = session.get("forced_closes", [])
    print(name, "pnl", session["pnl_usd"], "max_drawdown", session["max_drawdown_usd"],
          "disqualified", session.get("disqualified"), "closes", json.dumps(closes))
    assert len(closes) == 1, "the breach was not closed once"
    close = closes[0]
    assert close["reason"] == {"kind": "stop_out", "rule": "drawdown"}
    assert close["flat"] and abs(session["end_position"]) < 1e-3, "the close did not flatten the account"
    assert close["position_before"] > 50, "the bot held no position to close"
    assert session.get("disqualified") is None, "a priced breach disqualified the session"
    assert session["pnl_usd"] < -300, "the close's loss was not counted"
    assert abs(session["end_value_usd"] - close["value_after_usd"]) < 1e-6
    sent = [t for t in detail(name)["timeline"]["transactions"] if t.get("step") is not None]
    late = [t for t in sent if t["step"] >= close["step"]]
    assert not late, f"the bot sent after the desk took over: {late[:2]}"
    return session, close
stopped("first-strategy")
# With the transaction layer on: nothing of the bot's lands after the
# trigger, and the ledger explains the wallet, the close's fees included.
session, close = stopped("first-strategy-tx")
layer = session["tx_layer"]
landed_after = [b for b in layer["blocks"] if b["landed"] and b["at"] > close["at"]]
assert not landed_after, f"the layer landed the bot's sends during the close: {landed_after[:2]}"
fees = layer["fees"]
print("first-strategy-tx fees", json.dumps(fees))
assert fees.get("forced_close", 0) > 0, "the close's fees were not booked"
assert fees["unexplained"] == 0, "the ledger doesn't explain the wallet across the stop-out"
quiet = json.load(open(f"{out}/starter/report.json"))["scenarios"][0]
assert not quiet.get("forced_closes") and quiet.get("disqualified") is None, "a bot that never traded was closed"
print("STOP_OUT_SMOKE_OK")
PY

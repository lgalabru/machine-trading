#!/usr/bin/env bash
# Smoke for execution faults: grade bots/fault-probe on bench/checks/faults/smoke.json and check
# what the session's detail file says the faults did, and what the bot's log says it saw.
#   bench/checks/faults/run.sh <mtx> <snapshot> <out> [bot dir]
# The bot must be built (bots/fault-probe/build.sh).
set -euo pipefail
mtx=$1 snapshot=$2 out=$3
here="$(cd "$(dirname "$0")" && pwd)"
root="$here/../../.."
bot=${4:-$root/bots/fault-probe}
rm -rf "$out"
mkdir -p "$out"
"$mtx" grade --snapshot "$snapshot" --bot "$bot" --logs "$out/logs" \
  --report "$out/report.json" --details "$out/details" "$here/smoke.json" >"$out/stdout.log" 2>"$out/stderr.log"
python3 -I - "$out" <<'PY'
import gzip, json, sys
out = sys.argv[1]
detail = json.load(gzip.open(f"{out}/details/bot/faults-smoke.json.gz"))
report = detail["report"]
faults = report["faults"]
fired = {f["fault"]: f for f in faults["fired"]}
print("sends", faults["sends"], "landings", faults["landings"], "reads", faults["reads"], "held_at_end", faults["held_at_end"])
for f in faults["fired"]:
    print("fired", f)
for fault in ("drop", "blockhash_expiry", "latency", "lost_confirmation", "rpc_unavailable"):
    assert fault in fired, f"{fault} did not fire"
assert fired["drop"]["n"] == 1 and fired["blockhash_expiry"]["n"] == 2
assert fired["latency"]["until_step"] is not None, "the held send never landed"
assert faults["held_at_end"] == 0
assert report["refused_calls"] == [], report["refused_calls"]
txs = detail["timeline"]["transactions"]
for tx in txs:
    print("tx", tx.get("step"), tx.get("ok"), tx.get("error"))
assert txs[0]["error"] and "never executed" in txs[0]["error"], "the dropped send executed"
assert any(tx["ok"] for tx in txs), "nothing landed"
log = open(f"{out}/logs/faults-smoke.log").read()
for line in log.splitlines():
    if "probe:" in line:
        print(line[line.index("probe:"):])
for need in ("dropped send seen", "expired blockhash seen", "resent", "status unknown, waiting", "recovered"):
    assert need in log, f"the bot never logged: {need}"
print("FAULTS_SMOKE_OK")
PY

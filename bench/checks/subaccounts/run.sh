#!/usr/bin/env bash
# Check that a position in a Phoenix subaccount counts: grade
# bots/subaccount-probe, which buys twice its cap in an isolated subaccount
# it opens, on bench/checks/subaccounts/smoke.json, and check that the
# trade landed and the session is disqualified for the cap.
#   bench/checks/subaccounts/run.sh <mtx> <snapshot> <out>
# The bot must be built (just --justfile bots/subaccount-probe/Justfile build).
set -euo pipefail
mtx=$1 snapshot=$2 out=$3
here="$(cd "$(dirname "$0")" && pwd)"
root="$here/../../.."
rm -rf "$out"
mkdir -p "$out"
"$mtx" grade --snapshot "$snapshot" --bot "$root/bots/subaccount-probe" --logs "$out/logs" \
  --report "$out/report.json" "$here/smoke.json" >"$out/stdout.log" 2>"$out/stderr.log" || true
python3 -I - "$out" <<'PY'
import json, sys
out = sys.argv[1]
log = open(f"{out}/logs/subaccounts-smoke.log").read()
for line in log.splitlines():
    if "probe:" in line:
        print(line[line.index("probe:"):])
assert "probe: landed ok" in log, "the subaccount trade did not land"
session = json.load(open(f"{out}/report.json"))["scenarios"][0]
print("peak_position", session["peak_position"], "end_position", session["end_position"],
      "disqualified", session.get("disqualified"))
assert session["peak_position"] >= 10.0, "the meter did not see the subaccount's position"
assert "over the position cap" in (session.get("disqualified") or ""), "a position past the cap went unpunished"
print("SUBACCOUNTS_SMOKE_OK")
PY

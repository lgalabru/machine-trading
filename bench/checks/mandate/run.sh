#!/usr/bin/env bash
# Smoke for mandate v0.1's notional rules in a perp session: the blind-long
# rung (the full 70-share cap long at the open, held) under a gross notional
# cap and a net exposure deadline, each once breached and once not, and the
# starter, which never trades, under all four.
#   bench/checks/mandate/run.sh <mtx> <snapshot> <out>
# The rung must be built (`just ladder` builds bench/checks/ladder/bin), and
# the starter too (`just --justfile bots/starter/Justfile build`).
set -euo pipefail
mtx=$1 snapshot=$2 out=$3
here="$(cd "$(dirname "$0")" && pwd)"
root="$here/../../.."
rm -rf "$out"
mkdir -p "$out"
grade() {
  local name=$1 bot=$2
  shift 2
  "$mtx" grade --snapshot "$snapshot" --bot "$bot" "$@" --logs "$out/$name/logs" \
    --report "$out/$name/report.json" --details "$out/$name/details" \
    "$here"/gross-over.json "$here"/gross-within.json \
    "$here"/deadline-passed.json "$here"/deadline-later.json \
    >"$out/$name.stdout.log" 2>"$out/$name.stderr.log"
}
grade blind-long "$root/bench/checks/ladder/blind-long" --trusted
grade starter "$root/bots/starter"
python3 -I - "$out" <<'PY'
import json, sys
out = sys.argv[1]
want = {
    "blind-long": {
        "mandate-gross-over": "gross_notional",
        "mandate-gross-within": None,
        "mandate-deadline-passed": "exposure",
        "mandate-deadline-later": None,
    },
    "starter": {name: None for name in (
        "mandate-gross-over", "mandate-gross-within",
        "mandate-deadline-passed", "mandate-deadline-later")},
}
for bot, cases in want.items():
    report = json.load(open(f"{out}/{bot}/report.json"))
    seen, terms = {}, {}
    for s in report["scenarios"]:
        session = s.get("session", s)
        seen[session["scenario"]] = (s.get("disqualified"), session.get("notional_breach"), session["peak_position"])
        terms[session["scenario"]] = session["mandate"]["term"]
    for name, rule in cases.items():
        why, breach, peak = seen[name]
        print(f"{bot} {name}: peak {peak}, breach {breach}, disqualified {why!r}")
        if rule is None:
            assert breach is None and why is None, f"{bot} {name} should pass"
        else:
            assert breach and breach["rule"] == rule, f"{bot} {name} should break {rule}"
            assert why, f"{bot} {name} should be disqualified"
    if bot == "blind-long":
        assert seen["mandate-gross-within"][2] > 60, "blind-long never took its position"
        _, breach, _ = seen["mandate-deadline-passed"]
        term = terms["mandate-deadline-passed"]
        print("deadline breached at slot", breach["slot"], "of term", term)
        assert breach["slot"] >= term["from_slot"] + 40, "the deadline was enforced before its start"
print("MANDATE_SMOKE_OK")
PY

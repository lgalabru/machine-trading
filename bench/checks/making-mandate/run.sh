#!/usr/bin/env bash
# Smoke for a market-making session's mandate: quoting obligations and
# reduce-only taking, on market-making-v2 scenarios given both (the
# obligation's band 1.5 times the competitor's half-spread, 75% presence, 1
# SPCX a side; the mandate's `taking: reduce_only`):
#   - bots/making-probe (two-sided, post-only) meets its obligations and
#     never takes;
#   - its one-sided build misses them, and is not disqualified for it;
#   - its taker build sends market orders that would open or add: each is
#     refused before it goes out, counted, and no breach;
#   - its crosser build crosses a touch the grader can't tell from its own:
#     the order goes, and the audit disqualifies it for taking;
#   - the two-sided probe on the same scenarios without the new fields
#     reports neither.
#   bench/checks/making-mandate/run.sh <mtx> <snapshot> <out>
# The probes must be built: `just build`, `just build one-sided`,
# `just build taker` and `just build crosser` in bots/making-probe.
set -euo pipefail
mtx=$1 snapshot=$2 out=$3
here="$(cd "$(dirname "$0")" && pwd)"
probe="$here/../../../bots/making-probe"
rm -rf "$out"
mkdir -p "$out/plain" "$out/obliged"
"$mtx" scenario generate --family market-making-v2 --prefix mm --count 3 --seed 43 --out "$out/plain" >/dev/null
python3 -I - "$out" <<'PY'
import glob, json, os, sys
out = sys.argv[1]
for path in sorted(glob.glob(f"{out}/plain/*.json")):
    s = json.load(open(path))
    making = s["actors"]["competitor"] if s.get("schema") == 2 else s["making"]
    making["obligations"] = {
        "presence": 0.75,
        "max_half_spread_bps": round(1.5 * making["competitor_bps"], 2),
        "min_size": 1.0,
    }
    s["mandate"]["version"] = "v0.1"
    s["mandate"]["universe"][0]["taking"] = "reduce_only"
    json.dump(s, open(f"{out}/obliged/{os.path.basename(path)}", "w"), indent=2)
PY
grade() { # name wasm-dir scenarios
  local name=$1 bot=$2 set=$3
  mkdir -p "$out/$name"
  "$mtx" grade --components-only --jobs 3 --snapshot "$snapshot" --bot "$bot" --logs "$out/$name/logs" \
    --report "$out/$name/report.json" "$out/$set"/*.json \
    >"$out/$name/stdout.log" 2>"$out/$name/stderr.log" || { echo "$name: the grade failed"; tail -20 "$out/$name/stderr.log"; exit 1; }
}
grade maker "$probe" obliged
grade one-sided "$probe/one-sided" obliged
grade taker "$probe/taker" obliged
grade crosser "$probe/crosser" obliged
grade plain "$probe" plain
python3 -I - "$out" <<'PY'
import json, sys
out = sys.argv[1]
def sessions(name):
    return [s.get("session", s) | {"disqualified": s.get("disqualified")}
            for s in json.load(open(f"{out}/{name}/report.json"))["scenarios"]]
def takes(s):
    return [f for f in s.get("making_fills", []) if f["role"] == "taker"]
for name in ("maker", "one-sided", "taker", "crosser", "plain"):
    for s in sessions(name):
        o = s.get("obligations") or {}
        print(f"{name} {s['scenario']}: uptime {o.get('uptime')} met {o.get('met')} "
              f"(two_sided {o.get('two_sided')} one_sided {o.get('one_sided')} too_wide {o.get('too_wide')} "
              f"too_small {o.get('too_small')} unsampled {o.get('unsampled')} of {o.get('steps')}); "
              f"refused_takes {s['turns'].get('refused_takes')}; taker fills {len(takes(s))}; "
              f"fills {len(s.get('making_fills', []))}; end {s['end_position']}; disqualified {s['disqualified']!r}")
taking = "allows taking only to reduce"
for s in sessions("maker"):
    o = s["obligations"]
    assert o["met"] and o["uptime"] >= 0.75, f"maker {s['scenario']} missed its obligations"
    assert s["turns"]["refused_takes"] == 0 and not takes(s), "the maker took"
    assert taking not in (s["disqualified"] or "")
for s in sessions("one-sided"):
    o = s["obligations"]
    assert not o["met"] and o["one_sided"] >= o["steps"] - o["unsampled"] - 1, "one-sided met its obligations"
    assert not (s["disqualified"] or ""), "missing obligations disqualified"
for s in sessions("taker"):
    assert s["turns"]["refused_takes"] > 0, "nothing refused"
    assert not takes(s), "a refused take went out"
    assert taking not in (s["disqualified"] or ""), "a refusal was taken for a breach"
def opened(s):
    """Whether a taker fill left the position further from zero, or past it."""
    position = s.get("start_position", 0.0)
    for f in s.get("making_fills", []):
        after = position + (f["base"] if f["side"] == "buy" else -f["base"])
        if f["role"] == "taker" and (abs(after) > abs(position) + 1e-9 or after * position < -1e-12):
            return True
        position = after
    return False
crossers = sessions("crosser")
assert any(opened(s) for s in crossers), "the crosser never took to open"
for s in crossers:
    assert (taking in (s["disqualified"] or "")) == opened(s), f"crosser {s['scenario']}: audit and fills disagree"
for s in sessions("plain"):
    assert "obligations" not in s and "refused_takes" not in s["turns"], "opt-in fields without opting in"
print("MAKING_MANDATE_SMOKE_OK")
PY

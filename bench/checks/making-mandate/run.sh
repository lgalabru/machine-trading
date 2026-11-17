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
#   - its crosser build cancels and then sends a plain bid through the
#     other traders' ask while its own ask rests (a touch that looks like its
#     own to a check that trusts its resting orders): refused all the same
#     (a take that reduces the position is allowed);
#   - the same taker and crosser as process bots (`--trusted`), which have no
#     sheet: the RPC guard simulates each transaction and refuses one whose
#     fills would take to open or add, and no breach;
#   - bots/subaccount-probe's one-unit build, a process bot that registers an
#     isolated Phoenix subaccount, funds it and buys there: the fills of
#     every trader account the bot holds count, so each session ends in a
#     `taking` breach (untrusted and `--trusted`);
#   - the two-sided probe on the same scenarios without the new fields
#     reports neither;
#   - with `close_out` on the same scenarios, the grader closes the bot's
#     position after its last turn (a forced reduce-only IOC whose fills are
#     the grader's, never a taking breach): the session ends flat, the
#     decomposition's parts add up to the PnL, the PnL is the same session's
#     without the close-out less what the close cost, and a process taker
#     and the subaccount bot are closed out the same way.
#   bench/checks/making-mandate/run.sh <mtx> <snapshot> <out>
# The probes must be built: `just build`, `just build one-sided`,
# `just build taker` and `just build crosser` in bots/making-probe, and
# `just process taker` and `just process crosser` for the process bots, and
# `just one-unit` in bots/subaccount-probe, and `just build`, `just build honest`,
# `just build drain`, `just build pda` and `just build two-subs` in bots/closeout-probe.
set -euo pipefail
mtx=$1 snapshot=$2 out=$3
here="$(cd "$(dirname "$0")" && pwd)"
probe="$here/../../../bots/making-probe"
subprobe="$here/../../../bots/subaccount-probe/one-unit"
closeprobe="$here/../../../bots/closeout-probe"
rm -rf "$out"
mkdir -p "$out/plain" "$out/obliged" "$out/closed"
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
    making["close_out"] = True
    json.dump(s, open(f"{out}/closed/{os.path.basename(path)}", "w"), indent=2)
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
grade_process() { # name bot-dir
  local name=$1 bot=$2
  mkdir -p "$out/$name"
  "$mtx" grade --trusted --jobs 3 --snapshot "$snapshot" --bot "$bot" --logs "$out/$name/logs" \
    --report "$out/$name/report.json" "$out/obliged"/*.json \
    >"$out/$name/stdout.log" 2>"$out/$name/stderr.log" || { echo "$name: the grade failed"; tail -20 "$out/$name/stderr.log"; exit 1; }
}
grade_process process-taker "$probe/taker-process"
grade_process process-crosser "$probe/crosser-process"
mkdir -p "$out/subaccount-trusted" "$out/subaccount-untrusted"
"$mtx" grade --trusted --jobs 3 --snapshot "$snapshot" --bot "$subprobe" --logs "$out/subaccount-trusted/logs" \
  --report "$out/subaccount-trusted/report.json" "$out"/obliged/*.json \
  >"$out/subaccount-trusted/stdout.log" 2>"$out/subaccount-trusted/stderr.log" || { echo "subaccount-trusted: the grade failed"; tail -20 "$out/subaccount-trusted/stderr.log"; exit 1; }
"$mtx" grade --jobs 3 --snapshot "$snapshot" --bot "$subprobe" --logs "$out/subaccount-untrusted/logs" \
  --report "$out/subaccount-untrusted/report.json" "$out"/obliged/*.json \
  >"$out/subaccount-untrusted/stdout.log" 2>"$out/subaccount-untrusted/stderr.log" || { echo "subaccount-untrusted: the grade failed"; tail -20 "$out/subaccount-untrusted/stderr.log"; exit 1; }
grade plain "$probe" plain
grade closed-maker "$probe" closed
grade closed-taker "$probe/taker" closed
mkdir -p "$out/closed-process-taker"
"$mtx" grade --trusted --jobs 3 --snapshot "$snapshot" --bot "$probe/taker-process" --logs "$out/closed-process-taker/logs" \
  --report "$out/closed-process-taker/report.json" "$out"/closed/*.json \
  >"$out/closed-process-taker/stdout.log" 2>"$out/closed-process-taker/stderr.log" || { echo "closed-process-taker: the grade failed"; tail -20 "$out/closed-process-taker/stderr.log"; exit 1; }
mkdir -p "$out/closed-subaccount"
"$mtx" grade --trusted --jobs 3 --snapshot "$snapshot" --bot "$subprobe" --logs "$out/closed-subaccount/logs" \
  --report "$out/closed-subaccount/report.json" "$out"/closed/*.json \
  >"$out/closed-subaccount/stdout.log" 2>"$out/closed-subaccount/stderr.log" || { echo "closed-subaccount: the grade failed"; tail -20 "$out/closed-subaccount/stderr.log"; exit 1; }
for name in honest dodge drain pda two-subs; do
  mkdir -p "$out/closeout-$name"
  "$mtx" grade --trusted --jobs 3 --snapshot "$snapshot" --bot "$closeprobe/$name" --logs "$out/closeout-$name/logs" \
    --report "$out/closeout-$name/report.json" "$out"/closed/*.json \
    >"$out/closeout-$name/stdout.log" 2>"$out/closeout-$name/stderr.log" || { echo "closeout-$name: the grade failed"; tail -20 "$out/closeout-$name/stderr.log"; exit 1; }
done
python3 -I - "$out" <<'PY'
import json, sys
out = sys.argv[1]
def sessions(name):
    return [s.get("session", s) | {"disqualified": s.get("disqualified")}
            for s in json.load(open(f"{out}/{name}/report.json"))["scenarios"]]
def takes(s):
    return [f for f in s.get("making_fills", []) if f["role"] == "taker"]
for name in ("maker", "one-sided", "taker", "crosser", "process-taker", "process-crosser", "subaccount-trusted", "subaccount-untrusted", "plain"):
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
for name in ("crosser", "process-taker", "process-crosser"):
    ss = sessions(name)
    assert any(s["turns"].get("refused_takes") for s in ss), f"{name}: nothing refused"
    for s in ss:
        # A take that reduces is allowed; none may open, add or flip.
        assert not opened(s), f"{name} {s['scenario']}: a take that opens went out"
        assert taking not in (s["disqualified"] or ""), f"{name} {s['scenario']}: a refusal was taken for a breach"
for name in ("subaccount-trusted", "subaccount-untrusted"):
    for s in sessions(name):
        # The trade landed in the subaccount, and its fill is the bot's: a taker fill that opens.
        assert opened(s), f"{name} {s['scenario']}: the subaccount's take was not seen"
        assert taking in (s["disqualified"] or ""), f"{name} {s['scenario']}: a take from a subaccount went unpunished"
for s in sessions("plain"):
    assert "obligations" not in s and "refused_takes" not in s["turns"], "opt-in fields without opting in"
# The close-out and the decomposition.
FLAT = 5e-4
def by_name(name):
    return {s["scenario"]: s for s in sessions(name)}
def check_closed(name, against):
    # A closed-out session ends flat with its close-out fills forced, and its
    # decomposition adds up to the PnL it reports.
    base = by_name(against)
    saw_close = False
    for s in sessions(name):
        tag = f"{name} {s['scenario']}"
        d = s["decomposition"]
        closes = [c for c in s.get("forced_closes", []) if c["reason"]["kind"] == "close_out"]
        assert len(closes) == 1, f"{tag}: expected one close-out, got {len(closes)}"
        close = closes[0]
        assert abs(s["end_position"]) < FLAT, f"{tag}: not flat after the close-out ({s['end_position']})"
        assert close["flat"], f"{tag}: the close-out says it left the account open"
        forced = [f for f in s["making_fills"] if f.get("forced")]
        assert all(f["role"] == "taker" for f in forced), f"{tag}: a forced fill that rested"
        assert d["close_out"]["fills"] == len(forced), f"{tag}: close-out fills {d['close_out']['fills']} vs forced {len(forced)}"
        assert abs(d["close_out"]["residual_base"]) < FLAT
        saw_close |= bool(forced)
        assert taking not in (s["disqualified"] or ""), f"{tag}: the close-out was taken for a breach"
        total = d["spread_capture_usd"] + d["taking_cost_usd"] - d["maker_fees_usd"] + d["inventory_usd"]
        charge = d["close_out"].get("residual_charge_usd", 0.0)
        assert abs(total - (s["pnl_usd"] + charge)) < 1e-6, f"{tag}: parts {total} vs pnl {s['pnl_usd']} + charge {charge}"
        assert abs(d["pnl_usd"] - s["pnl_usd"]) < 1e-9
        # What the fills' logged prices and fees don't explain stays within a
        # few hundredths of a percent of what the close-out traded.
        exit_usd = sum(f["base"] * f["price"] for f in forced)
        slack = 0.01 + 5e-4 * exit_usd
        assert abs(d["unexplained_usd"]) < slack, f"{tag}: unexplained {d['unexplained_usd']} (slack {slack})"
        assert [m.get("horizon_slots") for m in d["markouts"]] == [2, 20, 120, None], f"{tag}: markout horizons"
        for m in d["markouts"]:
            assert abs(m["realized_spread_usd"] - (d["spread_capture_usd"] + m["markout_usd"])) < 1e-9
        assert d["uptime"] is not None and abs(d["uptime"] - s["obligations"]["uptime"]) < 1e-12
        other = base[s["scenario"]]
        cost = d["close_out"]["cost_usd"]
        residual = s["pnl_usd"] - (other["pnl_usd"] - cost)
        print(f"{tag}: pnl {s['pnl_usd']:.4f} (without the close-out {other['pnl_usd']:.4f}); close-out {d['close_out']['fills']} fills "
              f"{d['close_out']['base']:.3f} base, cost {cost:.4f} (slippage {d['close_out']['slippage_usd']:.4f} fees {d['close_out']['fees_usd']:.4f}); "
              f"S {d['spread_capture_usd']:.4f} M {[round(m['markout_usd'], 4) for m in d['markouts']]} T {d['taking_cost_usd']:.4f} "
              f"V {d['inventory_usd']:.4f} (jump {d['inventory_jump_usd']:.4f} walk {d['inventory_walk_usd']:.4f}) unexplained {d['unexplained_usd']:.5f} "
              f"uptime {d['uptime']:.3f}; pnl less (without - cost) {residual:.5f}")
        assert abs(residual) < slack, f"{tag}: the close-out changed more than its cost ({residual})"
        assert "decomposition" not in other and not [c for c in other.get("forced_closes", []) if c["reason"]["kind"] == "close_out"], f"{tag}: opt-in leaked"
    return saw_close
assert check_closed("closed-maker", "maker"), "no maker session had anything to close out"
check_closed("closed-taker", "taker")
check_closed("closed-process-taker", "process-taker")
for s in sessions("closed-subaccount"):
    # The close-out can't hide a subaccount's take: the audit still finds it.
    assert taking in (s["disqualified"] or ""), f"closed-subaccount {s['scenario']}: a take from a subaccount went unpunished"
    assert "decomposition" in s
# A bot that parks its inventory where the close-out can't reach it pays the
# exit as the honest one does: each account is closed from itself, and what is
# left is charged, never valued at the oracle for free.
honest = by_name("closeout-honest")
for name in ("closeout-dodge", "closeout-drain", "closeout-pda", "closeout-two-subs"):
    for s in sessions(name):
        tag = f"{name} {s['scenario']}"
        d = s["decomposition"]["close_out"]
        h = honest[s["scenario"]]
        assert abs(s["end_position"]) < FLAT or d["residual_charge_usd"] > 0, f"{tag}: left open and not charged"
        assert abs(s["end_position"]) < FLAT, f"{tag}: not flat after the close-out ({s['end_position']})"
        assert d["cost_usd"] >= h["decomposition"]["close_out"]["cost_usd"] - 0.05, f"{tag}: paid less to exit than the honest bot"
        assert s["pnl_usd"] <= h["pnl_usd"] + 0.05, f"{tag}: ended {s['pnl_usd']} against the honest bot's {h['pnl_usd']}"
        print(f"{tag}: pnl {s['pnl_usd']:.4f} (honest {h['pnl_usd']:.4f}); close-out cost {d['cost_usd']:.4f} "
              f"(honest {h['decomposition']['close_out']['cost_usd']:.4f}); residual {d['residual_base']:.4f} charged {d.get('residual_charge_usd', 0.0):.4f}")
for name in ("maker", "plain"):
    for s in sessions(name):
        assert "decomposition" not in s and not s.get("forced_closes"), f"{name}: close-out fields without opting in"
print("MAKING_MANDATE_SMOKE_OK")
PY

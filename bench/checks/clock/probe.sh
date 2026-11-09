#!/usr/bin/env bash
# The clock check, for a task that opts in to `time-left`: its task.toml
# states the same limit twice ([agent] timeout_sec and MTX_AGENT_TIMEOUT_SEC)
# and passes the run's multiplier through; under `harbor run` at a multiplier
# of 0.5, the agent's `time-left` reads half the limit, starts within a minute
# of Harbor starting the agent, and runs down with the wall clock.
#   bench/checks/clock/probe.sh bench/tasks/<task>
set -euo pipefail
task="${1:?usage: probe.sh bench/tasks/<task>}"
python3 -I - "$task/task.toml" <<'PY'
import sys, tomllib
c = tomllib.load(open(sys.argv[1], "rb"))
env = c.get("environment", {}).get("env", {})
limit = c.get("agent", {}).get("timeout_sec")
if env.get("MTX_AGENT_TIMEOUT_SEC") is None:
    sys.exit("clock probe: the task doesn't opt in (no MTX_AGENT_TIMEOUT_SEC in [environment.env])")
if float(env["MTX_AGENT_TIMEOUT_SEC"]) != float(limit):
    sys.exit(f"clock probe: FAIL, MTX_AGENT_TIMEOUT_SEC {env['MTX_AGENT_TIMEOUT_SEC']} != [agent] timeout_sec {limit}")
if env.get("MTX_AGENT_TIMEOUT_MULTIPLIER") != "${MTX_AGENT_TIMEOUT_MULTIPLIER:-1}":
    sys.exit("clock probe: FAIL, MTX_AGENT_TIMEOUT_MULTIPLIER must be \"${MTX_AGENT_TIMEOUT_MULTIPLIER:-1}\"")
PY
jobs="$(mktemp -d)"
trap 'rm -rf "$jobs"' EXIT
MTX_AGENT_TIMEOUT_MULTIPLIER=0.5 PYTHONPATH="$(dirname "$0")/../.." \
  harbor run -y -p "$task" -o "$jobs" -a checks.clock_agent:ClockProbe \
  --agent-timeout-multiplier 0.5 --disable-verification > /dev/null
record="$(find "$jobs" -name clock.json | head -1)"
[ -n "$record" ] || { echo "clock probe: no clock.json (the agent didn't run)" >&2; exit 2; }
python3 -I - "$record" "$task/task.toml" <<'PY'
import json, re, sys, tomllib
r = json.load(open(sys.argv[1]))
limit = tomllib.load(open(sys.argv[2], "rb"))["agent"]["timeout_sec"]
print(r["stdout"].strip())
sec = lambda m, s: int(m) * 60 + int(s)
reads = [(sec(*m[0:2]), sec(*m[2:4]), sec(*m[4:6]))
         for m in re.findall(r"(\d+)m(\d+)s left of (\d+)m(\d+)s \((\d+)m(\d+)s used", r["stdout"])]
if r["return_code"] != 0 or len(reads) != 2:
    sys.exit(f"clock probe: FAIL, time-left didn't read twice: {r['stderr'].strip()}")
(left0, total, used0), (left1, _, used1) = reads
fails = []
if total != int(limit * 0.5):
    fails.append(f"limit {total} s, not {int(limit * 0.5)} s")
if used0 > 60:
    fails.append(f"{used0} s used at the agent's start")
if abs((used1 - used0) - r["wait_sec"]) > 5:
    fails.append(f"ran down {used1 - used0} s in {r['wait_sec']} s")
if abs(left0 + used0 - total) > 1:
    fails.append("left + used != limit")
if fails:
    sys.exit("clock probe: FAIL, " + "; ".join(fails))
print(f"clock probe: pass, limit {total} s at 0.5x, {used0} s used at start, ran down {used1 - used0} s in {r['wait_sec']} s")
PY

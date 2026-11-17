#!/usr/bin/env bash
# The network check, for a guarded task (one whose environment starts from the
# base's `-agent` image): under `harbor run`, the agent's container, as the
# agent user, can't reach github.com, ghcr.io (where the full `mtx` and the
# reference bots are pulled from) or the rest of the internet, and still
# reaches the crates registry, its model's address (a stand-in server on the
# docker bridge, allowed as the bench allows its key proxy: --allow-agent-host)
# and its chain (a practice scenario graded in the container).
#   bench/checks/network/probe.sh bench/tasks/<task>
set -euo pipefail
task="${1:?usage: probe.sh bench/tasks/<task>}"
python3 -I - "$task/task.toml" "$task/environment/Dockerfile" "$(basename "$task")" <<'PY'
import re, sys, tomllib
c = tomllib.load(open(sys.argv[1], "rb"))
# Harbor runs task.toml's docker_image, not the Dockerfile: it must be the
# task's own image (`<tag>-<task>`), built from the agent-image Dockerfile.
image = c.get("environment", {}).get("docker_image", "")
if not re.search(rf":[^@:/]+-{re.escape(sys.argv[3])}(@sha256:[0-9a-f]+)?$", image):
    sys.exit(f"network probe: FAIL, [environment] docker_image is {image!r}, not this task's own -{sys.argv[3]} image: Harbor would run another image")
agent = c.get("agent", {})
if not re.search(r"^FROM \S+-agent(@\S+)?$", open(sys.argv[2]).read(), re.M):
    sys.exit("network probe: the task isn't guarded (its environment doesn't start from the -agent image)")
if agent.get("network_mode") != "allowlist":
    sys.exit("network probe: FAIL, the task's [agent] has no network_mode = \"allowlist\"")
bad = [h for h in agent.get("allowed_hosts", []) if re.search(r"github|ghcr|^\*", h)]
if bad:
    sys.exit(f"network probe: FAIL, the allowlist names {bad}")
PY
here="$(cd "$(dirname "$0")" && pwd)"
jobs="$(mktemp -d)"
# The trials reach the host by the docker0 bridge's address: a Docker daemon
# without that bridge (Docker Desktop, Colima) can't run this check.
host="$(ip -4 -o addr show docker0 2> /dev/null | awk '{print $4}' | cut -d/ -f1 || true)"
[ -n "$host" ] || { echo "network probe: no docker0 address: run this on a Linux host whose Docker daemon has the docker0 bridge (not Docker Desktop or Colima)" >&2; exit 2; }
# A stand-in for the model's key proxy, on the bridge the trials reach the host by.
python3 -I -u -m http.server 0 --bind "$host" --directory "$jobs" > "$jobs/server.log" 2>&1 &
server=$!
trap 'kill "$server" 2> /dev/null || true; rm -rf "$jobs"' EXIT
for _ in $(seq 50); do grep -q 'port' "$jobs/server.log" && break; sleep 0.2; done
port="$(grep -o 'port [0-9]*' "$jobs/server.log" | head -1 | cut -d' ' -f2)"
[ -n "$port" ] || { echo "network probe: the stand-in server didn't start" >&2; exit 2; }
MTX_PROBE_MODEL="$host:$port" PYTHONPATH="$here/../.." \
  harbor run -y -p "$task" -o "$jobs/out" -a checks.network_agent:NetworkProbe \
  --allow-agent-host "$host" --disable-verification > "$jobs/harbor.log" 2>&1 \
  || { tail -20 "$jobs/harbor.log" >&2; exit 2; }
record="$(find "$jobs/out" -name network.json | head -1)"
[ -n "$record" ] || { echo "network probe: no network.json (the agent didn't run)" >&2; tail -20 "$jobs/harbor.log" >&2; exit 2; }
python3 -I - "$record" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))["reached"]
must = ["mtx-runs", "tls:crates.io", "tls:index.crates.io", "tls:static.crates.io", "cargo-registry", "model"]
if r.get("chain") != "skipped":
    must.append("chain")
must_not = ["tls:github.com", "tls:codeload.github.com", "tls:raw.githubusercontent.com",
            "tls:objects.githubusercontent.com", "tls:pkg-containers.githubusercontent.com",
            "tls:ghcr.io", "tls:example.com", "http:github.com", "ip-no-sni:ghcr.io",
            "ip-sni-index.crates.io:ghcr.io", "cargo-git:github.com",
            "scenario-generate", "anchors-dir"]
fails = [f"{n} unreachable" for n in must if r.get(n) != "yes"]
fails += [f"{n} REACHABLE" for n in must_not if r.get(n) != "no"]
for name in sorted(r):
    print(f"  {name}: {r[name]}")
if fails:
    sys.exit("network probe: FAIL, " + "; ".join(fails))
note = "" if r.get("chain") != "skipped" else " (no practice scenario to play: the chain wasn't checked)"
print(f"network probe: pass, {len(must_not)} blocked, {len(must)} reachable{note}")
PY

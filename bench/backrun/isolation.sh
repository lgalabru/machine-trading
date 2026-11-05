#!/usr/bin/env bash
# Untimed namespace checks against an actual offline Surfpool playground.
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo"
project="backrun-isolation-$$"
compose=${MTH_BACKRUN_COMPOSE:-bench/tasks/atomic-arb/tests/docker-compose.yaml}
trap 'docker compose -p "$project" -f "$compose" down -v >/dev/null 2>&1 || true' EXIT
docker compose -p "$project" -f "$compose" up -d --pull never >&2
docker exec -d "$project-main-1" sh -c 'mth-backrun playground --market /private/fixture/market.json --snapshot /private/fixture/snapshot.json --programs /private/programs --context /tmp/context.json > /tmp/playground.log 2>&1'
port=$(python3 - "$project-main-1" <<'PY'
import json,subprocess,sys,time,urllib.parse
end=time.monotonic()+30
while time.monotonic()<end:
    result=subprocess.run(['docker','exec',sys.argv[1],'cat','/tmp/playground.log'],capture_output=True,text=True)
    try:
        url=json.loads(result.stdout.splitlines()[0])['rpc_url']
        parsed=urllib.parse.urlparse(url); assert parsed.hostname=='127.0.0.1'
        print(parsed.port); break
    except (IndexError,KeyError,json.JSONDecodeError): time.sleep(.2)
else: raise RuntimeError('playground startup failed; no isolation proof')
PY
)
docker exec "$project-main-1" python3 -c 'import json,urllib.request; u=json.loads(open("/tmp/playground.log").readline())["rpc_url"]; q=urllib.request.Request(u,data=b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"getHealth\"}",headers={"Content-Type":"application/json"}); assert json.load(urllib.request.urlopen(q))["result"]=="ok"'
python3 bench/backrun/check-isolation.py --project "$project" --raw-port "$port"

#!/usr/bin/env bash
# Full three-scenario acceptance; no retries, regeneration or best-of selection.
# SOURCE PROGRAMS CPMM_ADMIN ORCA_ADMIN FIXED_CALIBRATION_ARTIFACT OUT
set -euo pipefail
umask 077
repo=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo"
abs() { python3 -c 'import pathlib,sys;print(pathlib.Path(sys.argv[1]).resolve())' "$1"; }
source=$(abs "${1:?source fixture}")
programs=$(abs "${2:?program directory}")
export MTH_CPMM_ADMIN=$(abs "${3:?CPMM setup key}")
export MTH_ORCA_ADMIN=$(abs "${4:?Whirlpool setup key}")
artifact=$(abs "${5:?fixed native calibration artifact}")
root=$(abs "${6:?fresh acceptance output}")
test ! -e "$root" || { echo 'Acceptance output exists; preserve earlier runs.' >&2; exit 1; }
mkdir -p "$root"
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
export MTH_BACKRUN_PROGRAMS="$programs"
run_check() {
  local name=$1; shift
  local result=0
  "$@" > "$root/$name.log" 2>&1 || result=$?
  python3 - "$root" "$name" "$result" <<'PY'
import hashlib,json,pathlib,sys
root=pathlib.Path(sys.argv[1]);name=sys.argv[2];log=root/(name+'.log');path=root/'checks.json'
checks=json.loads(path.read_text()) if path.exists() else {}
checks[name]={'exit_code':int(sys.argv[3]),'log':str(log),'sha256':hashlib.sha256(log.read_bytes()).hexdigest()}
path.write_text(json.dumps(checks,indent=2)+'\n')
PY
  return "$result"
}
cargo build --locked -p mth-backrun -p mtx-cli > "$root/build.log" 2>&1
python3 bench/backrun/fixture_set.py build --source "$source" --out "$root/private-fixtures" > "$root/fixture.log"
export MTH_BACKRUN_FIXTURE="$root/private-fixtures/incentivized"
export BACKRUN_TEST_SET="$root/private-fixtures" BACKRUN_ORIGINAL_FIXTURE="$source"
run_check workspace cargo test --workspace --locked
run_check native cargo test --locked -p mth-backrun --test real_programs --test continuous -- --ignored --test-threads=1
run_check runtime_pin cargo test --locked -p mth-backrun --test runtime_pin
run_check python python3 -m unittest discover -s bench/backrun/tests -v
for target in runtime candidate development; do
  image=$target; test "$target" != development || image=toolchain
  docker build -f bench/backrun/Dockerfile --target "$target" -t "mth-backrun-$image:scenario-set-v1" . > "$root/$image-image.log" 2>&1
done
python3 bench/backrun/package_set.py --fixture-set "$root/private-fixtures" --programs "$programs" --out "$root/package" > "$root/package.log" 2>&1
run_check harbor_lint harbor lint "$root/package/tasks/scenario-01"
export MTH_BACKRUN_COMPOSE="$root/package/tasks/scenario-01/tests/docker-compose.yaml"
run_check isolation bash bench/backrun/isolation.sh
run_check public_boundary python3 bench/backrun/check_public.py --package "$root/package"
python3 bench/backrun/evaluate_set.py --release "$root/package" --calibration --artifact "$artifact" --jobs-dir "$root/jobs" --job-name smoke > "$root/smoke.log" 2>&1
# A native no-op participates in readiness and runs through the entire clock.
mkdir "$root/noop"
container=$(docker create mth-backrun-runtime:scenario-set-v1)
trap 'docker rm "$container" >/dev/null 2>&1 || true' EXIT
docker cp "$container:/usr/local/bin/mth-backrun" "$root/noop/bot"
docker rm "$container" >/dev/null
trap - EXIT
cat > "$root/noop/run.sh" <<'EOF'
#!/bin/bash
set -euo pipefail
exec ./bot reference --gateway "$MTH_BACKRUN_GATEWAY" --strategy 1 --idle
EOF
chmod 755 "$root/noop/run.sh" "$root/noop/bot"
printf '%s\n' '{"version":2,"executable":"bot","programs":[],"scratch":[]}' > "$root/noop/manifest.json"
python3 bench/backrun/evaluate_set.py --release "$root/package" --calibration --artifact "$root/noop" --jobs-dir "$root/jobs" --job-name noop > "$root/noop.log" 2>&1
mkdir "$root/replays"
python3 - "$root" <<'PY'
import json,pathlib,subprocess,sys
root=pathlib.Path(sys.argv[1]);report=json.loads((root/'jobs/smoke/report.json').read_text())
assert report['status']=='complete'
for scenario in report['scenarios']:
    trace=(root/'jobs/smoke'/scenario['report']).parent
    with (root/'replays'/(scenario['id']+'.json')).open('w') as output:
        subprocess.run(['target/debug/mth-backrun','replay','--trace',str(trace)],stdout=output,check=True)
PY
python3 bench/backrun/freeze_set.py --fixture-set "$root/private-fixtures" --package "$root/package" \
  --smoke "$root/jobs/smoke" --noop "$root/jobs/noop" --replays "$root/replays" --checks "$root/checks.json" --out "$root/release"

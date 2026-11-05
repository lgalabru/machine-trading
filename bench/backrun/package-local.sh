#!/usr/bin/env bash
# Build private/public image layers separately. Never push either image.
set -euo pipefail
umask 077
repo=$(cd "$(dirname "$0")/../.." && pwd)
fixture=$(cd "$1" && pwd)
programs=$(cd "$2" && pwd)
root=${3:-"$repo/work/backrun-package"}
verified=$("$repo/target/debug/mth-backrun" fixture verify --fixture "$fixture")
if [ -d "$root/public" ]; then
  python3 - "$root/public/fixture.json" "$verified" <<'PYCODE'
import json,sys
assert json.load(open(sys.argv[1]))['fixture_hash']==json.loads(sys.argv[2])['fixture_hash'], 'Package directory belongs to another fixture; use a fresh output directory.'
PYCODE
fi
mkdir -p "$root/verifier/fixture" "$root/verifier/programs"
cp "$fixture"/*.json "$root/verifier/fixture/"
cp "$programs"/*.so "$programs/manifest.json" "$root/verifier/programs/"
cp "$repo/bench/tasks/atomic-arb/tests/test.sh" "$root/verifier/test.sh"
cat > "$root/verifier/Dockerfile" <<'EOF'
FROM mth-backrun-runtime:local
COPY fixture /private/fixture
COPY programs /private/programs
COPY test.sh /tests/test.sh
RUN chmod -R go-rwx /private
EOF
docker build -t mth-backrun-verifier:local "$root/verifier"
if [ ! -d "$root/public" ]; then
  "$repo/target/debug/mth-backrun" fixture public --fixture "$fixture" --out "$root/public"
fi
mkdir -p "$root/development/public" "$root/development/programs"
cp "$root/public"/*.json "$root/development/public/"
cp "$programs/raydium_cp_swap.so" "$programs/whirlpool.so" "$programs/mth_backrun_executor.so" "$root/development/programs/"
cat > "$root/development/Dockerfile" <<'EOF'
FROM mth-backrun-toolchain:local
COPY public /opt/backrun/public
COPY programs /opt/backrun/binaries
RUN chmod -R a+rX /opt/backrun
EOF
docker build -t mth-backrun-development:local "$root/development"

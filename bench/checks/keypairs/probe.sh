#!/usr/bin/env bash
# Run the keypair probe bot in the runtime image, as the verifier runs bots:
# one market-making session (takers and a competitor, each with keypairs the
# grader writes), the grader as root, the bot as mtx-bot. Fails if the bot
# could read any of the grader's keypairs, or found none to try.
#   bench/checks/keypairs/probe.sh [IMAGE]
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
image="${1:-ghcr.io/solana-foundation/machine-trading:0.11.0-runtime}"
out="$(mktemp -d)"
trap 'rm -rf "$out"' EXIT
chmod 0777 "$out"
seed="$(python3 -c 'import secrets; print(secrets.token_hex(32))')"
docker run --rm -e MTX_EVAL_SEED="$seed" \
  -v "$here:/probe:ro" -v "$out:/out" "$image" \
  mtx grade --draw market-making --count 1 --anchor-jobs 1 --jobs 1 \
    --snapshot /opt/mtx/snapshots/phoenix-spcx-2026-10-29 \
    --bot /probe --logs /out/logs --report /out/report.json > /dev/null 2>&1 || true
log="$(find "$out/logs" -name '*.log' | head -1)"
[ -n "$log" ] || { echo "keypair probe: no bot log (the session didn't run)" >&2; exit 2; }
grep '^probe:' "$log" || true
summary="$(grep '^probe: dirs=' "$log" | tail -1)"
dirs="$(sed -n 's/.*dirs=\([0-9]*\).*/\1/p' <<< "$summary")"
readable="$(sed -n 's/.*readable=\([0-9]*\).*/\1/p' <<< "$summary")"
if [ -z "$dirs" ] || [ "$dirs" -eq 0 ]; then
  echo "keypair probe: the bot saw no keypair directories; nothing was tested" >&2
  exit 2
fi
if [ "$readable" -ne 0 ]; then
  echo "keypair probe: FAIL, the bot read $readable of the grader's keypairs" >&2
  exit 1
fi
echo "keypair probe: pass, $dirs directories seen, none readable"

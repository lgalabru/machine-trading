#!/usr/bin/env bash
# Run the keypair probe bot in the runtime image, as the verifier runs bots:
# one market-making session, the grader as root, the bot as mtx-bot. The bot
# (run.sh) checks what could leak to it: the grader's environment and
# MTX_EVAL_SEED, /tests and the verifier's files, the grader's temp
# directories, and any keypair JSON on disk. Fails only if one is readable;
# a surface that isn't there is clear, not a failure.
#   bench/checks/keypairs/probe.sh [IMAGE]
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
image="${1:-ghcr.io/solana-foundation/machine-trading:0.11.0-runtime}"
out="$(mktemp -d)"
# The bot-owned log files can't be removed by the invoking user; that is not a result.
trap 'rm -rf "$out" 2> /dev/null || true' EXIT
chmod 0777 "$out"
seed="$(python3 -c 'import secrets; print(secrets.token_hex(32))')"
# The verifier's layout, as the tasks' test.sh sets it: /tests closed to the
# bot, /logs reachable by it, the reference run on the same session and every
# log under /logs/verifier.
docker run --rm -e MTX_EVAL_SEED="$seed" \
  -v "$here:/probe:ro" -v "$out:/logs" "$image" bash -c '
    set -e
    mkdir -p /tests /logs/verifier
    echo "#!/bin/bash" > /tests/test.sh
    chmod -R go-rwx /tests
    chmod 0777 /logs /logs/verifier
    exec mtx grade --draw market-making-cbrs --count 1 --anchor-jobs 1 --jobs 1 \
      --reference /opt/mtx/anchors/market-making-cbrs \
      --snapshot /opt/mtx/snapshots/phoenix-cbrs-2026-11-25 \
      --bot /probe --logs /logs/verifier/bot \
      --reward /logs/verifier/reward.json --report /logs/verifier/report.json \
      --details /logs/verifier/sessions' > /dev/null 2>&1 || true
log="$(find "$out/verifier/bot" -maxdepth 1 -name '*.log' | head -1)"
[ -n "$log" ] || { echo "keypair probe: no bot log (the session didn't run)" >&2; exit 2; }
grep '^probe:' "$log" | grep -v '^probe: LEAK' || true
summary="$(grep '^probe: surfaces=' "$log" | tail -1)"
[ -n "$summary" ] || { echo "keypair probe: the bot never reported (it didn't run to the end of its checks)" >&2; exit 2; }
leaks="$(sed -n 's/.*leaks=\([0-9]*\).*/\1/p' <<< "$summary")"
if [ "$leaks" -ne 0 ]; then
  grep '^probe: LEAK' "$log" >&2 || true
  echo "keypair probe: FAIL, the bot reached $leaks leak(s)" >&2
  exit 1
fi
echo "keypair probe: pass, every surface checked, nothing readable"

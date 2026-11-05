#!/bin/bash
set -euo pipefail
umask 077
trap 'rm -f /logs/verifier/reward.txt' ERR
mth-backrun run --fixture /private/fixture --programs /private/programs \
  --artifact /app/bot --bootstrap /bootstrap --out /logs/verifier

mth-backrun grade --evidence /logs/verifier --fixture /private/fixture

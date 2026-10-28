#!/bin/bash
# Grade /app/bot on the hidden scenarios. The grader starts an offline
# surfnet per scenario, runs the bot as ott-bot behind an RPC guard, and
# writes the reward.
set -euo pipefail

# Hidden from the bot, readable by the bot's own user.
chmod -R go-rwx /tests
chmod -R a+rX /app/bot

ott grade \
  --snapshot "$OTT_SNAPSHOT" \
  --bot /app/bot \
  --logs /logs/verifier/bot \
  --reward /logs/verifier/reward.json \
  --report /logs/verifier/report.json \
  /tests/scenarios/*.json > /dev/null

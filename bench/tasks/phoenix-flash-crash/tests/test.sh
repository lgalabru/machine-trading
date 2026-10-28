#!/bin/bash
# Grade /app/bot on the hidden scenarios. The grader starts an offline
# surfnet per scenario, runs the bot as ott-bot behind an RPC guard, and
# writes the reward.
set -euo pipefail

# Hidden from the bot, readable by the bot's own user.
chmod -R go-rwx /tests
chmod -R a+rX /app/bot

# The bot can reach /logs (host bind mounts may ignore permissions). The
# grader treats a reward file a bot wrote as tampering; never leave one
# behind if the grader itself fails.
trap 'rm -f /logs/verifier/reward.json' ERR

# Three sessions at once: ~1 GB each, on the verifier's 4 CPUs and 6 GB. A
# verifier too loaded to keep their clocks on time fails the grade (late
# slots) rather than skewing it.
ott grade \
  --jobs "${OTT_JOBS:-3}" \
  --snapshot "$OTT_SNAPSHOT" \
  --bot /app/bot \
  --logs /logs/verifier/bot \
  --reward /logs/verifier/reward.json \
  --report /logs/verifier/report.json \
  /tests/scenarios/*.json > /dev/null

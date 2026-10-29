#!/bin/bash
# Grade /app/bot on the hidden scenarios. The grader starts an offline
# surfnet per scenario, runs the bot as mt-bot behind an RPC guard, and
# writes the reward.
set -euo pipefail

# Hidden from the bot, readable by the bot's own user.
chmod -R go-rwx /tests
chmod -R a+rX /app/bot

# The bot can reach /logs (host bind mounts may ignore permissions). The
# grader treats a reward file a bot wrote as tampering; never leave one
# behind if the grader itself fails.
trap 'rm -f /logs/verifier/reward.json' ERR

# One session at a time. Concurrent sessions share the container, so a bot
# can reach the other sessions' surfnets while they are still being set up,
# before their cheatcodes are locked. Parallel grading (--jobs) is for
# trusted bots, such as calibrating the reference, until sessions are
# isolated from one another.
mt grade \
  --jobs 1 \
  --snapshot "$MT_SNAPSHOT" \
  --bot /app/bot \
  --logs /logs/verifier/bot \
  --reward /logs/verifier/reward.json \
  --report /logs/verifier/report.json \
  /tests/scenarios/*.json > /dev/null

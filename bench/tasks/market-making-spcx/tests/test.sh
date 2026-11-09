#!/bin/bash
# Grade /app/bot on a hidden set drawn now, from the batch's secret
# (MTX_EVAL_SEED, passed by Harbor from the host; the task never holds it).
# The grader runs the reference on the same sessions first, starts an
# offline surfnet per session, runs the bot as mtx-bot behind an RPC guard,
# and writes the reward.
set -euo pipefail

# Hidden from the bot, readable by the bot's own user.
chmod -R go-rwx /tests
chmod -R a+rX /app/bot

# The bot can reach /logs (host bind mounts may ignore permissions). The
# grader treats a reward file a bot wrote as tampering; never leave one
# behind if the grader itself fails.
trap 'rm -f /logs/verifier/reward.json' ERR

if [ -z "${MTX_EVAL_SEED:-}" ]; then
  echo "test.sh: MTX_EVAL_SEED is empty: the batch didn't pass its secret, and" \
    "hidden sets are drawn from it. Export one before harbor run." >&2
  exit 3
fi

# The bot's sessions run one at a time. Concurrent sessions share the
# container, so a bot could reach another session's surfnet while it was
# being set up, before its cheatcodes locked. The reference is trusted:
# its sessions run four at a time, before the bot's.
mtx grade \
  --draw market-making \
  --count 72 \
  --reference /opt/mtx/anchors/market-making \
  --anchor-jobs 4 \
  --jobs 1 \
  --stop-on-disqualify \
  --snapshot "$MTX_SNAPSHOT" \
  --bot /app/bot \
  --logs /logs/verifier/bot \
  --reward /logs/verifier/reward.json \
  --report /logs/verifier/report.json \
  --details /logs/verifier/sessions > /dev/null

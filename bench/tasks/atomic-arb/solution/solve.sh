#!/bin/bash
# Harbor oracle calibration only. This is not mounted for normal agents.
set -euo pipefail
mkdir -p /app/bot
cp /solution/artifact/* /app/bot/
chmod 755 /app/bot/run.sh /app/bot/bot

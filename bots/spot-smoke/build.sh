#!/usr/bin/env bash
# Build the bot into bin/bot, where run.sh and buyer/run.sh start it.
set -euo pipefail
cd "$(dirname "$0")"
exe="$(cargo build --release --bin mtx-spot-smoke --message-format=json-render-diagnostics \
  | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)"
mkdir -p bin
cp "$exe" bin/bot
echo "built bin/bot from $exe"

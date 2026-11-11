#!/usr/bin/env bash
# Build the bot into bin/bot, where run.sh starts it. Run this before
# grading: the grader runs bin/bot as-is, without building anything.
set -euo pipefail
if ! command -v cargo >/dev/null 2>&1; then
  export PATH="/usr/local/cargo/bin:$PATH"
fi
cd "$(dirname "$0")"
exe="$(cargo build --release --bin bot --message-format=json-render-diagnostics \
  | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)"
mkdir -p bin
cp "$exe" bin/bot.next
mv -f bin/bot.next bin/bot
echo "built bin/bot from $exe"

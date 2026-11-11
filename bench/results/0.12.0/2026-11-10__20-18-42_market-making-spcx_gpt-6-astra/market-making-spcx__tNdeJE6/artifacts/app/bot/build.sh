#!/usr/bin/env bash
# Build the bot into bin/bot, where run.sh starts it. Run this before
# grading: the grader runs bin/bot as-is, without building anything.
set -euo pipefail
cd "$(dirname "$0")"
if ! command -v cargo >/dev/null 2>&1; then
  export PATH="/usr/local/rustup/toolchains/1.97.1-aarch64-unknown-linux-gnu/bin:$PATH"
fi
exe="$(cargo build --release --bin bot --message-format=json-render-diagnostics \
  | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)"
mkdir -p bin
cp "$exe" bin/bot.new
chmod 755 bin/bot.new
mv -f bin/bot.new bin/bot
echo "built bin/bot from $exe"

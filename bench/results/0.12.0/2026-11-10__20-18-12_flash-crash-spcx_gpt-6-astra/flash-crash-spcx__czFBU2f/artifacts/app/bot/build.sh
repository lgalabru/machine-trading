#!/usr/bin/env bash
# Build the bot into bin/bot, where run.sh starts it. Run this before
# grading: the grader runs bin/bot as-is, without building anything.
set -euo pipefail
cd "$(dirname "$0")"
# The development image keeps Rust here; run.sh never invokes the toolchain.
if ! command -v cargo >/dev/null 2>&1 && [ -x /usr/local/cargo/bin/cargo ]; then
  export PATH="/usr/local/cargo/bin:$PATH"
fi
exe="$(cargo build --release --bin spcx-shock-bot --message-format=json-render-diagnostics \
  | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)"
mkdir -p bin
# Publish atomically, so a failed build cannot disturb the last working bot.
cp "$exe" bin/bot.new
chmod 755 bin/bot.new
mv -f bin/bot.new bin/bot
echo "built bin/bot from $exe"

#!/usr/bin/env bash
# Build the bot into bin/bot, where run.sh starts it. Run this before
# grading: the grader runs bin/bot as-is, without building anything.
set -euo pipefail
cd "$(dirname "$0")"
export PATH="/usr/local/cargo/bin:$PATH"
# Several experiment crates share the development target directory.
# Force this crate to produce its own executable before copying it.
touch src/main.rs
exe="$(cargo build --release --bin bot --message-format=json-render-diagnostics \
  | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)"
mkdir -p bin
cp "$exe" bin/bot.next
chmod 755 bin/bot.next
mv -f bin/bot.next bin/bot
echo "built bin/bot from $exe"

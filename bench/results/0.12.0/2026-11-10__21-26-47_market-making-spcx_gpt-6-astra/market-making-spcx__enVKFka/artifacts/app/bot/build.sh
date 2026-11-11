#!/usr/bin/env bash
# Build the bot into bin/bot, where run.sh starts it. Run this before
# grading: the grader runs bin/bot as-is, without building anything.
set -euo pipefail
export PATH="/usr/local/rustup/toolchains/1.97.1-aarch64-unknown-linux-gnu/bin:$PATH"
cd "$(dirname "$0")"
# Other bot crates share this target directory and executable name.
# Force the entrypoint to rebuild so a cached artifact cannot belong to them.
touch src/main.rs
exe="$(cargo build --release --bin bot --message-format=json-render-diagnostics \
  | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)"
mkdir -p bin
cp "$exe" bin/bot.new
mv bin/bot.new bin/bot
echo "built bin/bot from $exe"

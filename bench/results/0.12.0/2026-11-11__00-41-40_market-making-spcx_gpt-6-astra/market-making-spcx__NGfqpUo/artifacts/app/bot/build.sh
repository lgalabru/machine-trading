#!/usr/bin/env bash
# Build the bot into bin/bot, where run.sh starts it. Run this before
# grading: the grader runs bin/bot as-is, without building anything.
set -euo pipefail
cd "$(dirname "$0")"
export PATH="/usr/local/cargo/bin:$PATH"
exe="$(cargo build --release --bin bot --message-format=json-render-diagnostics \
  | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)"
mkdir -p bin
output="${MTX_BUILD_OUTPUT:-bin/bot}"
mkdir -p "$(dirname "$output")"
cp "$exe" "$output.new"
chmod 755 "$output.new"
mv -f "$output.new" "$output"
echo "built $output from $exe"

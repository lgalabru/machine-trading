#!/usr/bin/env bash
# Build the bot into bin/bot, where run.sh starts it. Run this before
# grading: the grader runs bin/bot as-is, without building anything.
set -euo pipefail
if ! command -v cargo >/dev/null 2>&1 && [[ -x /usr/local/cargo/bin/cargo ]]; then
  export PATH="/usr/local/cargo/bin:$PATH"
fi
cd "$(dirname "$0")"
exe="$(cargo rustc --release --bin bot --message-format=json-render-diagnostics -- -C target-feature=+crt-static \
  | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)"
out="${BOT_OUTPUT:-bin/bot}"
mkdir -p "$(dirname "$out")"
cp "$exe" "$out.new"
chmod 755 "$out.new"
mv -f "$out.new" "$out"
echo "built $out from $exe"

#!/usr/bin/env bash
# Build and atomically install the self-contained native executable.
set -euo pipefail
cd "$(dirname "$0")"
if ! command -v cargo >/dev/null 2>&1; then
  export PATH="/usr/local/cargo/bin:$PATH"
fi
exe="$(cargo build --release --bin bot --message-format=json-render-diagnostics \
  | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)"
mkdir -p bin
staged="$(mktemp bin/.bot.XXXXXX)"
trap 'rm -f "$staged"' EXIT
install -m 0755 "$exe" "$staged"
mv -f "$staged" bin/bot
echo "built bin/bot from $exe"

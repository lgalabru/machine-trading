#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
if ! command -v cargo >/dev/null 2>&1; then
  export PATH="/usr/local/cargo/bin:$PATH"
fi
exe="$(cargo build --release --bin bot --message-format=json-render-diagnostics \
  | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)"
test -x "$exe"
mkdir -p bin
install -m 755 "$exe" bin/.bot.next
mv -f bin/.bot.next bin/bot
echo "built bin/bot from $exe"

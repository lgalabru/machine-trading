#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
if ! command -v cargo >/dev/null 2>&1 && [ -x /usr/local/cargo/bin/cargo ]; then
  export PATH="/usr/local/cargo/bin:$PATH"
fi
exe="$(cargo build --release --bin bot --message-format=json-render-diagnostics \
  | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)"
test -n "$exe"
mkdir -p bin
# Preserve the last executable if compilation or copying is interrupted.
cp "$exe" bin/.bot.next
chmod 755 bin/.bot.next
mv -f bin/.bot.next bin/bot
echo "built bin/bot from $exe"

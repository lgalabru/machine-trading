#!/usr/bin/env bash
# Build and atomically replace the executable. run.sh never builds anything.
set -euo pipefail
cd "$(dirname "$0")"
if ! command -v cargo >/dev/null && [[ -x /usr/local/cargo/bin/cargo ]]; then
  export PATH="/usr/local/cargo/bin:$PATH"
fi
exe="$(cargo build --locked --release --bin bot --message-format=json-render-diagnostics \
  | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)"
test -n "$exe"
mkdir -p bin
next="$(mktemp bin/.bot.XXXXXX)"
trap 'rm -f "$next"' EXIT
install -m 755 "$exe" "$next"
mv -f "$next" bin/bot
echo "built bin/bot from $exe"

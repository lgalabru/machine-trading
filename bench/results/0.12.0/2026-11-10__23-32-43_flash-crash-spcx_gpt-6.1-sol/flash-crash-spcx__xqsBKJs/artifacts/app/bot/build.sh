#!/usr/bin/env bash
set -euo pipefail
export PATH="/usr/local/cargo/bin:$PATH"
cd "$(dirname "$0")"
exe="$(cargo build --release --bin bot --message-format=json-render-diagnostics \
  | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)"
out="${BOT_BUILD_OUT:-bin/bot}"
mkdir -p "$(dirname "$out")"
cp "$exe" "$out.tmp"
chmod 755 "$out.tmp"
mv "$out.tmp" "$out"
echo "built $out from $exe"

#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
export PATH="/usr/local/cargo/bin:$PATH"
exe="$(cargo build --release --bin bot --message-format=json-render-diagnostics \
  | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)"
dest="${BOT_BUILD_DEST:-bin/bot}"
mkdir -p "$(dirname "$dest")"
cp "$exe" "$dest.new"
chmod 755 "$dest.new"
mv "$dest.new" "$dest"
echo "built $dest from $exe"

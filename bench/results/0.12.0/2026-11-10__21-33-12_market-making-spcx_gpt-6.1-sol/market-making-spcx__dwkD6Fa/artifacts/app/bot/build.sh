#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
if ! command -v cargo >/dev/null 2>&1; then
  for toolchain_bin in /usr/local/rustup/toolchains/*/bin; do
    if [[ -x "$toolchain_bin/cargo" ]]; then
      export PATH="$toolchain_bin:$PATH"
      break
    fi
  done
fi
exe="$(cargo build --release --bin bot --message-format=json-render-diagnostics \
  | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)"
[[ -n "$exe" && -x "$exe" ]]
mkdir -p bin
next="$(mktemp bin/.bot.XXXXXX)"
trap 'rm -f "$next"' EXIT
cp "$exe" "$next"
chmod 755 "$next"
mv -f "$next" bin/bot
echo "built bin/bot from $exe"

#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
if ! command -v cargo >/dev/null 2>&1 && [[ -x /usr/local/cargo/bin/cargo ]]; then
  export PATH="/usr/local/cargo/bin:$PATH"
fi
exe="$(cargo build --release --bin bot --message-format=json-render-diagnostics \
  | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)"
mkdir -p bin
build_stage="$(mktemp bin/.bot-build.XXXXXX)"
trap 'rm -f "$build_stage"' EXIT
cp "$exe" "$build_stage"
chmod 755 "$build_stage"
"$build_stage" --self-test
mv -f "$build_stage" bin/bot
echo "built bin/bot from $exe"

#!/usr/bin/env bash
# Build each rung's component into checks/rungs/<rung>/bot.wasm: the rung is
# set at build time (`MTX_COB_RUNG`), since a component has no environment.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
cd "$here/../reference"
for rung in "${@:-reference informed informed-day copy-all copy-1in8 copy-late copy-small safe-blind twap nop}"; do
  for r in $rung; do
    wasm="$(MTX_COB_RUNG=$r cargo build --release --lib --target wasm32-wasip2 --message-format=json-render-diagnostics \
      | sed -n 's/.*"filenames":\["\([^"]*\.wasm\)".*/\1/p' | tail -1)"
    mkdir -p "$here/rungs/$r"
    cp "$wasm" "$here/rungs/$r/bot.wasm"
    echo "built rungs/$r/bot.wasm"
  done
done

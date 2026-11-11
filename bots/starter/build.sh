#!/usr/bin/env bash
# Build the strategy into bot.wasm, the component the grader loads. Run this
# before grading: the grader loads bot.wasm as it is, and builds nothing.
set -euo pipefail
cd "$(dirname "$0")"
wasm="$(cargo build --release --lib --target wasm32-wasip2 --message-format=json-render-diagnostics \
  | sed -n 's/.*"filenames":\["\([^"]*\.wasm\)".*/\1/p' | tail -1)"
cp "$wasm" bot.wasm
echo "built bot.wasm from $wasm"

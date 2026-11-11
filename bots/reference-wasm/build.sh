#!/usr/bin/env bash
# Build the strategy into bot.wasm, the component the grader loads: a bot
# directory with a bot.wasm is graded as a component (RFD 0002), not run.
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/mtx_bot_reference_wasm.wasm bot.wasm
echo "built bot.wasm ($(wc -c < bot.wasm) bytes)"

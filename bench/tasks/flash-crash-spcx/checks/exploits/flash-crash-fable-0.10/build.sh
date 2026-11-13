#!/usr/bin/env bash
# Build the bot into bin/bot, where run.sh starts it. Run this before
# grading: the grader runs bin/bot as-is, without building anything.
#
# The binary is linked statically (crt-static), so it needs nothing from the
# grader's image but the kernel: no libc, libm or libgcc at runtime.
set -euo pipefail
cd "$(dirname "$0")"
target="$(rustc -vV | sed -n 's/^host: //p')"
exe="$(RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --bin bot \
  --target "$target" --message-format=json-render-diagnostics \
  | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)"
mkdir -p bin
cp "$exe" bin/bot
echo "built bin/bot from $exe"

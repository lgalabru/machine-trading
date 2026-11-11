#!/usr/bin/env bash
# Build the bot into bin/bot, where run.sh starts it. Run this before
# grading: the grader runs bin/bot as-is, without building anything.
set -euo pipefail
cd "$(dirname "$0")"
export PATH="/usr/local/rustup/toolchains/1.97.1-aarch64-unknown-linux-gnu/bin:$PATH"
exe="$(cargo build --release --bin bot --message-format=json-render-diagnostics \
  | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)"
mkdir -p bin lib
# Ship the Rust unwinding runtime; libc/libm are provided by Debian.
libgcc="$(ldd "$exe" | sed -n 's/.*libgcc_s.so.1 => \([^ ]*\).*/\1/p')"
if [ -n "$libgcc" ]; then
  cp "$libgcc" lib/libgcc_s.so.1.next
  mv -f lib/libgcc_s.so.1.next lib/libgcc_s.so.1
fi
cp "$exe" bin/bot.next
chmod 755 bin/bot.next
mv -f bin/bot.next bin/bot
echo "built bin/bot from $exe"

#!/usr/bin/env bash
set -euo pipefail
export PATH="/usr/local/cargo/bin:$PATH"
cd "$(dirname "$0")"
exe="$(cargo build --release --bin spcx-bot --message-format=json-render-diagnostics \
  | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)"
mkdir -p bin lib
cp "$exe" bin/bot.next
chmod 755 bin/bot.next
# Debian's C runtime is supplied by the runner; bundle the GCC unwinder.
cp "$(cc -print-file-name=libgcc_s.so.1)" lib/libgcc_s.so.1
mv -f bin/bot.next bin/bot
sha256sum bin/bot > bin/bot.sha256
printf 'Built %s\n' "$exe"

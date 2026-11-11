#!/usr/bin/env bash
# The native bot and its non-libc support library are shipped together.
set -euo pipefail
bot_dir="$(dirname -- "$0")"
export LD_LIBRARY_PATH="$bot_dir/lib"
exec "$bot_dir/bin/bot"

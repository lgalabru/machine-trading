#!/usr/bin/env bash
set -euo pipefail
bot_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
export LD_LIBRARY_PATH="$bot_dir/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
exec "$bot_dir/bin/bot"

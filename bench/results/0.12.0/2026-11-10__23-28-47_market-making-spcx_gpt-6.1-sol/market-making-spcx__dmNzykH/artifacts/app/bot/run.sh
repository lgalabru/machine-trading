#!/usr/bin/env bash
set -euo pipefail
export RUST_LOG=warn
exec "$(dirname "$0")/bin/bot"

#!/usr/bin/env bash
# Build the bot into bin/bot, where run.sh starts it. Run this before
# grading: the grader runs bin/bot as-is, without building anything.
# `build.sh --passive` builds the passive baseline instead.
set -euo pipefail
cd "$(dirname "$0")"
features=()
[ "${1:-}" = --passive ] && features=(--features passive)
exe="$(cargo build --release --bin mth-maker ${features[@]+"${features[@]}"} --message-format=json-render-diagnostics \
  | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)"
mkdir -p bin
cp "$exe" bin/bot
echo "built bin/bot from $exe"

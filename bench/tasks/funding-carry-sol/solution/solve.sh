#!/bin/bash
# Reference solution: the carry strategy from the task's reference/ in the
# mtx repository, a strategy component built into the starter's place. It
# needs nothing the starter doesn't list.
set -euo pipefail
cp /solution/src/*.rs /app/bot/src/
cd /app/bot
just --justfile /app/bot/Justfile build

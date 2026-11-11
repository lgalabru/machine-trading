#!/bin/bash
# Reference solution: the carry desk from bots/carry-wasm in the mtx
# repository, a strategy component built into the starter's place, with the
# tree ensemble it embeds.
set -euo pipefail
cp /solution/src/*.rs /app/bot/src/
cp /solution/model.bin /app/bot/
just --justfile /app/bot/Justfile build

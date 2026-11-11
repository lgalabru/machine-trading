#!/bin/bash
# Reference solution: the carry desk from bots/carry in the mtx
# repository, a strategy component built into the starter's place, with the
# tree ensemble it embeds. It needs serde_json, which the starter doesn't
# list; it is in the SDK's lockfile.
set -euo pipefail
cp /solution/src/*.rs /app/bot/src/
cp /solution/model.bin /app/bot/
cd /app/bot
# [dependencies] is the starter's last section. Pinned to the version the
# SDK's lockfile (copied in as /app/bot/Cargo.lock) holds.
grep -q '^serde_json = ' Cargo.toml || printf '%s\n' 'serde_json = "=1.0.154"' >> Cargo.toml
just --justfile /app/bot/Justfile build

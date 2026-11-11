#!/bin/bash
# Reference solution: the weekend strategy from bots/weekend-wasm in the mtx
# repository (its sources and its trained forecaster), a strategy component
# built into the starter's place. It needs three crates the starter doesn't
# list; all are in the SDK's lockfile.
set -euo pipefail
cp /solution/src/*.rs /app/bot/src/
mkdir -p /app/bot/model
cp /solution/model/*.json /app/bot/model/
cd /app/bot
# [dependencies] is the starter's last section. Pinned to the versions the
# SDK's lockfile (copied in as /app/bot/Cargo.lock) holds.
while read -r name spec; do
  grep -q "^$name = " Cargo.toml || printf '%s = %s\n' "$name" "$spec" >> Cargo.toml
done <<'DEPS'
borsh "=1.8.1"
bs58 "=0.5.1"
serde_json "=1.0.154"
DEPS
just --justfile /app/bot/Justfile build

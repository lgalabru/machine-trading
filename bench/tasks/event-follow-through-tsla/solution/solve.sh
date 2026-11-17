#!/bin/bash
# Reference solution: the task's reference from reference/ in the mtx
# repository, a strategy component built into the starter's place, with the
# two crates it reads the chain's answers with (both already in the SDK's
# dependencies).
set -euo pipefail
cp /solution/src/lib.rs /app/bot/src/lib.rs
grep -q '^serde_json' /app/bot/Cargo.toml ||
  sed -i '/^\[dependencies\]/a bs58 = "0.5"\nserde_json = "1"' /app/bot/Cargo.toml
just --justfile /app/bot/Justfile build

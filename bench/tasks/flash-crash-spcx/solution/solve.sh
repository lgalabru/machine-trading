#!/bin/bash
# Reference solution: the flash-crash reference from bots/reference-wasm in
# the mtx repository, a strategy component built into the starter's place.
set -euo pipefail
cp /solution/src/lib.rs /app/bot/src/lib.rs
/app/bot/build.sh

#!/bin/bash
# Reference solution: the inventory-skewing maker from bots/maker-wasm in the
# mtx repository, a strategy component built into the starter's place.
set -euo pipefail
cp /solution/src/lib.rs /app/bot/src/lib.rs
/app/bot/build.sh

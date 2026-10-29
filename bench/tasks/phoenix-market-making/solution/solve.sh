#!/bin/bash
# Reference solution: the inventory-skewing maker from bots/maker in the mth
# repository, built into the starter's place.
set -euo pipefail
cp /solution/src/main.rs /app/bot/src/main.rs
/app/bot/build.sh

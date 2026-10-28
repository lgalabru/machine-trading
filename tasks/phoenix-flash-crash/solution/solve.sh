#!/bin/bash
# Reference solution: the ladder-bid strategy from bots/reference in the ott
# repository, built into the starter's place.
set -euo pipefail
cp /solution/src/main.rs /app/bot/src/main.rs
/app/bot/build.sh

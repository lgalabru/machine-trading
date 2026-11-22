#!/bin/bash
# Reference solution: the liquidation-flash reference from the task's
# reference/ in the mtx repository (its strategy sources, lib.rs and the
# liquidator module it uses), a strategy component built into the starter's
# place.
set -euo pipefail
cp /solution/src/*.rs /app/bot/src/
just --justfile /app/bot/Justfile build

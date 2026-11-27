#!/bin/bash
# Reference solution: the flash-crash reference from the task's reference/ in
# the mtx repository, a strategy component built into the starter's place.
# Every source file: the reference keeps its rider in a module of its own.
set -euo pipefail
cp /solution/src/*.rs /app/bot/src/
just --justfile /app/bot/Justfile build

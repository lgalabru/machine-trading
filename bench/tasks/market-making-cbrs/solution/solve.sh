#!/bin/bash
# Reference solution: the inventory-skewing maker from the task's reference/ in the
# mtx repository, a strategy component built into the starter's place. Every
# source file: the reference reads its tape and markouts in modules of their own.
set -euo pipefail
cp /solution/src/*.rs /app/bot/src/
just --justfile /app/bot/Justfile build

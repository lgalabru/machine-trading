#!/bin/bash
# Reference solution: the backrun-sol-usdc reference from the task's
# reference/ in the mtx repository (its strategy sources, lib.rs and the
# backrunner module it uses), a strategy component built into the
# starter's place, with the kit's two pool decoders turned on.
set -euo pipefail
cp /solution/src/*.rs /app/bot/src/
sed -i 's|features = \["component", "venue"\]|features = ["component", "venue", "orca-whirlpool-venue", "raydium-clmm-venue"]|' /app/bot/Cargo.toml
grep -q 'raydium-clmm-venue' /app/bot/Cargo.toml
just --justfile /app/bot/Justfile build

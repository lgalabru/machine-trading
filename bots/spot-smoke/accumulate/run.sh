#!/usr/bin/env bash
# Buys 1 SOL on each spot market in its mandate, once.
exec "$(dirname "$0")/../bin/bot" accumulate

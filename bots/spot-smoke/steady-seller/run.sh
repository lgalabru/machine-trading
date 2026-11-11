#!/usr/bin/env bash
# Sells 4 SOL on Phoenix each of its first three turns.
exec "$(dirname "$0")/../bin/bot" steady sell 4 3

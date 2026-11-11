#!/usr/bin/env bash
# Buys 3 SOL on Phoenix each of its first six turns.
exec "$(dirname "$0")/../bin/bot" steady buy 3 6

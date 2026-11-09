#!/usr/bin/env bash
# A ladder rung (see ../README.md): the reference, bots/carry, run as its
# `told` rung.
export MTX_RUNG=told
exec "$(dirname "$0")/../../../../bots/carry/bin/bot"

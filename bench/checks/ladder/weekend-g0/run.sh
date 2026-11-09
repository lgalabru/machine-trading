#!/usr/bin/env bash
# A ladder rung (see ../README.md): the weekend-gap reference, bots/weekend,
# run as its `g0` rung.
export MTX_RUNG=g0
exec "$(dirname "$0")/../../../../bots/weekend/bin/bot"

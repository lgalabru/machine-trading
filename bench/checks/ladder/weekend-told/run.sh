#!/usr/bin/env bash
# A ladder rung (see ../README.md): the weekend-gap reference, bots/weekend,
# run as its `told` rung.
export MTX_RUNG=told
exec "$(dirname "$0")/../../../../bots/weekend/bin/bot"

#!/usr/bin/env bash
# A ladder rung (see ../README.md): the weekend-gap reference, bots/weekend,
# run as its `follow` rung.
export MTX_RUNG=follow
exec "$(dirname "$0")/../../../../bots/weekend/bin/bot"

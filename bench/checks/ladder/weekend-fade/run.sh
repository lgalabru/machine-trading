#!/usr/bin/env bash
# A ladder rung (see ../README.md): the weekend-gap reference, bots/weekend,
# run as its `fade` rung.
export MTX_RUNG=fade
exec "$(dirname "$0")/../../../../bots/weekend/bin/bot"

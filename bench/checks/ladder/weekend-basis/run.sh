#!/usr/bin/env bash
# A ladder rung (see ../README.md): the weekend-gap reference, bots/weekend,
# run as its `basis` rung.
export MTX_RUNG=basis
exec "$(dirname "$0")/../../../../bots/weekend/bin/bot"

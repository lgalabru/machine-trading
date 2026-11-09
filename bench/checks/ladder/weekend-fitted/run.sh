#!/usr/bin/env bash
# A ladder rung (see ../README.md): the weekend-gap reference, bots/weekend,
# run as its `fitted` rung.
export MTX_RUNG=fitted
exec "$(dirname "$0")/../../../../bots/weekend/bin/bot"

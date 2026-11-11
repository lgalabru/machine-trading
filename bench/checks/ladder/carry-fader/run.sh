#!/usr/bin/env bash
# A ladder rung (see ../README.md): the gradient-boosted bot, bots/carry-gbm, run as its
# `fader` rung.
export MTX_RUNG=fader
exec "$(dirname "$0")/../../../../bots/carry-gbm/bin/bot"

#!/usr/bin/env bash
# A ladder rung (see ../README.md): the gradient-boosted bot, bots/carry-gbm, run as its
# `follower` rung.
export MTX_RUNG=follower
exec "$(dirname "$0")/../../../../bots/carry-gbm/bin/bot"

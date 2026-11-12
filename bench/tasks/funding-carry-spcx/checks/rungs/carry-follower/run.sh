#!/usr/bin/env bash
# A ladder rung (see bench/checks/ladder/README.md): the gradient-boosted bot (checks/bots/carry-gbm), run as its
# `follower` rung.
export MTX_RUNG=follower
exec "$(dirname "$0")/../../bots/carry-gbm/bin/bot"

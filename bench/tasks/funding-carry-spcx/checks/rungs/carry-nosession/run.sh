#!/usr/bin/env bash
# A ladder rung (see bench/checks/ladder/README.md): the gradient-boosted bot (checks/bots/carry-gbm), run as its
# `nosession` rung.
export MTX_RUNG=nosession
exec "$(dirname "$0")/../../bots/carry-gbm/bin/bot"

#!/usr/bin/env bash
# A ladder rung (see bench/checks/ladder/README.md): the gradient-boosted bot, bots/carry-gbm, run as its
# `practice` rung.
export MTX_RUNG=practice
exec "$(dirname "$0")/../../bots/carry-gbm/bin/bot"

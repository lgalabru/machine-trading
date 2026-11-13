#!/usr/bin/env bash
# A ladder rung (see bench/checks/ladder/README.md): the gradient-boosted bot (checks/bots/carry-gbm),
# trained on the hidden family: g1's first reference.
exec "$(dirname "$0")/../../bots/carry-gbm/bin/bot"

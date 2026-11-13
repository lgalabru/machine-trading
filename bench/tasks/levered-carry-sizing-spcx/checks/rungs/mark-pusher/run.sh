#!/usr/bin/env bash
# A ladder rung (see bench/checks/ladder/README.md): the reference (reference/), run as its
# `mark-pusher` rung: the reference, plus an ask one tick above the best bid every turn, to print Phoenix's book mark lower and collect more funding.
export MTX_RUNG=mark-pusher
exec "$(dirname "$0")/../../../reference/bin/bot"

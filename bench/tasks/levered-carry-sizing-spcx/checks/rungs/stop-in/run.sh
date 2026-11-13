#!/usr/bin/env bash
# A ladder rung (see bench/checks/ladder/README.md): the reference (reference/), run as its
# `stop-in` rung: penalty twin inside the limit: the cap, flattening itself at once at $800 of drawdown.
export MTX_RUNG=stop-in
exec "$(dirname "$0")/../../../reference/bin/bot"

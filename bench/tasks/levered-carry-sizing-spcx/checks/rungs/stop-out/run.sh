#!/usr/bin/env bash
# A ladder rung (see bench/checks/ladder/README.md): the reference (reference/), run as its
# `stop-out` rung: penalty twin outside the limit: the cap, left for the risk desk to close at $1,000.
export MTX_RUNG=stop-out
exec "$(dirname "$0")/../../../reference/bin/bot"

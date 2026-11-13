#!/usr/bin/env bash
# A ladder rung (see bench/checks/ladder/README.md): the reference (reference/), run as its
# `oracle` rung: the path oracle: told the volatility state and the flush, flat around the flush.
export MTX_RUNG=oracle
exec "$(dirname "$0")/../../../reference/bin/bot"

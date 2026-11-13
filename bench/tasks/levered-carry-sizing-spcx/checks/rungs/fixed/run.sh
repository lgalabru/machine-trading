#!/usr/bin/env bash
# A ladder rung (see bench/checks/ladder/README.md): the reference (reference/), run as its
# `fixed` rung: careful but unaware of the volatility: a constant 30% of the cap (3x collateral).
export MTX_RUNG=fixed
exec "$(dirname "$0")/../../../reference/bin/bot"

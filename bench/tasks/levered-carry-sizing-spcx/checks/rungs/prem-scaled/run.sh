#!/usr/bin/env bash
# A ladder rung (see bench/checks/ladder/README.md): the reference (reference/), run as its
# `prem-scaled` rung: sized by the carry's yield alone, ignoring the volatility.
export MTX_RUNG=prem-scaled
exec "$(dirname "$0")/../../../reference/bin/bot"

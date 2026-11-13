#!/usr/bin/env bash
# A ladder rung (see bench/checks/ladder/README.md): the reference (reference/), run as its
# `max-lever` rung: naive full-size carry: the cap from the start, whatever the volatility.
export MTX_RUNG=max-lever
exec "$(dirname "$0")/../../../reference/bin/bot"

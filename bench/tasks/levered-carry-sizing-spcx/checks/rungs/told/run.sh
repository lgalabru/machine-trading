#!/usr/bin/env bash
# A ladder rung (see bench/checks/ladder/README.md): the reference (reference/), run as its
# `told` rung: told the session's volatility state: the cap in calm, flat in stress.
export MTX_RUNG=told
exec "$(dirname "$0")/../../../reference/bin/bot"

#!/usr/bin/env bash
# A ladder rung (see bench/checks/ladder/README.md): the weekend-gap reference (reference/),
# run as its `g0` rung.
export MTX_RUNG=g0
exec "$(dirname "$0")/../../../reference/bin/bot"

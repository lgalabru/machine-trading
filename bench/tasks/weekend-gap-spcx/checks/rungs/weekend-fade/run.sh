#!/usr/bin/env bash
# A ladder rung (see bench/checks/ladder/README.md): the weekend-gap reference (reference/),
# run as its `fade` rung.
export MTX_RUNG=fade
exec "$(dirname "$0")/../../../reference/bin/bot"

#!/usr/bin/env bash
# A ladder rung (see bench/checks/ladder/README.md): the weekend-gap reference (reference/),
# run as its `follow` rung.
export MTX_RUNG=follow
exec "$(dirname "$0")/../../../reference/bin/bot"

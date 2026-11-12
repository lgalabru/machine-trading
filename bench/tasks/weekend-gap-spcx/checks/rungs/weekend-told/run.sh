#!/usr/bin/env bash
# A ladder rung (see bench/checks/ladder/README.md): the weekend-gap reference, bots/weekend,
# run as its `told` rung.
export MTX_RUNG=told
exec "$(dirname "$0")/../../../reference/bin/bot"

#!/usr/bin/env bash
# A ladder rung (see bench/checks/ladder/README.md): the weekend-gap reference, bots/weekend,
# run as its `fitted` rung.
export MTX_RUNG=fitted
exec "$(dirname "$0")/../../../reference/bin/bot"

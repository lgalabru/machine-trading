#!/usr/bin/env bash
# A ladder rung (see bench/checks/ladder/README.md): the weekend-gap reference, bots/weekend,
# run as its `basis` rung.
export MTX_RUNG=basis
exec "$(dirname "$0")/../../../reference/bin/bot"

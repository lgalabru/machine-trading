#!/usr/bin/env bash
# A ladder rung (see bench/checks/ladder/README.md): the reference, bots/carry, run as its
# `told` rung.
export MTX_RUNG=told
exec "$(dirname "$0")/../../../reference/bin/bot"

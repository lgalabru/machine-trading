#!/usr/bin/env bash
# A ladder rung (see bench/checks/ladder/README.md): the flash-crash reference (reference/),
# run as the task's informed bot, scoring v2's 1.
export MTX_RUNG=informed
exec "$(dirname "$0")/../../../reference/bin/bot"

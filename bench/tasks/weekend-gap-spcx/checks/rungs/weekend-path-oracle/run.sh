#!/usr/bin/env bash
# A ladder rung (see bench/checks/ladder/README.md): the weekend-gap reference (reference/),
# run as its `path-oracle` rung.
export MTX_RUNG=path-oracle
exec "$(dirname "$0")/../../../reference/bin/bot"

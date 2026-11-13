#!/usr/bin/env bash
# A ladder rung (see bench/checks/ladder/README.md): built into ../bin by `just ladder`.
exec "$(dirname "$0")/../../../../../checks/ladder/bin/quoter" naive

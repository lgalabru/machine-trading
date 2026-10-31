#!/usr/bin/env bash
# A ladder rung (see ../README.md): built into ../bin by `just ladder`.
exec "$(dirname "$0")/../bin/quoter" skew

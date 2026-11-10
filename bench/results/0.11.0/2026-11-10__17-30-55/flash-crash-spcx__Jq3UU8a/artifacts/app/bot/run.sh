#!/usr/bin/env bash
# Entry point the grader runs. It must start the bot without building or
# downloading anything: the grader has no network.
d="$(dirname "$0")"
if [ -f "$d/params.env" ]; then set -a; . "$d/params.env"; set +a; fi
exec "$d/bin/bot"

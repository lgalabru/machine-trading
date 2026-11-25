#!/usr/bin/env bash
# Entry point the grader runs. It must start the bot without building or
# downloading anything: the grader has no network.
exec "$(dirname "$0")/bin/bot"

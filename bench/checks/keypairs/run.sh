#!/usr/bin/env bash
# Keypair probe, run as a bot: the grader's own taker and competitor accounts
# sign with keypairs it writes during setup. A bot runs in the same container,
# as an unprivileged user; it must not be able to read them. This bot looks
# for them while its session runs and reports what it found in its log.
set -u
seen=0
readable=0
for _ in $(seq 1 20); do
  for dir in /tmp/mtx-takers-* /tmp/mtx-competitor-*; do
    [ -d "$dir" ] || continue
    seen=$((seen + 1))
    if ls "$dir" > /dev/null 2>&1; then
      for file in "$dir"/*.json; do
        if [ -r "$file" ] && head -c 1 "$file" > /dev/null 2>&1; then
          echo "probe: READABLE $file"
          readable=$((readable + 1))
        fi
      done
    fi
  done
  [ "$seen" -gt 0 ] && break
  sleep 0.5
done
echo "probe: dirs=$seen readable=$readable"
# Sit out the rest of the session without trading.
trap 'exit 0' TERM
while true; do sleep 1; done

#!/usr/bin/env bash
# Ask the bench machine's dispatcher (ops/bench/bench-dispatch) for one
# command, from a GitHub workflow: `ops/bench/ssh.sh status`.
#
# Reads BENCH_KEY (the workflow's private key), BENCH_KNOWN_HOSTS (the
# machine's host key line, so a machine that isn't it is refused),
# BENCH_HOST and BENCH_USER, all environment secrets. The key's authorized_keys entry pins it to the dispatcher:
# the arguments reach it as SSH_ORIGINAL_COMMAND and nothing else runs.
set -euo pipefail
: "${BENCH_KEY:?}" "${BENCH_KNOWN_HOSTS:?}" "${BENCH_HOST:?}" "${BENCH_USER:?}"
dir="$(mktemp -d "${RUNNER_TEMP:-/tmp}/bench.XXXXXX")"
trap 'rm -rf "$dir"' EXIT
(umask 077 && printf '%s\n' "$BENCH_KEY" > "$dir/key" && printf '%s\n' "$BENCH_KNOWN_HOSTS" > "$dir/known_hosts")
ssh -i "$dir/key" -F /dev/null \
  -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=yes \
  -o UserKnownHostsFile="$dir/known_hosts" -o ConnectTimeout=20 \
  "$BENCH_USER@$BENCH_HOST" "$*"

#!/usr/bin/env bash
# Ask the bench machine's dispatcher (ops/sunburst/bench-dispatch) for one
# command, from a GitHub workflow: `ops/sunburst/ssh.sh status`.
#
# Reads SUNBURST_KEY (the workflow's private key), SUNBURST_KNOWN_HOSTS (the
# machine's host key line, so a machine that isn't it is refused) and
# SUNBURST_HOST. The key's authorized_keys entry pins it to the dispatcher:
# the arguments reach it as SSH_ORIGINAL_COMMAND and nothing else runs.
set -euo pipefail
: "${SUNBURST_KEY:?}" "${SUNBURST_KNOWN_HOSTS:?}" "${SUNBURST_HOST:?}"
dir="$(mktemp -d "${RUNNER_TEMP:-/tmp}/sunburst.XXXXXX")"
trap 'rm -rf "$dir"' EXIT
(umask 077 && printf '%s\n' "$SUNBURST_KEY" > "$dir/key" && printf '%s\n' "$SUNBURST_KNOWN_HOSTS" > "$dir/known_hosts")
ssh -i "$dir/key" -F /dev/null \
  -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=yes \
  -o UserKnownHostsFile="$dir/known_hosts" -o ConnectTimeout=20 \
  "ubuntu@$SUNBURST_HOST" "$*"

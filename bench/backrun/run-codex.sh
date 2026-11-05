#!/usr/bin/env bash
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd)
release_args=()
if [[ -n ${1:-} ]]; then release_args=(--release "$1"); fi
exec "$repo/target/debug/mtx" evaluate --task atomic-arb \
  ${release_args[@]+"${release_args[@]}"} \
  --agent codex --model openai/gpt-6.1-terra --effort high \
  --jobs-dir "$repo/work/backrun-codex-terra/jobs" \
  --job-name "${2:-terra-continuous-$(date +%Y%m%d-%H%M%S)}"

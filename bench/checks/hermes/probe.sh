#!/usr/bin/env bash
# The Hermes harness check, with no model: two trials of a task, one after
# the other, through bench/harness/mtx_hermes.py, against a stand-in
# OpenAI-compatible provider (mock_provider.py) on this machine. The
# provider's script lists what Hermes's home holds, plants a skill, a
# memory and probe files, then writes and reads a file. It passes when the
# install, the provider route, the tool loop, the effort, the trajectory
# and the token counts all work, the second trial sees nothing the first
# planted, and the provider's key appears in no file the trials left.
# `route` is the way models.toml reaches a provider: custom (base_url and
# api_format: Gemini, any OpenAI-compatible host), openrouter or openai.
#   bench/checks/hermes/probe.sh bench/tasks/<task> [hermes version] [jobs dir] [route]
set -euo pipefail
task="${1:?usage: probe.sh bench/tasks/<task> [hermes version] [jobs dir] [route]}"
version="${2:-}"
route="${4:-custom}"
here="$(cd "$(dirname "$0")" && pwd)"
if [ -n "${3:-}" ]; then jobs="$3"; mkdir -p "$jobs"; else jobs="$(mktemp -d)"; fi
# The trials reach the provider through the host's address, not loopback.
host="$(hostname -I | awk '{print $1}')"
port="$(python3 -I -c 'import socket; s = socket.socket(); s.bind(("", 0)); print(s.getsockname()[1])')"
MOCK_API_KEY="mock-$(python3 -I -c 'import secrets; print(secrets.token_hex(24))')"
export MOCK_API_KEY
python3 -I "$here/mock_provider.py" --host "$host" --port "$port" --log "$jobs/requests.jsonl" &
mock=$!
trap 'kill "$mock" 2> /dev/null || true' EXIT
sleep 1
case "$route" in
  custom) model=(-m custom/mock-trader --api-format openai_chat_completions) ;;
  # Real names, so Hermes treats the stand-in as it would the model.
  openrouter) model=(-m openrouter/moonshotai/kimi-k3) ;;
  openai) model=(-m openai/gpt-6.1-sol) ;;
  *) echo "route: custom, openrouter or openai" >&2; exit 2 ;;
esac
PYTHONPATH="$here/../../harness${PYTHONPATH:+:$PYTHONPATH}" \
  harbor run -y -p "$task" -o "$jobs" -a mtx_hermes:Hermes "${model[@]}" \
  --base-url "http://$host:$port/v1" --api-key-name MOCK_API_KEY \
  --effort high ${version:+--ak "version=$version"} -k 2 -n 1 --disable-verification > "$jobs/harbor.log" 2>&1 \
  || { tail -20 "$jobs/harbor.log" >&2; exit 2; }
fails=0
if grep -rlF "$MOCK_API_KEY" "$jobs"; then
  echo "hermes probe: FAIL, the provider's key is in the files above"; fails=1
fi
python3 -I "$here/read.py" "$jobs" || fails=1
exit "$fails"

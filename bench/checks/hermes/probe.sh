#!/usr/bin/env bash
# The Hermes harness check, with no model: two trials of a task, one after
# the other, through bench/harness/mtx_hermes.py and the bench's key proxy
# (bench/harness/key_proxy.py), against a stand-in provider
# (mock_provider.py) on this machine. The provider's script lists what
# Hermes's home holds, copies every process environment the agent can read
# under /proc into the trial's logs, plants a skill, a memory and probe
# files, then writes and reads a file. It passes when the install, the
# route, the tools, the effort, the trajectory and the token counts all
# work, the second trial sees nothing the first planted, and the
# provider's key is in no file the trials left, the /proc copies included
# (which do hold the proxy's token, as a control).
# `route` is how models.toml reaches a provider: openai (Responses API),
# anthropic (Messages API), openrouter (chat completions, with the
# proxy's effort rewrite) or chat (any OpenAI-compatible host, as Gemini).
#   bench/checks/hermes/probe.sh bench/tasks/<task> <hermes version> <route> [jobs dir]
set -euo pipefail
task="${1:?usage: probe.sh bench/tasks/<task> <hermes version> <route> [jobs dir]}"
version="${2:?hermes version}"
route="${3:?route: openai, anthropic, openrouter or chat}"
here="$(cd "$(dirname "$0")" && pwd)"
harness="$(cd "$here/../../harness" && pwd)"
if [ -n "${4:-}" ]; then jobs="$4"; mkdir -p "$jobs"; temporary=""; else jobs="$(mktemp -d)"; temporary="$jobs"; fi
case "$route" in
  openai) entry='api = "openai"
model = "gpt-6.1-sol"
upstream = "http://127.0.0.1:PORT/v1"' ;;
  anthropic) entry='api = "anthropic"
model = "claude-opus-5-5"
upstream = "http://127.0.0.1:PORT"' ;;
  openrouter) entry='api = "chat"
model = "moonshotai/kimi-k3"
reasoning = "openrouter"
upstream = "http://127.0.0.1:PORT/api/v1"' ;;
  chat) entry='api = "chat"
model = "gemini-3.1-pro-preview"
upstream = "http://127.0.0.1:PORT/v1beta/openai"' ;;
  *) echo "route: openai, anthropic, openrouter or chat" >&2; exit 2 ;;
esac
# The stand-in provider, on loopback: only the proxy talks to it.
port="$(python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
MOCK_API_KEY="mock-$(python3 -I -c 'import secrets; print(secrets.token_hex(24))')"
MTX_PROXY_SECRET="$(python3 -I -c 'import secrets; print(secrets.token_hex(32))')"
export MOCK_API_KEY MTX_PROXY_SECRET
registry="$jobs/models.toml"
printf '[probe]\n%s\nkey = "MOCK_API_KEY"\n' "${entry//PORT/$port}" > "$registry"
python3 -I "$here/mock_provider.py" --host 127.0.0.1 --port "$port" --log "$jobs/requests.jsonl" &
mock=$!
# The proxy, on the docker bridge the trials reach the host by, as
# `just bench` runs it.
host="$(ip -4 -o addr show docker0 | awk '{print $4}' | cut -d/ -f1)"
MTX_MODELS="$registry" python3 -I "$harness/key_proxy.py" --host "$host" --port 0 probe > "$jobs/proxy.url" 2> "$jobs/proxy.log" &
proxy=$!
trap 'kill "$mock" "$proxy" 2> /dev/null || true; [ -z "$temporary" ] || rm -rf -- "$temporary"' EXIT
for _ in $(seq 50); do [ -s "$jobs/proxy.url" ] && break; sleep 0.2; done
mapfile -t args < <(MTX_MODELS="$registry" python3 -I "$harness/models.py" args probe "$(cat "$jobs/proxy.url")")
MTX_PROXY_TOKEN="$(MTX_MODELS="$registry" python3 -I "$harness/models.py" token probe)"
export MTX_PROXY_TOKEN
# Harbor never holds the key or the batch's secret, only the route's token.
env -u MOCK_API_KEY -u MTX_PROXY_SECRET PYTHONPATH="$harness${PYTHONPATH:+:$PYTHONPATH}" \
  harbor run -y -p "$task" -o "$jobs" -a mtx_hermes:Hermes "${args[@]}" \
  --effort high --ak "version=$version" -k 2 -n 1 --disable-verification > "$jobs/harbor.log" 2>&1 \
  || { tail -20 "$jobs/harbor.log" >&2; exit 2; }
fails=0
# The key on stdin, not the command line.
if grep -rlF -f <(printf '%s\n' "$MOCK_API_KEY") "$jobs"; then
  echo "hermes probe: FAIL, the provider's key is in the files above"; fails=1
fi
python3 -I "$here/read.py" "$jobs" "$route" || fails=1
# What Hermes asked the proxy for beyond its model calls, all refused.
if [ -s "$jobs/proxy.log" ]; then echo "proxy refused:"; sort "$jobs/proxy.log" | uniq -c; fi
exit "$fails"

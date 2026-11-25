#!/usr/bin/env bash
# Make a fresh Ubuntu machine a bench machine, as mtx-bench-1 (sunburst) is:
# the same tools at the same versions, the same Docker networks, the repo,
# the dispatcher. Run it on the machine as its admin user (who has sudo);
# it is safe to run again.
#
#   scp ops/bench/provision.sh <machine>: && ssh <machine> bash provision.sh
#
# It doesn't copy the API keys: it creates ~/.machine-trading.env with their
# names, and the operator fills them (from the other machine, with scp, or
# from 1Password). Nor does it install GitHub's dispatcher keys: see
# docs/development.md, "Automation". Check the result with
# `bash provision.sh --check`.
set -euo pipefail

# The versions mtx-bench-1 runs (2026-11-25). A batch that spans machines
# must run the same ones, so a result doesn't depend on where it ran.
UV_VERSION=0.13.7
JUST_VERSION=1.59.0
HARBOR_VERSION=0.25.0
HF_VERSION=2.4.0
REPO=https://github.com/lgalabru/machine-trading.git
# The trials' compose networks come from this pool; the key proxy listens on
# docker0, the pool's first address (10.200.0.1). Docker's default pools run
# out at a few dozen trials at once.
DAEMON_JSON='{"default-address-pools":[{"base":"10.200.0.0/14","size":24}]}'
KEYS=(ANTHROPIC_API_KEY HF_TOKEN OPENAI_API_KEY HERMES_API_KEY)

export PATH="$HOME/.local/bin:$PATH"

check() {
  local ok=0
  have() { if "$@" > /dev/null 2>&1; then echo "ok    $*"; else echo "FAIL  $*"; ok=1; fi; }
  have docker info
  have docker compose version
  have docker buildx version
  have test "$(cat /etc/docker/daemon.json 2>/dev/null)" = "$DAEMON_JSON"
  have test "$(ip -4 -o addr show docker0 | awk '{print $4}' | cut -d/ -f1)" = 10.200.0.1
  have test "$(uv --version | awk '{print $2}')" = "$UV_VERSION"
  have test "$(just --version | awk '{print $2}')" = "$JUST_VERSION"
  have test "$(harbor --version)" = "$HARBOR_VERSION"
  have hf version
  have test -d "$HOME/machine-trading/.git"
  have test -x "$HOME/bin/bench-dispatch"
  have test "$(stat -c %a "$HOME/.machine-trading.env")" = 600
  for key in "${KEYS[@]}"; do
    have grep -qE "^$key=.+" "$HOME/.machine-trading.env"
  done
  return "$ok"
}

if [ "${1:-}" = --check ]; then
  check
  exit
fi

sudo apt-get update -qq
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
  docker.io docker-compose-v2 docker-buildx git jq curl rsync python3 ca-certificates
if [ "$(cat /etc/docker/daemon.json 2>/dev/null)" != "$DAEMON_JSON" ]; then
  echo "$DAEMON_JSON" | sudo tee /etc/docker/daemon.json > /dev/null
  sudo systemctl restart docker
fi
sudo usermod -aG docker "$USER"

if [ "$(uv --version 2>/dev/null | awk '{print $2}')" != "$UV_VERSION" ]; then
  curl -LsSf "https://astral.sh/uv/$UV_VERSION/install.sh" | env UV_NO_MODIFY_PATH=1 sh
fi
if [ "$(just --version 2>/dev/null | awk '{print $2}')" != "$JUST_VERSION" ]; then
  mkdir -p "$HOME/.local/bin"
  curl --proto '=https' --tlsv1.2 -sSf https://just.systems/install.sh \
    | bash -s -- --tag "$JUST_VERSION" --to "$HOME/.local/bin" --force
fi
uv tool install --quiet "harbor==$HARBOR_VERSION"
uv tool install --quiet "huggingface-hub==$HF_VERSION"

[ -d "$HOME/machine-trading/.git" ] || git clone -q "$REPO" "$HOME/machine-trading"
mkdir -p "$HOME/bin" "$HOME/logs" "$HOME/discarded"
# A copy, not the checkout's file: a commit on main can't widen what a
# GitHub key may run (docs/development.md).
install -m 755 "$HOME/machine-trading/ops/bench/bench-dispatch" "$HOME/bin/bench-dispatch"

if [ ! -e "$HOME/.machine-trading.env" ]; then
  (umask 077 && printf '%s=\n' "${KEYS[@]}" > "$HOME/.machine-trading.env")
fi
chmod 600 "$HOME/.machine-trading.env"

echo
echo "Done. Fill the keys in ~/.machine-trading.env, log in again (docker group), then:"
echo "  bash provision.sh --check"

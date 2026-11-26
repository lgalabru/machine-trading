#!/usr/bin/env bash
# What CI's Rust jobs need on top of the Rust image, as root: the system
# libraries the Solana crates link, Node (the site build and the viewer
# check), Python, `just`, clippy, nightly rustfmt (the repo's rustfmt.toml
# uses nightly options), the wasm32-wasip2 target, and the bot users
# `mtx grade` runs bots as (session::BOT_USERS).
#
# The Dockerfile bakes it into the CI image; a job whose image isn't
# published runs it on the plain Rust image instead (see ci-image.yml).
set -euo pipefail

apt-get update
apt-get install -y --no-install-recommends libudev-dev clang libclang-dev jq python3 python3-venv curl ca-certificates git
if ! command -v node > /dev/null; then
  curl -fsSL https://deb.nodesource.com/setup_24.x | bash -
  apt-get install -y --no-install-recommends nodejs
fi
rm -rf /var/lib/apt/lists/*

rustup component add clippy rustfmt
rustup target add wasm32-wasip2
rustup toolchain install nightly --profile minimal --component rustfmt

command -v just > /dev/null || cargo install --locked --root /usr/local just@1.49.0

for user in mtx-bot mtx-bot-2 mtx-bot-3 mtx-bot-4 mtx-bot-5 mtx-bot-6 mtx-bot-7 mtx-bot-8 mtx-bot-9 mtx-bot-10 mtx-bot-11 mtx-bot-12 mtx-bot-13 mtx-bot-14 mtx-bot-15 mtx-bot-16; do
  id "$user" > /dev/null 2>&1 || useradd --system --create-home --shell /usr/sbin/nologin "$user"
done

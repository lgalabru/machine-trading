#!/usr/bin/env bash
# Install Agave as `sol`, at the version the plugin is built for, and the
# node's identity (unstaked: it doesn't vote). Idempotent.
#
# Anza's installer gives the CLI (`solana`, `solana-keygen`) but no longer
# `agave-validator`, so the validator is built from the release's source
# with its own toolchain, and linked into the installer's directory, which
# is sol.service's PATH.
#
#   ./install-agave.sh            # v4.4.1, the latest stable on 2026-11-20
set -euo pipefail

# The release; its rust-toolchain.toml's Rust; and the interface the plugin
# is built against for it (see build-plugin.sh).
AGAVE_VERSION="${AGAVE_VERSION:-v4.4.1}"
RUST_VERSION="${RUST_VERSION:-1.98.1}"

[ "$(id -un)" = sol ] || { echo "run as sol" >&2; exit 1; }
sh -c "$(curl -sSfL "https://release.anza.xyz/${AGAVE_VERSION}/install")"
bin="$HOME/.local/share/solana/install/active_release/bin"

if ! command -v rustup >/dev/null && [ ! -x "$HOME/.cargo/bin/rustup" ]; then
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
fi
. "$HOME/.cargo/env"
rustup toolchain install "$RUST_VERSION" --profile minimal

version="${AGAVE_VERSION#v}"
build="$HOME/agave-build"
mkdir -p "$build"
if [ ! -d "$build/agave-$version" ]; then
  curl -sSfL "https://github.com/anza-xyz/agave/archive/refs/tags/${AGAVE_VERSION}.tar.gz" \
    | tar -xz -C "$build"
fi
out="$HOME/agave-${AGAVE_VERSION}"
(cd "$build/agave-$version" && ./scripts/cargo-install-all.sh --validator-only "$out")
for binary in "$out"/bin/*; do
  ln -sf "$binary" "$bin/$(basename "$binary")"
done
"$bin/agave-validator" --version
"$bin/solana" config set --url http://127.0.0.1:8899 >/dev/null

if [ ! -f "$HOME/validator-keypair.json" ]; then
  "$bin/solana-keygen" new --no-bip39-passphrase --silent -o "$HOME/validator-keypair.json"
fi
echo "identity: $("$bin/solana-keygen" pubkey "$HOME/validator-keypair.json")"

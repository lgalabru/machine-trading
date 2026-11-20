#!/usr/bin/env bash
# Install Agave as `sol`, at the version the plugin is built for, and the
# node's identity (unstaked: it doesn't vote). Idempotent.
#
#   ./install-agave.sh            # v4.4.1, the latest stable on 2026-11-20
set -euo pipefail

# The release, and the interface the plugin must be built against for it
# (agave/Cargo.toml's agave-geyser-plugin-interface): see build-plugin.sh.
AGAVE_VERSION="${AGAVE_VERSION:-v4.4.1}"

[ "$(id -un)" = sol ] || { echo "run as sol" >&2; exit 1; }
sh -c "$(curl -sSfL "https://release.anza.xyz/${AGAVE_VERSION}/install")"
export PATH="$HOME/.local/share/solana/install/active_release/bin:$PATH"
agave-validator --version
solana config set --url http://127.0.0.1:8899 >/dev/null

if [ ! -f "$HOME/validator-keypair.json" ]; then
  solana-keygen new --no-bip39-passphrase --silent -o "$HOME/validator-keypair.json"
fi
echo "identity: $(solana-keygen pubkey "$HOME/validator-keypair.json")"

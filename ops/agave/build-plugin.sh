#!/usr/bin/env bash
# Build the mtx geyser plugin as `sol`, for the Agave the node runs: the
# plugin's interface version and Rust toolchain must be the validator's,
# since a trait object crosses between them with no stable ABI.
#
# Agave v4.4.1 builds with rustc 1.98.1 (its rust-toolchain.toml) against
# agave-geyser-plugin-interface 5.0.0: the plugin's `agave-4-4` feature.
#
# The source is copied to ~/mtx/src from a checkout, since the repository is
# private: see README.md's "Deploy the plugin".
#
#   ./build-plugin.sh
set -euo pipefail

RUST_VERSION="1.98.1"

[ "$(id -un)" = sol ] || { echo "run as sol" >&2; exit 1; }
if ! command -v rustup >/dev/null && [ ! -x "$HOME/.cargo/bin/rustup" ]; then
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
fi
. "$HOME/.cargo/env"
rustup toolchain install "$RUST_VERSION" --profile minimal

src="$HOME/mtx/src"
[ -f "$src/Cargo.toml" ] || { echo "no source in $src: copy a checkout there first" >&2; exit 1; }
cargo "+$RUST_VERSION" build --release --manifest-path "$src/Cargo.toml" \
  -p mtx-geyser --no-default-features --features agave-4-4
install -m 0755 "$src/target/release/libmtx_geyser.so" "$HOME/mtx/libmtx_geyser.so"
echo "plugin: $HOME/mtx/libmtx_geyser.so ($(cat "$src/.commit" 2>/dev/null || echo "unknown commit"))"

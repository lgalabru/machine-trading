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
# Each build under a name of its own, and the configuration pointed at it:
# a reload (reload-plugin.sh) must load a file the process hasn't loaded,
# since the old library can stay mapped after its unload (glibc keeps a
# library whose threads registered thread-local destructors), and loading
# the same path again would hand back the old code. The validator's next
# start loads it too.
commit="$(cat "$src/.commit" 2>/dev/null || date +%Y%m%d%H%M%S)"
plugins="$HOME/mtx/plugins"
mkdir -p "$plugins"
lib="$plugins/libmtx_geyser-$commit-$(date +%s).so"
install -m 0755 "$src/target/release/libmtx_geyser.so" "$lib"
config="$HOME/mtx/geyser.json"
python3 -I - "$config" "$lib" <<'PY'
import json, os, sys
path, lib = sys.argv[1], sys.argv[2]
with open(path) as f:
    config = json.load(f)
config["libpath"] = lib
with open(path + ".new", "w") as f:
    json.dump(config, f, indent=2)
os.replace(path + ".new", path)
PY
# The last five builds are kept; older ones go.
ls -1t "$plugins"/libmtx_geyser-*.so | tail -n +6 | xargs -r rm -f
echo "plugin: $lib ($commit), in $config"

#!/usr/bin/env bash
# Build actual upstream AMMs without modifying their source. Setup keys stay local.
set -euo pipefail
umask 077
repo=$(cd "$(dirname "$0")/../.." && pwd)
root=${1:-"$repo/work/backrun"}
mkdir -p "$root/programs" "$root/sources" "$root/private"
command -v cargo-build-sbf >/dev/null
cargo-build-sbf --version | head -n 1 | grep -F "4.4.0" >/dev/null
command -v solana-keygen >/dev/null
checkout() {
  local name=$1 url=$2 revision=$3
  if [ ! -d "$root/sources/$name/.git" ]; then
    git init -q "$root/sources/$name"
    git -C "$root/sources/$name" remote add origin "$url"
  fi
  git -C "$root/sources/$name" fetch --depth 1 origin "$revision"
  git -C "$root/sources/$name" checkout --detach "$revision"
  test "$(git -C "$root/sources/$name" rev-parse HEAD)" = "$revision"
}
checkout raydium https://github.com/raydium-io/raydium-cp-swap 3d69122790e97cd6aff24e8ba95940793773f58c
checkout orca https://github.com/orca-so/whirlpools cc683ec92acb5e7606caa3d8e514d9383e9c7c11
if [ ! -f "$root/private/cpmm-admin.json" ]; then
  solana-keygen new --silent --no-bip39-passphrase --outfile "$root/private/cpmm-admin.json"
fi
cp "$root/sources/orca/programs/whirlpool/src/auth/localnet/localnet-admin-keypair-0.json" "$root/private/orca-admin.json"
export CPSWAP_LOCALNET_ADMIN
CPSWAP_LOCALNET_ADMIN=$(solana-keygen pubkey "$root/private/cpmm-admin.json")
(cd "$root/sources/raydium" && cargo-build-sbf --tools-version v1.57 --manifest-path programs/cp-swap/Cargo.toml --sbf-out-dir "$root/programs" -- --locked --features localnet)
(cd "$root/sources/orca" && cargo-build-sbf --tools-version v1.57 --manifest-path programs/whirlpool/Cargo.toml --sbf-out-dir "$root/programs" -- --locked)
cargo-build-sbf --tools-version v1.57 --manifest-path "$repo/programs/backrun-executor/Cargo.toml" --sbf-out-dir "$root/programs" -- --locked
python3 - "$root" <<'PY'
import hashlib,json,pathlib,sys,subprocess
root=pathlib.Path(sys.argv[1])
files={p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root/'programs').glob('*.so'))}
manifest={'version':1,'sbf_toolchain':{'cargo_build_sbf':'4.4.0','platform_tools':'v1.57'},'raydium_revision':'3d69122790e97cd6aff24e8ba95940793773f58c','orca_revision':'cc683ec92acb5e7606caa3d8e514d9383e9c7c11','surfpool_revision':'64d531fb264bb976f1569f509362a7dfa30b4c26','cpmm_localnet_admin':subprocess.check_output(['solana-keygen','pubkey',str(root/'private/cpmm-admin.json')],text=True).strip(),'files':files}
(root/'programs/manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
PY

#!/usr/bin/env bash
# One-time root setup of a mainnet-beta RPC node (non-voting) that runs the
# mtx geyser plugin (docs/rfd/0004, steps 6 and 7). Idempotent: running it
# again changes nothing that is already right.
#
# After Anza's guide (https://docs.anza.xyz/operations/setup-an-rpc-node,
# .../setup-a-validator, .../guides/validator-start). Run as root on the
# node, from this directory:
#
#   sudo ./setup-root.sh
#
# Then, as `sol`: ./install-agave.sh, ./build-plugin.sh, and
# `sudo systemctl enable --now sol`.
set -euo pipefail

# The ports gossip, turbine and repair use: --dynamic-port-range in
# validator.sh. JSON-RPC stays on localhost (--private-rpc and
# --rpc-bind-address 127.0.0.1), so it isn't opened.
DYNAMIC_PORTS="8000:8020"

[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }
here="$(cd "$(dirname "$0")" && pwd)"

# Build dependencies: Agave's, and the plugin's (Rust, C, protobuf).
apt-get update -qq
apt-get install -y -qq build-essential pkg-config libudev-dev llvm libclang-dev \
  protobuf-compiler clang git curl ufw

# Kernel limits, as the guide sets them.
cat >/etc/sysctl.d/21-agave-validator.conf <<'EOF'
# Increase max UDP buffer sizes
net.core.rmem_max = 134217728
net.core.wmem_max = 134217728
# Increase memory mapped files limit
vm.max_map_count = 1000000
# Increase number of allowed open file descriptors
fs.nr_open = 1000000
EOF
sysctl -q -p /etc/sysctl.d/21-agave-validator.conf

cat >/etc/security/limits.d/90-solana-nofiles.conf <<'EOF'
# Increase process file descriptor count limit
* - nofile 1000000
# Increase memory locked limit (kB)
* - memlock 2000000
EOF

# The node runs as `sol`, with no login password.
id sol >/dev/null 2>&1 || adduser --disabled-password --gecos "" sol

# The guide wants accounts and ledger on separate disks. This server has one
# NVMe, so they are separate directories on it: enough for a node that
# doesn't vote, and the first thing to change if it can't keep up.
for dir in /mnt/ledger /mnt/accounts /mnt/snapshots; do
  mkdir -p "$dir"
  chown sol:sol "$dir"
done

# The CPU at its full clock: replay is single-thread bound.
if compgen -G "/sys/devices/system/cpu/cpu*/cpufreq/scaling_governor" >/dev/null; then
  echo performance | tee /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor >/dev/null
fi

# The node's files: its launch script, the plugin's configuration, and the
# mtx runtime's state directory.
install -d -o sol -g sol /home/sol/bin /home/sol/mtx /home/sol/mtx/logs
install -o sol -g sol -m 0755 "$here/validator.sh" /home/sol/bin/validator.sh
install -o sol -g sol -m 0755 "$here/install-agave.sh" /home/sol/bin/install-agave.sh
install -o sol -g sol -m 0755 "$here/build-plugin.sh" /home/sol/bin/build-plugin.sh
[ -f /home/sol/mtx/geyser.json ] || install -o sol -g sol -m 0644 "$here/geyser.json" /home/sol/mtx/geyser.json
# The plugin's source, if a checkout was copied next to this directory
# (README.md's "Deploy the plugin"): `sol` builds it.
if [ -f "$here/../mtx-src/Cargo.toml" ]; then
  rm -rf /home/sol/mtx/src
  cp -a "$here/../mtx-src" /home/sol/mtx/src
  chown -R sol:sol /home/sol/mtx/src
fi

# systemd, as the guide writes the unit.
install -m 0644 "$here/sol.service" /etc/systemd/system/sol.service
cat >/etc/logrotate.d/sol <<'EOF'
/home/sol/agave-validator.log {
  rotate 7
  daily
  missingok
  postrotate
    systemctl kill -s USR1 sol.service
  endscript
}
EOF
systemctl daemon-reload
systemctl restart logrotate.service || true

# The firewall: SSH, and the node's peer-to-peer ports. Nothing else in.
ufw allow 22/tcp >/dev/null
ufw allow "$DYNAMIC_PORTS/tcp" >/dev/null
ufw allow "$DYNAMIC_PORTS/udp" >/dev/null
ufw --force enable >/dev/null
ufw status verbose

echo "root setup done: next, as sol, install-agave.sh and build-plugin.sh"

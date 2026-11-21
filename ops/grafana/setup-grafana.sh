#!/usr/bin/env bash
# Installs Docker and the mtx-grafana unit (otel-lgtm on localhost) on the
# node. Run once, as root, after ops/agave/setup-root.sh copied the source
# to /home/sol/mtx/src:
#
#   sudo bash /home/sol/mtx/src/ops/grafana/setup-grafana.sh
#
# Then, from your machine: ssh -N -L 3000:127.0.0.1:3000 ubuntu@<node>
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "run as root" >&2
  exit 1
fi

SRC=/home/sol/mtx/src/ops/grafana
test -f "$SRC/docker-compose.yml" || { echo "$SRC/docker-compose.yml is missing" >&2; exit 1; }

apt-get update -q
apt-get install -y -q docker.io docker-compose-v2
systemctl enable --now docker

# The stack writes as its own users; the directory is the container's /data.
install -d -m 0777 /var/lib/mtx-grafana

install -m 0644 "$SRC/mtx-grafana.service" /etc/systemd/system/mtx-grafana.service
systemctl daemon-reload
systemctl enable --now mtx-grafana

# Docker publishes ports past ufw's rules; these are bound to 127.0.0.1, so
# nothing is open to the internet. Check:
ss -ltnp | grep -E ':(3000|4317|4318)\b' || true
echo "mtx-grafana is up: tunnel port 3000 to reach it"

#!/usr/bin/env bash
# Build the mtx plugin and swap it into the running validator, as `sol`,
# with no restart (`agave-validator plugin reload`): the old runtime stops
# (its strategies' last steps finish, every socket connection is closed,
# every thread joined), and the new one starts on the same sockets and
# restores the strategies from the desired state.
#
# Sidecars see their connections close and connect again. The cache starts
# empty and is backfilled from the node's RPC, so strategies step again
# within seconds.
#
#   ./reload-plugin.sh            # after copying the new source to ~/mtx/src
set -euo pipefail

LEDGER="${LEDGER:-/mnt/ledger}"
AGAVE="${AGAVE:-$HOME/.local/share/solana/install/active_release/bin/agave-validator}"
NAME="mtx-geyser"
CONTROL="${CONTROL:-$HOME/mtx/control.sock}"

[ "$(id -un)" = sol ] || { echo "run as sol" >&2; exit 1; }
"$(dirname "$0")/build-plugin.sh"
"$AGAVE" --ledger "$LEDGER" plugin reload "$NAME" "$HOME/mtx/geyser.json"
"$AGAVE" --ledger "$LEDGER" plugin list
# The new runtime answers on its control socket once it has started.
for _ in $(seq 1 50); do
  if python3 -I - "$CONTROL" <<'PY'
import json, socket, sys
s = socket.socket(socket.AF_UNIX)
s.settimeout(2)
s.connect(sys.argv[1])
s.sendall(b'{"command":"status"}\n')
reply = json.loads(s.makefile().readline())
assert reply["reply"] == "ok", reply
print("runtime:", ", ".join(f'{x["name"]} {x["state"]}' for x in reply["strategies"]) or "no strategies")
PY
  then exit 0; fi
  sleep 0.2
done
echo "the new runtime doesn't answer on $CONTROL" >&2
exit 1

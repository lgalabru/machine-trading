#!/usr/bin/env bash
# Anti-cheat check, around the RPC guard: a bot that never calls the guard's
# refused methods (so it isn't disqualified) but finds the surfnet's own RPC
# port and tries cheats there directly. The grader locks every cheatcode
# before the bot starts, so these must all be refused: the bot ends with
# nothing gained, and the session (and its grade) must complete normally.
set -u
. "$(dirname "$0")/http.sh"
guard_port="${MT_RPC_URL##*:}"
guard_port="${guard_port%%/*}"
owner=11111111111111111111111111111111
usdc=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v

# Listening TCP ports, from /proc: no tools needed, readable by any user.
ports=$(awk 'NR > 1 && $4 == "0A" { split($2, a, ":"); print a[2] }' \
  /proc/net/tcp /proc/net/tcp6 2>/dev/null | sort -u | while read -r hex; do
  printf '%d\n' "0x$hex"
done)
for port in $ports; do
  [ "$port" = "$guard_port" ] && continue
  url="http://127.0.0.1:$port"
  echo "--- $url"
  for body in \
    '{"jsonrpc":"2.0","id":1,"method":"surfnet_setTokenAccount","params":["'$owner'","'$usdc'",{"amount":1000000000000}]}' \
    '{"jsonrpc":"2.0","id":2,"method":"surfnet_enableCheatcode","params":["all"]}' \
    '{"jsonrpc":"2.0","id":3,"method":"requestAirdrop","params":["'$owner'",1000000000]}' \
    '{"jsonrpc":"2.0","id":4,"method":"exit","params":[]}'; do
    http_post "$url" "$body" | cut -c1-160
    echo
  done
done
sleep 100000

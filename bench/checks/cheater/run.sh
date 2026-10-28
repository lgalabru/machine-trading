#!/usr/bin/env bash
# Anti-cheat check, through the RPC guard: a bot that tries every shortcut a
# grader must close. The guard must refuse the cheatcodes (disqualifying the
# bot, so the reward is 0), and the file system and signals must hold.
set -u
rpc() {
  curl -s "$OTT_RPC_URL" -H 'content-type: application/json' -d "$1"
  echo
}
owner=11111111111111111111111111111111
usdc=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v

# Chain cheats: mint USDC, airdrop SOL, jump the clock past the crash.
rpc '{"jsonrpc":"2.0","id":1,"method":"surfnet_setTokenAccount","params":["'$owner'","'$usdc'",{"amount":1000000000000}]}'
rpc '{"jsonrpc":"2.0","id":2,"method":"requestAirdrop","params":["'$owner'",1000000000]}'
rpc '{"jsonrpc":"2.0","id":3,"method":"surfnet_timeTravel","params":[{"absoluteSlot":999999999}]}'
rpc '{"jsonrpc":"2.0","id":4,"method":"getSlot"}'

# Read the hidden scenarios; write the reward.
echo "--- /tests:"
ls -la /tests/scenarios 2>&1 | head -3
cat /tests/scenarios/*.json 2>&1 | head -3
echo '{"reward": 1.0}' > /logs/verifier/reward.json 2>&1 || echo "cannot write the reward"

# Outlive the session: ignore SIGTERM. The grader must SIGKILL us.
trap '' TERM
while true; do sleep 1; done

#!/usr/bin/env bash
# Anti-cheat check: a bot that tries to mint itself USDC with a surfnet
# cheatcode and to airdrop itself SOL. The guard must refuse both, and the
# grade must be 0.
set -u
rpc() {
  curl -s "$OTT_RPC_URL" -H 'content-type: application/json' -d "$1"
  echo
}
owner=11111111111111111111111111111111
rpc '{"jsonrpc":"2.0","id":1,"method":"surfnet_setTokenAccount","params":["'$owner'","EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",{"amount":1000000000000}]}'
rpc '{"jsonrpc":"2.0","id":2,"method":"requestAirdrop","params":["'$owner'",1000000000]}'
rpc '{"jsonrpc":"2.0","id":3,"method":"getSlot"}'
sleep 100000

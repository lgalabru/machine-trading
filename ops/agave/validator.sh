#!/bin/bash
# The mainnet-beta RPC node (non-voting) with the mtx geyser plugin. Run by
# sol.service as `sol`. Flags as Anza's guide gives them
# (https://docs.anza.xyz/operations/setup-an-rpc-node,
# https://docs.anza.xyz/clusters/available for mainnet's entrypoints,
# genesis hash and Anza's known validators).
#
# - No account indexes: the mtx runtime reads accounts by address, and an
#   index costs hundreds of GB of RAM.
# - JSON-RPC on localhost only: the runtime and `solana catchup` are its only
#   clients.
# - v4.4 renamed --limit-ledger-size to --limit-blockstore-size, whose count
#   is required: 200 million shreds, the old flag's default, a few days.
# - v4.4 needs at least 27 dynamic ports: 8000-8030, as the guide opens.
# - --no-xdp: v4.4 sends retransmits over XDP by default, which needs
#   CAP_NET_ADMIN and CAP_NET_RAW. UDP sockets until sol.service grants them
#   (AmbientCapabilities), an optimization a non-voting node can wait for.
# - `exec`, so the log reopens on USR1 (logrotate).
exec agave-validator \
  --identity /home/sol/validator-keypair.json \
  --known-validator 7Np41oeYqPefeNQEHSv1UDhYrehxin3NStELsSKCT4K2 \
  --known-validator GdnSyH3YtwcxFvQrVVJMm1JhTS4QVX7MFsX56uJLUfiZ \
  --known-validator DE1bawNcRJB9rVm3buyMVfr8mBEoyyu73NBovf2oXJsJ \
  --known-validator CakcnaRDHka2gXyfbEd2d3xsvkJkqsLw2akB3zsN1D2S \
  --only-known-rpc \
  --no-voting \
  --full-rpc-api \
  --private-rpc \
  --rpc-bind-address 127.0.0.1 \
  --rpc-port 8899 \
  --ledger /mnt/ledger \
  --accounts /mnt/accounts \
  --snapshots /mnt/snapshots \
  --log /home/sol/agave-validator.log \
  --dynamic-port-range 8000-8030 \
  --entrypoint entrypoint.mainnet-beta.solana.com:8001 \
  --entrypoint entrypoint2.mainnet-beta.solana.com:8001 \
  --entrypoint entrypoint3.mainnet-beta.solana.com:8001 \
  --entrypoint entrypoint4.mainnet-beta.solana.com:8001 \
  --entrypoint entrypoint5.mainnet-beta.solana.com:8001 \
  --expected-genesis-hash 5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d \
  --wal-recovery-mode skip_any_corrupted_record \
  --limit-blockstore-size 200000000 \
  --no-xdp \
  --geyser-plugin-config /home/sol/mtx/geyser.json

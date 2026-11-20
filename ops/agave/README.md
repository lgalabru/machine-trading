# A mainnet RPC node for the mtx runtime

The node RFD 0004's steps 6 and 7 run on: a mainnet-beta Agave RPC node
that doesn't vote, with the mtx geyser plugin loaded, its runtime
controlled by `mtx live` over a Unix socket on the node.

It follows Anza's guide
([validator or RPC node](https://docs.anza.xyz/operations/validator-or-rpc-node),
[setup an RPC node](https://docs.anza.xyz/operations/setup-an-rpc-node)).
Mainnet's entrypoints, genesis hash and Anza's known validators are from
[clusters/available](https://docs.anza.xyz/clusters/available).

## The machine

`mtx-alpha`, 213.239.140.203 (Hetzner), Ubuntu 26.04.1: an AMD EPYC 9555P
(64 cores, AVX-512, SHA-NI), 1.1 TB RAM, one 7 TB NVMe. The guide puts
accounts and ledger on separate disks; with one disk they are separate
directories on it (`/mnt/ledger`, `/mnt/accounts`, `/mnt/snapshots`), which
a node that doesn't vote can live with. A second NVMe for accounts is the
first change if replay falls behind.

## Versions

| | |
|---|---|
| Agave | v4.4.1, the latest stable on 2026-11-20 |
| Its Rust | 1.98.1 (`rust-toolchain.toml` at the tag) |
| Its plugin interface | `agave-geyser-plugin-interface` 5.0.0 |
| The plugin's build | `-p mtx-geyser --no-default-features --features agave-4-4`, with rustc 1.98.1 |

The plugin's default feature, `agave-4-3`, is surfpool's interface (4.3.2),
for the bench's tests. A trait object crosses between validator and
plugin with no stable ABI: change the Agave version and the plugin is
rebuilt against that version's interface and toolchain.

## Set up

1. As root, once: `./setup-root.sh`. It installs the build dependencies,
   sets the guide's sysctl and limits, creates `sol`, the directories, the
   systemd unit and log rotation, and opens SSH and ports 8000-8020 in ufw.
   JSON-RPC listens on localhost only.
2. As `sol`: `~/bin/install-agave.sh`: Agave v4.4.1's CLI from Anza's
   installer, its validator built from the release's source (the installer
   no longer ships `agave-validator`; a few minutes on this machine), and
   an unstaked identity at `~/validator-keypair.json`.
3. Deploy the plugin (below), then as `sol`: `~/bin/build-plugin.sh`.
4. As root: `systemctl enable --now sol`.

## Deploy the plugin

The repository is private, so the source goes from a checkout:

```sh
git archive HEAD | ssh mtx-alpha 'mkdir -p /tmp/mtx-src && tar -x -C /tmp/mtx-src'
# then on the node, as root: move /tmp/mtx-src to /home/sol/mtx/src, owned by sol,
# with the commit in /home/sol/mtx/src/.commit
```

## Watch it

```sh
solana catchup --our-localhost        # as sol: how far behind the cluster
tail -f /home/sol/agave-validator.log
mtx live --socket /home/sol/mtx/control.sock status    # the runtime, its feed
```

A node starting from a snapshot downloads it from a known validator, then
replays to the tip: expect an hour or more before `catchup` says it caught
up.

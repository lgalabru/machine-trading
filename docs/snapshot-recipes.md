# Snapshot recipes

A snapshot is the chain state a task trades on: a gzipped export of a
Surfpool fork (`surfnet.json.gz`) and a `manifest.json`. A **recipe**
(`snapshots/<name>/recipe.toml`) says what the snapshot holds and how to
build it, and one command builds any of them:

```sh
just snapshot-build <name>                     # forks MTX_FORK_RPC_URL (or the public RPC)
mtx snapshot build snapshots/<name>/recipe.toml --out /tmp/rebuilt
```

The build starts a surfnet (a mainnet fork, or another snapshot offline),
fetches the declared programs and accounts, runs the warm-up actions in
order, exports every account the surfnet holds and writes the manifest. It
fails if the export lacks anything the recipe declares, so a recipe can't
promise an account its warm-up never loads.

## The file

```toml
name = "sol-usdc-spot-2026-11-11"   # the snapshot's name
kind = "spot"                       # "spot", "stake-pool", or absent for Phoenix Eternal
symbol = "SOL-USDC"

[fork]                              # optional
from_snapshot = "../other"          # start offline from a snapshot instead of forking

[[program]]                         # fetched, and checked executable
name = "phoenix-v1"
address = "PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY"

[[account]]                         # fetched: markets, pools, mints, oracles
name = "phoenix-v1 SOL/USDC"
kind = "market"                     # free text, kept in the manifest
address = "4DoNfFBfF7UokCC2FQzriy7yHK6DY6NVdYpuekQ5pRgg"

[[warmup]]                          # run in order
action = "spot-exercise"
```

Unknown keys and actions are errors.

## Warm-up actions

Real venues:

| Action | What it does |
|---|---|
| `phoenix-eternal` (`symbol`, `phoenix_api`) | Onboards a throwaway trader, reads the market, rests and cancels an order, and replays the market-move scenario templates so the oracle and maker accounts load. Writes `exchange.json`. |
| `spot-exercise` | Funds a throwaway trader and takes the paths a spot session takes on Phoenix v1 and OpenBook v2 SOL/USDC: seat or open orders account, a resting order and its cancel, a small sell. |
| `stake-pool-exercise` | Funds a throwaway wallet and takes the paths a stake-pool session takes on Jito's pool and Marinade: a stake and an instant redemption on each (Jito's reserve may refuse it: only logged), and the first step of each delayed exit, a stake account split off Jito's largest validator and deactivated, and a Marinade ticket. |

Simulated assets (see below): `create-mint`, `phoenix-v1-market`,
`openbook-v2-market`. Pool creation for AMMs is left for the AMM venue
modules; they add actions here.

The account set depends on what the warm-up touches, which is why warm-up
is part of the recipe: a path a session takes that no warm-up exercised
would fetch from mainnet at run time, and the offline surfnet has no
mainnet.

## Simulated markets

Mainnet has no depth for some assets a task needs (Ondo's tokens are
empty, most SPCX liquidity sits in proprietary pools that can't be
forked). A recipe can make a mint and a market on the real program
instead, so a bot trades them with the code it uses on mainnet.

```toml
[[account]]
name = "USDC"
kind = "mint"
address = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"

[[warmup]]
action = "create-mint"
name = "NVDAon"
decimals = 9                      # "spl" (default); "token-2022" for the AMM actions

[[warmup]]
action = "openbook-v2-market"
name = "NVDAon/USDC"
base = "NVDAon"                   # a created mint, a declared account's name, or an address
quote = "USDC"
base_lot_atoms = 1000000          # sizes are in token atoms: the grid is explicit
quote_lot_atoms = 1               # OpenBook needs 1 (see below)
price = { calibration = "calibration/nvda.toml", key = "fair_value.price" }
ladder = { levels = 5, half_spread_bps = 10, step_bps = 10, level_size = "20" }
```

- `price` is `{ value = 12.5 }` or a calibration TOML (relative to the
  recipe) plus the dotted key of a number in it.
- The ladder posts `levels` post-only orders a side, each `level_size` base
  units, the first `half_spread_bps` from the price and the rest `step_bps`
  apart, on the market's tick and lot grid (bids round down, asks up).
- `phoenix-v1-market` takes classic SPL mints and `bids_size`, `asks_size`,
  `num_seats` (the program only runs the sizes it was compiled for; the
  default, 4096 / 4096 / 8321, is the SOL/USDC market's), and
  `taker_fee_bps`. Its lots are `base_lot_atoms`, `quote_lot_atoms` and
  `tick_quote_atoms_per_base_unit`, which must divide into whole lots.
- `openbook-v2-market` takes classic SPL mints, `maker_fee` and `taker_fee`,
  and needs `quote_lot_atoms = 1`: the kit's order builder leaves the quote
  limit at `i64::MAX`, which the program rejects for a larger quote lot.
- The deployed Phoenix v1 and OpenBook v2 programs do not trade Token-2022
  mints. `create-mint` can still make one (`program = "token-2022"`), for
  the AMM pool actions that follow; an order-book action on it fails the
  build with a message that says so.
- Mints are written straight into the surfnet with no mint authority and no
  freeze authority, so nobody can mint more.
- A Phoenix v1 market is handed over as mainnet's are: after the ladder is
  posted, the creator names the Seat Manager's market PDA its successor and
  the Seat Manager claims the authority. A bot then claims its seat with
  the kit's `plan_claim_seat` and rests post-only orders like on mainnet
  (`crates/grader/tests/recipe.rs` does exactly that).
- The market's creator is the maker of the ladder; it is
  funded by cheatcode with both mints. Its key, the market keys and the
  mints' addresses derive from the build's **secret**, the recipe name and
  the market's name. The recipe name ships with the snapshot, so the secret
  is what keeps the keys private: it is never recorded, and a build draws a
  random one unless `--secret` (or `MTX_BUILD_SECRET`) gives it. Give the
  same secret to rebuild the same addresses; without it the addresses
  differ and the manifest records the new ones.

## The manifest

Alongside the fork slot and time, the manifest records:

```json
{
  "recipe_sha256": "…",
  "sources": [{ "role": "market", "name": "phoenix-v1 SOL/USDC", "address": "…" }],
  "simulated": true,
  "created": [
    { "kind": "openbook-v2-market", "name": "NVDAon/USDC", "address": "…",
      "program": "opnb2…", "simulated": true,
      "detail": { "price": "…", "levels": 5, "base_mint": "…", "quote_mint": "…" } }
  ]
}
```

`simulated` is absent when the recipe made nothing. A task whose snapshot
has `simulated: true` should say so in its instruction, and name the
assets in `created`.

## Reproducibility

A build from a mainnet fork depends on mainnet at the fork slot, so two
builds differ in prices, slots and times; the manifest's recipe hash,
fork slot and sources say what was built and when.

**Program code.** Mainnet programs are upgraded in place, so the same
address can run different code a week later. This is not hypothetical: after
the SPCX snapshot was taken, Phoenix Eternal and Phoenix Rise were
redeployed, and a fresh fork of them no longer accepts the funding
scenario's overrides, so a rebuilt SPCX snapshot fails the funding-carry
task. The manifest therefore records, for every declared program, its
ProgramData account, deploy slot and a hash of its code (`programdata`,
`deploy_slot`, `code_sha256` in `sources`). A recipe can then:

- `expect_code_sha256 = "…"`: fail the build when the code on the chain
  differs, and
- `pin_from = "."` (any snapshot directory, relative to the recipe): replace
  the chain's ProgramData with the one in that snapshot, so the rebuild runs
  the code the task was built on. The source's name is recorded as
  `pinned_from`.

The two committed recipes pin to their own snapshots.

**Clock.** A build from `fork.from_snapshot` starts from the source's fork slot
and time, makes a block for each transaction and none otherwise, and pins
the clock to a fixed slot and time before each step of a simulated market
(a surfnet finalizes blocks asynchronously, so the time between two
transactions would otherwise vary with the machine and be written into
the books), so nothing the warm-up writes depends on how long the machine
takes. The manifest's `fork_slot` and `created_at_unix` are then the slot and
time after the last pinned step, which is where a session starts, so its
clock is never before an order's recorded time. With the same secret it is byte-identical
on every run (`crates/grader/tests/recipe.rs` checks this under parallel
test threads). The surfnet's own payer and faucet, random on every start,
are left out of the export. A build from a mainnet fork runs on the wall
clock.

**Overwriting.** `mtx snapshot build` refuses to replace a
`surfnet.json.gz` already in the output directory; pass `--out DIR`, or
`--force`. An `exchange.json` comes from the `phoenix-eternal` action, or is
copied from the `from_snapshot` source.

The two original `prepare` commands are the `phoenix-eternal` and
`spot-exercise` actions. Their warm-up trader is a random keypair, as before,
so a rebuild's trader accounts have different addresses than the committed
snapshot's; everything else it touches is the same set.

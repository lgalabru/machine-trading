# Venue profile: Raydium (`raydium`), v0.1.0

Status: draft

[Raydium](https://raydium.io) runs several pool programs on Solana. This
version profiles one, its **concentrated-liquidity market maker** (CLMM),
program `CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK`. A CLMM pool trades
two tokens over a curve split into ticks; liquidity providers supply it
over tick ranges, and a swap walks the ticks it crosses. Each pool is an
[`amm`](../instruments/amm.md) instrument. Raydium's constant-product
programs, AMM v4 and CPMM, are listed by the pool profile but not specified
here yet.

## Instruments

| Reference | Meaning |
| --- | --- |
| `raydium:amm:<ADDRESS>` | The CLMM pool at the base58 address `<ADDRESS>`. |
| `raydium:amm:*` | A [wildcard](../mandate.md#wildcard-allowances): every CLMM pool, or, with `assets`, every pool whose two mints are both listed. |

A pool is named by its address, since a pair has a pool per fee config. An
account the CLMM program doesn't own, or that isn't a `PoolState`, is no
instrument of this venue. Once Raydium's other programs are profiled, a
`raydium:amm:` reference will name a pool of any of them, and the account's
owner will say which; a wildcard will match all of them.

A pool's **base** is its token 0 (`token_mint_0`) and its **quote** its
token 1 (`token_mint_1`), so its price is 1 per 0. Raydium orders the two
mints by address.

Pools the benchmark's snapshots hold (`snapshots/atomic-arb-2026-11-21`
holds the first, `snapshots/thin-pools-2026-11-21` all four):

| Pool | Base/quote | Fee | Tick spacing |
| --- | --- | --- | --- |
| `3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv` | SOL/USDC | 4 bp | 1 |
| `JVoPtWWDsRcLvQosu5fWc2CaNF6jEtJzbxdPtcEuvZo` | SOL/JTO | 100 bp | 120 |
| `3RA4PDZAZ49nm6hdDCnCTrnECqQD93ny8oHP4T1CSTcR` | SOL/GEOD | 5 bp | 1 |
| `BkX9RSHoDJGkU7pbHiksv3DprdLM3KWGoDSfGN9cGJ5d` | GEOD/USDC | 25 bp | 60 |

## Account

The account is the agent authority's wallet: its token accounts for each
pool's two mints, under SPL Token or Token-2022, and its lamports for SOL.
A swap holds nothing in the pool. A liquidity position is an NFT-backed
`PersonalPositionState` the authority would own, which this version
doesn't measure (see [Not covered](#not-covered)).

## Measurements

- **The pool's assets.** `token_mint_0` and `token_mint_1`, read from the
  pool account. A wildcard's `assets` is matched against these, never
  against anything the agent says.
- **Price.** `sqrt_price_x64`, a Q64.64 square root of token-1 atoms per
  token-0 atom; the price in units is its square scaled by the mints'
  decimals. The fee is the pool's `AmmConfig`'s `trade_fee_rate`, in
  millionths, plus the pool's dynamic fee where it has one. A pool refuses
  swaps before its `open_time`.
- **Top.** The price of a small swap each way, fees included, quoted
  exactly over the pool's tick arrays, as the
  [pool profile](../instruments/amm.md#prices) defines it.
- **Fills.** One per transaction that moved the pool's two vaults
  (`token_vault_0`, `token_vault_1`) in opposite directions, read from the
  transaction's token balances: the side is the base vault's direction (the
  vault receiving base is a sell by the taker), the size its change, and the
  price the quote vault's change over the base vault's, fees included.
- **Holdings.** The wallet's balances of both mints, after each landed
  transaction, from that transaction's own token balances.

## Swaps

`mtx:strategy@0.3.0`'s `swap` line on a `raydium:amm:<ADDRESS>` venue is
sent as the CLMM program's `swap_v2`, after idempotent creation of the
wallet's token accounts for both mints:

| Line | `swap_v2` |
| --- | --- |
| `side` | `bid` buys the base: the input vault is token 1's. `ask` sells it: the input vault is token 0's. |
| `amount`, `unit` | `base` with `ask`, or `quote` with `bid`, is an exact input; the other two are an exact output (`is_base_input`). |
| `limit` | A price, quote per base, fees included; converted to the program's `other_amount_threshold` (the least received or the most paid), so a pool that moved past the limit refuses the swap on chain. `sqrt_price_limit_x64` is 0: no partial fills. |

`swap_v2` names the pool's `AmmConfig` and observation account, both token
programs and the memo program whatever the mints are, then, as remaining
accounts, the pool's tick-array bitmap extension (when a swap may reach
past the pool's own bitmap) and the tick arrays the swap enters, in order.

Inside an atomic group (`tx-options.atomic`), each swap line is one
instruction of the group's transaction, and the last line's limit is the
route's least profit.

## Not covered

- Liquidity positions: `open_position_v2`, `increase_liquidity_v2`,
  `decrease_liquidity_v2` aren't sent, and a position's holdings aren't
  measured.
- Token-2022 transfer fees and transfer hooks.
- Raydium AMM v4 (`675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8`) and CPMM
  (`CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C`).

## The benchmark's enforcer

An `amm` session (`venues.amm` in a scenario, `docs/scenario-format.md`)
runs on a snapshot that clones each pool with its `AmmConfig`, observation,
bitmap extension, mints, vaults, and the initialized tick arrays within 12
of the current one (`clone-pool`). The kit's quotes
(`mtx_kit::protocols::raydium::clmm`, feature `raydium-clmm`) port the
program's swap, so a quote is what the program does on the same state, to
the atom.

- **Universe.** When the session starts, each of its pools is checked
  against the mandate with its two mints: a named allowance, or a wildcard
  whose `assets` hold both. A pool the mandate doesn't admit fails the
  session before it runs. Every fill is checked again.
- **Valued** at the scenario's reference prices for each asset, never at a
  pool's own price: pools are moved by the trades they price.
- **Mandate v0.3.0.** Rule 11 is judged after every landed transaction from
  its own token balances; rule 13 on every transaction that landed or
  failed. A session lends nothing, so rule 12 can't be broken in it.

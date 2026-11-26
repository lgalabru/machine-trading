# Venue profile: Orca (`orca`), v0.1.0

Status: draft

[Orca](https://www.orca.so) runs concentrated-liquidity pools on Solana,
**Whirlpools**, program `whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc`. A
Whirlpool trades two tokens over a curve split into ticks; liquidity
providers supply it over tick ranges, and a swap walks the ticks it crosses.
Each pool is an [`amm`](../instruments/amm.md) instrument.

## Instruments

| Reference | Meaning |
| --- | --- |
| `orca:amm:<ADDRESS>` | The Whirlpool at the base58 address `<ADDRESS>`. |
| `orca:amm:*` | A [wildcard](../mandate.md#wildcard-allowances): every Whirlpool, or, with `assets`, every Whirlpool whose two mints are both listed. |

A pool is named by its address, since a pair has a pool per fee tier and
tick spacing. An account the Whirlpool program doesn't own, or that isn't a
`Whirlpool`, is no instrument of this venue.

A pool's **base** is its token A (`token_mint_a`) and its **quote** its
token B (`token_mint_b`), so its price is B per A. Whirlpool orders the two
mints by address, which makes SOL the base of SOL/USDC but JTO the base of
JTO/JitoSOL.

Pools the benchmark's snapshots hold (`snapshots/atomic-arb-2026-11-21`
holds the first, `snapshots/thin-pools-2026-11-21` all six):

| Pool | Base/quote | Fee | Tick spacing |
| --- | --- | --- | --- |
| `Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE` | SOL/USDC | 4 bp | 4 |
| `Hp53XEtt4S8SvPCXarsLSdGfZBuUr5mMmZmX2DRNXQKp` | SOL/JitoSOL | 1 bp | 1 |
| `G2FiE1yn9N9ZJx5e1E2LxxMnHvb1H3hCuHLPfKJ98smA` | JTO/JitoSOL | 30 bp | 64 |
| `4UVvSVdeth9mxpPe4nwwZseXPd2bqqDm4xVaJSve3DXw` | JTO/JitoSOL | 100 bp | 128 |
| `2UhFnySoJi6c89aydGAGS7ZRemo2dbkFRhvSJqDX4gHJ` | SOL/JTO | 30 bp | 64 |
| `FsPFkRRTyCJrwK3S8x8JvU8Vmruk5EGKKQsLqywfvF5E` | SOL/GEOD | 30 bp | 64 |

## Account

The account is the agent authority's wallet: its token accounts for each
pool's two mints, under SPL Token or Token-2022, and its lamports for SOL.
A swap holds nothing in the pool. A liquidity position is an NFT-backed
`Position` account the authority would own, which this version doesn't
measure (see [Not covered](#not-covered)).

## Measurements

- **The pool's assets.** `token_mint_a` and `token_mint_b`, read from the
  pool account. A wildcard's `assets` is matched against these, never
  against anything the agent says.
- **Price.** `sqrt_price`, a Q64.64 square root of B atoms per A atom; the
  price in units is its square scaled by the mints' decimals. The fee is
  `fee_rate`, in millionths, plus an adaptive fee when the pool's fee tier
  has one, held in the pool's `Oracle` account (the PDA of `oracle` and the
  pool). A pool without an oracle account has no adaptive fee.
- **Top.** The price of a small swap each way, fees included, quoted
  exactly over the pool's tick arrays, as the
  [pool profile](../instruments/amm.md#prices) defines it.
- **Fills.** One per transaction that moved the pool's two vaults
  (`token_vault_a`, `token_vault_b`) in opposite directions, read from the
  transaction's token balances: the side is the base vault's direction (the
  vault receiving base is a sell by the taker), the size its change, and the
  price the quote vault's change over the base vault's, fees included.
- **Holdings.** The wallet's balances of both mints, after each landed
  transaction, from that transaction's own token balances.

## Swaps

`mtx:strategy@0.3.0`'s `swap` line on an `orca:amm:<ADDRESS>` venue is sent
as Whirlpool's `swap_v2`, after idempotent creation of the wallet's token
accounts for both mints:

| Line | `swap_v2` |
| --- | --- |
| `side` | `bid` buys the base: `a_to_b` false. `ask` sells it: `a_to_b` true. |
| `amount`, `unit` | `base` with `ask`, or `quote` with `bid`, is an exact input; the other two are an exact output (`amount_specified_is_input`). |
| `limit` | A price, quote per base, fees included; converted to the program's `other_amount_threshold` (the least received or the most paid), so a pool that moved past the limit refuses the swap on chain. |

`swap_v2` names each mint and its token program, the memo program, the
pool's oracle, and up to three tick arrays, fixed or dynamic, in the
direction of the swap. A swap that would cross a third array fails.

Inside an atomic group (`tx-options.atomic`), each swap line is one
instruction of the group's transaction, and the last line's limit is the
route's least profit.

## Not covered

- Liquidity positions: `open_position`, `increase_liquidity`,
  `decrease_liquidity` and `collect_fees` aren't sent, and a position's
  holdings aren't measured.
- Token-2022 transfer fees (on such a mint `swap_v2` moves the fee on top
  of the quote) and transfer hooks.
- Supplemental tick arrays beyond the three `swap_v2` takes.

## The benchmark's enforcer

An `amm` session (`venues.amm` in a scenario, `docs/scenario-format.md`)
runs on a snapshot that clones each pool with its config, mints, vaults,
oracle where it exists, and the tick arrays within 12 of the current one
(`clone-pool`). The kit's quotes
(`mtx_kit::protocols::orca::whirlpool`, feature `orca-whirlpool`) port the
program's swap line by line, so a quote is what the program does on the
same state, to the atom.

- **Universe.** When the session starts, each of its pools is checked
  against the mandate with its two mints: a named allowance, or a wildcard
  whose `assets` hold both. A pool the mandate doesn't admit fails the
  session before it runs. Every fill is checked again.
- **Valued** at the scenario's reference prices for each asset, never at a
  pool's own price: pools are moved by the trades they price.
- **Mandate v0.3.0.** Rule 11 is judged after every landed transaction from
  its own token balances; rule 13 on every transaction that landed or
  failed. A session lends nothing, so rule 12 can't be broken in it.

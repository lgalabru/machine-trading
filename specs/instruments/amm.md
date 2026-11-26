# Instrument profile: pool (`amm`), v0.1.0

Status: draft

The bot interface's `amm` market kind (`mtx:strategy@0.2.0` and later),
and mandate v0.3.0's `amm` instruments. The venues that implement it have
their own profiles: [Orca](../venues/orca.md) and
[Raydium](../venues/raydium.md).

A pool trades an asset for another against a curve rather than an order
book: constant product (Raydium CPMM), concentrated liquidity (Raydium
CLMM, Orca Whirlpool) or bins (Meteora DLMM). A **swap** pays one asset and
receives the other at the curve's price, and moves it. A **liquidity
position** supplies both assets over a price range (the whole curve, a tick
range or a bin range) and earns the fees swaps through it pay.

A pool is two instruments in one: as a market it is spot (a swap is a spot
trade), and as a liquidity position it is a holding of its own.

## Position

The position in an `amm` instrument is the account's **holdings of the
base asset**, as for [spot](spot.md): in the wallet, and inside each of the
account's liquidity positions in the pool, at the amounts a withdrawal now
would return, uncollected fees included. A mandate's `max_position` bounds
that total.

## Sides

A swap that receives the base asset is a **buy**, one that gives it up a
**sell**. Adding liquidity is neither until the curve moves: a position's
base grows as the price falls through its range (it buys) and shrinks as the
price rises (it sells). This profile counts those as fills of the position,
at the price they happened at, so `sides` and the price limits audit them.

## Prices

A swap's price is the quote paid or received per base unit, fees included,
as the pool's vaults record it. The market's top is the price of a small
swap each way.

## Value

Both assets, wherever the account holds them (wallet and every liquidity
position, fees included), at the [spot](spot.md#value) reference prices.

## Arbitrage

Most trading through pools on Solana is atomic arbitrage: one transaction
that swaps through two or more pools and ends holding what it started
with, plus a profit ([research](../../docs/RESEARCH.md#atomic-sol-arbitrage-on-mainnet-from-jitos-explorer-2026-11-21)).
This profile treats it as follows:

- **Symbol.** A pool's symbol is its account address, in base58. Pairs
  aren't unique: a token pair has many pools on one venue, one per fee tier
  or curve. `orca:amm:Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE` is
  Whirlpool's SOL/USDC pool with 4 bp fees.
- **Venues.** The profile covers these programs:

  | Venue | Program | Curve | Venue profile |
  | --- | --- | --- | --- |
  | `orca` | Whirlpool `whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc` | concentrated, ticks | [orca](../venues/orca.md) |
  | `raydium` | CLMM `CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK` | concentrated, ticks | [raydium](../venues/raydium.md) |
  | `raydium` | AMM v4 `675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8` and CPMM `CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C` | constant product | not yet |
  | `meteora` | DLMM `LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo` | bins | not yet |
  | `meteora` | DAMM v1 `Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB` | constant product or stable | not yet |

  A pool's symbol is its account's address, and the account's owner says
  which program it belongs to. A wildcard (`orca:amm:*`) matches every
  pool of its venue, narrowed by `assets` to the pools whose two mints
  are both listed ([mandate](../mandate.md#wildcard-allowances)).
- **A route is several fills.** Each swap in a route is a fill in its
  pool, with the side and price the [Sides](#sides) and [Prices](#prices)
  sections give. A route is not an instrument. The mandate audits each of
  its swaps, and audits what the account holds once the transaction ends.
- **Positions between transactions.** On-chain state can only be read
  between transactions, so a token the route passes through never shows
  up as a holding. An enforcer that measures after every transaction the
  account lands sees exactly what the route leaves behind. Mandate v0.3.0
  bounds that with an [inventory limit](../mandate.md#inventory).
- **Flash loans.** A flash borrow from a money market, repaid in the same
  transaction, is not a position either: the program refuses a transaction
  that doesn't repay it. The mandate bounds its size per
  transaction and reserve ([`max_flash_borrow`](../mandate.md#flash-borrows)).
- **Price for valuation.** Pools are moved by the trades they price, so a
  pool's own price is never the price an account is valued at. A session
  values every asset at the scenario's reference price for it, as for
  [spot](spot.md#value).

## Implementation status

What the grader and the kit implement of this profile (2026-11-21):

- **Venues**: `orca` Whirlpool and `raydium` CLMM, with exact quotes in the
  kit (`mtx_kit::protocols::orca::whirlpool`,
  `mtx_kit::protocols::raydium::clmm`), which a strategy component can also
  build in (features `orca-whirlpool-venue` and `raydium-clmm-venue`). The
  runtime's `amm` venue swaps through them (`crates/runtime/src/venue/amm.rs`).
  Raydium AMM v4 and CPMM and the Meteora programs aren't implemented.
- **Swaps only**: no liquidity positions, so the position is the wallet's
  base, and adding or removing liquidity isn't sent.
- **Fills** are read from each pool's vaults, one per transaction that
  moved them in opposite directions, at the average price they recorded,
  fee included; the worst tick isn't named, so a limit price is judged at
  the average.
- **Valuation**: an `amm` session values the wallet's SOL at the scenario's
  reference price (`venues.amm.reference_usd`, moved by its path) and USDC
  at a dollar, never at a pool's price (`docs/scenario-format.md`, "An amm
  session").
- **Routes**: an atomic group of swap lines is one transaction
  (`tx-options.atomic`); the last line's limit is the route's least
  profit. Mandate v0.3.0's inventory rule is judged after each of the
  account's transactions from that transaction's own token balances.

## Open questions

- A new `Holding` variant (`Holding::Liquidity`: range, base, quote, fees),
  or liquidity counted into `Holding::Spot`'s `locked`.
- Whether a range order (a concentrated position entirely on one side of the
  price) is audited as a resting order.

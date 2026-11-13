# Instrument profile: pool (`amm`), v0

Status: draft, proposed (not yet agreed). The bot interface's `amm` market
kind (`mtx:strategy@0.2.0`) needs a profile; this is the proposal.

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
price rises (it sells). The proposal counts those as fills of the position,
at the price they happened at, so `sides` and the price limits audit them.

## Prices

A swap's price is the quote paid or received per base unit, fees included,
as the pool's vaults record it. The market's top is the price of a small
swap each way.

## Value

Both assets, wherever the account holds them (wallet and every liquidity
position, fees included), at the [spot](spot.md#value) reference prices.

## Open questions

- A new `Holding` variant (`Holding::Liquidity`: range, base, quote, fees),
  or liquidity counted into `Holding::Spot`'s `locked`.
- Whether a range order (a concentrated position entirely on one side of the
  price) is audited as a resting order.

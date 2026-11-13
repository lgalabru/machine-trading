# Instrument profile: pool liquidity (`amm`), v0

Status: draft, proposed for review (not yet agreed). The bot interface's
`amm` market kind (`mtx:strategy@0.2.0`) needs a profile; this is the
proposal, as [Raydium](../venues/raydium.md) and
[Meteora](../venues/meteora.md) implement it.

A pool trades an asset for another against a curve rather than an order
book: constant product (Raydium CPMM), concentrated liquidity (Raydium
CLMM, Orca Whirlpool) or bins (Meteora DLMM). It offers two instruments:

- **Swapping** against the pool is a [spot](spot.md) instrument
  (`<venue>:spot:<SYMBOL>`): a swap pays one asset and receives the other at
  the curve's price, and moves it. Nothing rests and nothing is locked, so
  the spot position is the wallet's base. A venue profile documents how its
  swaps differ from a book's (no resting orders, a fill's price includes
  the pool's fee).
- **Providing liquidity** is this profile (`<venue>:amm:<SYMBOL>`): one or
  more **liquidity positions**, each supplying both assets over a price
  range (the whole curve, a tick range or a bin range) and earning the fees
  swaps through it pay.

## Position

The position in an `amm` instrument is the **base asset its liquidity
positions hold**, at the amounts a withdrawal now would return, plus their
uncollected base fees, in base units. A mandate's `max_position` bounds
that total; the wallet's base belongs to the spot instrument, so a mandate
that bounds both lists both.

A position's holdings move with the pool's price: its base grows as the
price falls through its range (the position buys) and shrinks as the price
rises (it sells).

## Sides

Adding and removing liquidity are neither buys nor sells, and neither is
audited as a fill: `sides` and the price limits audit the account's swaps
(its spot fills). The curve's trades against a position aren't fills of
the account either.

## Prices

A range is in quote per base, like a book's prices; each venue profile says
how it maps onto the pool's grid (ticks, bins).

## Value

What each position holds of both assets, and its uncollected fees, at the
[spot](spot.md#value) reference prices, never the pool's own price (the
account's own swaps move it). Rewards a pool pays in other tokens are not
valued.

## Holding

The reference implementation reads a position as `Holding::Amm`: its id
(the venue's name for it), both assets, the amounts and fees of each, and
its range.

## Open questions

- Whether a position's base should count towards the spot instrument's
  position too (one base-asset position, wherever it sits), rather than the
  amm instrument's alone.
- Whether the curve's trades against a position should be audited as its
  fills, at the price they happened at, so `sides` and the price limits
  bound liquidity provision too.
- Whether a range order (a concentrated position entirely on one side of the
  price) is audited as a resting order.

# Instrument profile: pool liquidity (`amm`), v0

Status: draft, proposed for review (not yet agreed). The bot interface's
`amm` market kind (`mtx:strategy@0.2.0`) needs a profile; this is the
proposal, as [Raydium](../venues/raydium.md),
[Meteora](../venues/meteora.md) and [Orca](../venues/orca.md) implement it.

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
that total.

The same base also counts toward the [spot](spot.md#position) position of
that asset, which is one position wherever the account holds it: a mandate
that caps the asset's spot position caps the wallet's base and every pool
position's together, so base can't be parked in a pool to hold more than
the cap. An `amm` allowance's own `max_position` bounds one pool's
positions in addition.

A position's holdings move with the pool's price: its base grows as the
price falls through its range (the position buys) and shrinks as the price
rises (it sells).

## Sides

Adding and removing liquidity count as trades for an allowance's `sides`,
in base, as the order books count a resting order by the side it would
trade:

- a **deposit that takes base into the pool is a buy**, and, since its
  range reaches above the price (across it, or wholly above it with base
  alone) and sells that base as the price rises, **also a sell**, as a
  resting ask is;
- a **deposit of the quote asset alone** (a range below the price, which
  buys base as the price falls into it) **is a buy** only, as a resting
  bid is;
- a **withdrawal that returns base is a sell**. Returning base includes
  collecting fees paid in it. A withdrawal that returns only the quote
  asset is neither.

So a sell-only or exit-only mandate can reduce or close positions but
can't deposit, and a buy-only one can deposit only quote alone, below the
price. The rule is checked on the `amm` instrument's own allowance, and the
limit prices don't apply: a deposit or a withdrawal has no price of its
own. The curve's trades against a position aren't fills of the account:
`sides` and the price limits audit the account's swaps (its spot fills)
as before.

An enforcer measures it from the chain, **instruction by instruction**:
each of the pool's instructions in a transaction, whether the transaction
runs it or another program calls it, is read from the token transfers it
makes to and from the pool's vaults, never from the transaction's net (a
swap and a deposit in one transaction are each seen). A move is the
account's when it takes the account's tokens (the owner of a token account
it passes), its position (the holder of the position's NFT, or the owner a
position records) or its signature, whoever pays the fee. So a bot's
hand-built transaction counts as much as an order a seat sent. A gate
refuses such an order up front, with the reason, and refuses an order
whose move it can't tell where the allowance restricts the sides.

## Position held

A position is the account's wherever it holds the position's NFT: in any
of its token accounts for the NFT's mint, not only the associated one, as
the wallet's assets are read.

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

- Whether the curve's trades against a position should be audited as its
  fills, at the price they happened at, so the price limits bound liquidity
  provision too.

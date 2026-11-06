# Instrument profile: spot (`spot`), v0

Status: draft

A spot market trades an asset for another asset, now: a buy pays the
**quote** asset and receives the **base** asset, and a sell does the
opposite. On Solana both are tokens, such as SOL (base) for USDC (quote) in
`SOL-USDC`, and a fill moves them between the venue's accounts and the
trader's.

Spot differs from a [perpetual](perp.md) in what the account holds. A spot
account holds the asset itself, so there is no margin, no liquidation and
no funding, and it cannot hold less than nothing: selling more than the
account holds would need a loan, which is a `money-market` instrument, not
spot.

Spot is defined from mandate v1. A v0 reader rejects it.

## Position

The position in a spot instrument is the account's **holdings of the base
asset**, in its base units, such as SOL. It is never negative. It counts the
base asset wherever the account holds it:

- in the agent authority's wallet;
- on each venue in the universe, in the accounts its venue profile names,
  whether free to withdraw or locked behind the account's open orders.

A spot position therefore belongs to the asset, not to one venue. Two
allowances on the same base asset, such as `openbook:spot:SOL-USDC` and
`phoenix:spot:SOL-USDC`, measure the same holdings, and each one's
`max_position` applies to them.

`max_position` bounds the holdings, including what the account held when
the term started. A mandate to sell 30,000 SOL sets it at 30,000 or more.

Open orders do not change the position until they fill. An ask locks the
base asset it offers, which still counts, since the account still holds it;
a bid locks the quote asset.

## Sides

A **buy** fill receives the base asset, and a **sell** fill gives it up,
whether the account was the taker or the maker. An allowance's `sides` is
checked against both its fills and its open orders, as the
[mandate](../mandate.md#rules) states.

## Prices

A fill's price is the quote asset paid or received per base unit, before
fees, as the venue records it. `min_sell_price` and `max_buy_price` are in
the same units: for `SOL-USDC`, USDC per SOL.

## Value

A spot instrument contributes the account's holdings of both its assets to
the account's value:

- the base asset's holdings, at the base asset's reference price;
- the quote asset's holdings, at the quote asset's reference price.

The venue profile defines both reference prices. A venue's own order book
is not a reference price: the account's own orders can move it.

Each asset is counted once in the account's value, however many spot
instruments or venues hold it.

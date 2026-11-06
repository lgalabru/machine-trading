# Instrument profile: money market (`money-market`), v0

Status: draft

A money market pools one asset from many lenders and lends it to
borrowers. A **supply** lends the asset to the pool and earns interest; a
**borrow** takes it from the pool and pays interest. Borrowing needs
**collateral**: assets the account has supplied, worth more than what it
owes, which the venue may sell (**liquidate**) if the debt grows too close
to the collateral's value.

On Solana a money market is a lending program, such as Kamino Lending, with
one pool (a **reserve**) per asset in each lending market. The reserves of
one lending market back each other: assets supplied to one serve as
collateral for borrows from another.

The reference implementation does not read money-market instruments yet.

## Position

The position in a money-market instrument is what the account has supplied
to the reserve **minus** what it has borrowed from it, in the reserve
asset's base units, such as USDC. It is positive when the account lends and
negative when it borrows.

Both sides count interest as it accrues. A supply's claim on the pool grows
with the interest lenders earn, and a borrow's debt grows with the interest
borrowers pay, so a position moves between two measurements even when the
agent does nothing. The venue profile says how each is read.

`max_position` bounds the supply, and `min_position` the borrow: with
`"max_position": 1000, "min_position": -50000`, the account may supply up
to 1,000 or borrow up to 50,000. Without `min_position`, the borrow is
bounded by `max_position` too. `min_position: 0` forbids borrowing the
asset at all, and `"max_position": 0, "min_position": -50000` allows only
a borrow.

`sides`, `min_sell_price` and `max_buy_price` do not apply: a money market
has no fills and no prices of its own.

## Health

Every borrowing position the venue profile names has a **health**: the
venue's liquidation threshold for the position, as a share of what it
owes. Health is above 1 while the position is safe, and the venue may
liquidate it once health falls to 1 or below. The mandate's `min_health`
limits it; the venue profile defines it exactly.

## Value

A money-market instrument contributes what the account has supplied, at the
asset's reference price, minus what it has borrowed, at the same price. The
venue profile defines the reference price. Collateral counts once: an asset
supplied to a reserve is the supply, not also a holding in the wallet.

# Instrument profile: perpetual (`perp`), v0

Status: draft

A perpetual future is a contract on an underlying's price with no expiry. It
stays near the underlying's spot price through periodic funding payments
between longs and shorts. A dated future has an expiry and converges to spot
by settling at it. The two carry different risks: a dated future has to be
rolled at expiry, while a perpetual pays or earns funding for as long as it
is held. They are therefore separate kinds, and `future` is reserved.

## Position

A position is a signed quantity of the underlying, in its **base units**,
such as shares or tokens. It is positive when long and negative when short.
The venue profile defines the base unit and how to convert the venue's own
quantities into it.

`max_position` bounds the absolute value of the filled position. Open
orders do not count until they fill, so an agent that must stay under the cap
sizes its resting orders so that fills can't take it over.

## Value

A perpetual position contributes its unrealized PnL at the venue's mark price
to the account's value. The venue profile defines the mark, and whether
unsettled funding is counted.

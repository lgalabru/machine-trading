# Instrument profile: stake pool (`stake-pool`), v0

Status: draft, proposed (not yet agreed). The bot interface's `stake-pool`
market kind (`mtx:strategy@0.2.0`) needs a profile; this is the proposal.

A stake pool takes SOL, stakes it with validators, and issues a liquid
staking token (an LST, such as JitoSOL or mSOL) that redeems for a growing
amount of SOL. A **deposit** pays SOL and receives the token at the pool's
exchange rate; a **withdrawal** returns the token for SOL, at once (a fee)
or as a stake account that unlocks at the end of the epoch.

## Position

The position in a `stake-pool` instrument is the account's **holdings of
the pool's token**, in its base units, wherever the account holds it: the
wallet, and withdrawals not yet settled.

## Sides

A deposit is a **buy** of the token, a withdrawal a **sell**, at the pool's
exchange rate (SOL per token) as the deposit or withdrawal recorded it.

## Value

The token at the pool's exchange rate times the SOL reference price
([spot](spot.md#value)), not at a market's price for it: a stake pool's own
rate is what a redemption returns. A withdrawal waiting for the epoch counts
at the SOL it will return.

## Open questions

- Whether the value uses the pool's exchange rate or the token's market
  price when the two differ (a depeg).
- How an epoch boundary in a session is modelled (the rate moves once an
  epoch).

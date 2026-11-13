# Instrument profile: stake pool (`stake-pool`), v0

Status: draft, **proposed for Ludo's approval** (not yet agreed). No
existing profile fits a stake pool: a deposit and a withdrawal trade SOL
for the pool's token like a spot market, but at a rate the pool sets, not
against other traders, and an exit can wait for an epoch. The bot
interface's `stake-pool` market kind (`mtx:strategy@0.2.0`) needs a
profile; this is the proposal, and the benchmark's grader implements it
(`mtx_grader::stake`).

A stake pool takes SOL, stakes it with validators, and issues a liquid
staking token (an LST, such as JitoSOL or mSOL) that redeems for a growing
amount of SOL. Its **rate** is the SOL one token redeems for: the pool's SOL
under management over its token supply. It grows once an epoch, when the
pool takes in its validators' rewards, less its fee.

- A **deposit** (stake) pays SOL and receives the token at the rate,
  grossed up by the pool's deposit fee.
- An **instant withdrawal** returns the token for SOL at once, at the rate
  less a fee, from what the pool keeps liquid. That is limited: an SPL
  pool's reserve, or Marinade's liquidity pool, whose fee grows as it
  drains.
- A **delayed withdrawal** returns the token for SOL that unlocks after the
  current epoch: a stake account split off a validator (SPL), or a ticket
  (Marinade).

## Position

The position in a `stake-pool` instrument is the account's **holdings of
the pool's token**, in its base units (9 decimals for both named pools),
wherever the account holds it: every token account it owns for the mint. A
mandate's `max_position` and `min_position` bound that total. A delayed
withdrawal is no longer the token: it is SOL waiting (see Value).

The account's SOL is **native SOL**, the lamports in its wallet: a pool
takes and pays lamports, not wrapped SOL. Unlike the [spot](spot.md)
profile, which leaves native SOL out as fee money, a stake-pool account
counts it, fees included: staking spends the same SOL.

## Sides

A deposit is a **buy** of the token and an instant withdrawal a **sell**,
at the SOL per token the pool's own transaction moved: SOL into or out of
the pool's SOL account (an SPL pool's reserve stake; Marinade's reserve or
liquidity pool) over the token the account gained or gave. `sides` and the
price limits (in SOL per token) audit them. A delayed withdrawal is a sell
too, but at no price until it pays out; v0 does not audit it as a fill.

## Value

The token at the **pool's rate** times the SOL reference price
([spot](spot.md#value), Pyth SOL/USD), not at a market's price for it:
the rate is what a redemption returns and what lenders (Kamino's Scope
`SplStake` and `MsolStake` entries) price it at. Native SOL counts at the
reference price, and a delayed withdrawal counts at the SOL it will
return: a stake account the account may withdraw, at its lamports, and a
Marinade ticket, at its `lamports_amount`.

For an SPL pool the rate is `total_lamports / pool_token_supply` from the
pool account. For Marinade it is the state's `msol_price` (SOL per mSOL in
2^-32ths), the price its instant and delayed exits pay; its deposits mint
at its virtual staked SOL over its mSOL supply, which `msol_price` tracks.

## Epochs

The rate moves at an epoch boundary, when a crank (permissionless) takes
the validators' rewards in: an SPL pool's `UpdateValidatorListBalance` and
`UpdateStakePoolBalance`, Marinade's `update_active`. Until it runs, an
SPL pool refuses deposits and withdrawals (`StakeListAndPoolOutOfDate`).

## The benchmark's enforcer

`mtx_grader::stake` runs stake-pool sessions. The position is read from
the account's token accounts every step; the value is the pool's rate as
above, through `Market::value_usd`; fills come from the pools' own
transactions. A scenario's epoch boundaries move the clock into the next
epoch between two steps and run the pools' cranks before the bot's next
turn (see the venue profiles).

## Open questions (for Ludo)

- **The mark.** v0 values the token at the pool's rate. The alternative is
  what the account could get now: the instant exit's price for its size,
  or a pool's (an AMM's) bid. The rate is consistent with lenders and with
  redemption over an epoch; the executable bid is consistent with "what
  you can actually get" in a depeg. Recommendation: the rate, with the
  executable bid reported, and a mandate free to cap position size by
  depth.
- **Delayed withdrawals as fills.** Whether a delayed withdrawal should be
  a sell at the rate when it is ordered (it fixes the SOL it returns), so
  `sides` and price limits audit it.
- **Profile or venue.** Whether `stake-pool` stays its own kind or becomes
  spot with a venue-set price; the epoch and the delayed exit argue for its
  own.

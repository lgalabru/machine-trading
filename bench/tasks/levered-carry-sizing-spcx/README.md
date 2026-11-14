# solana-foundation/levered-carry-sizing-spcx

The agent's bot runs a levered funding carry on Phoenix's SPCX (SpaceX)
perpetual, which trades at a persistent discount to its index so that
Phoenix's funding pays the longs. It sizes the long to the index's
volatility state and to the risk desk's stop-out.

| | |
| --- | --- |
| Market | Phoenix SPCX perp, mainnet snapshot of 2026-10-29 |
| Sessions | 100 hidden per grade; 6 practice |
| Mandate | Long only, 700 SPCX cap (about 10× collateral); $1,000 loss limit, enforced by a priced stop-out |
| Bound | Hindsight on the book, plus the funding a position against every premium would collect |
| Reference | Sizes the long by the carry's yield over an online estimate of the book's variance, inside a stop distance, and goes flat when the discount's churn reads as stress. Hidden from the agent: it isn't described in the instruction and practice doesn't run it |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged). This task departs from
it in one way: a drawdown past the limit doesn't disqualify. The grader's
risk desk closes the position at the market and the session ends there
(`scoring.stop_out`, see `docs/scenario-format.md`).

## The skill

A good bot profits here because it sizes a funding carry to the market's
volatility state and to its distance from the stop-out: large while the
index is calm, small or flat once it turns stressed, before the crowd that
holds the discount flushes the book.

- **Wins:** pricing the carry with the session's funding parameters and
  cap; an online variance estimate against the session's own calm; reading
  a widening, churning discount as the crowd leaning in; a stop distance
  inside the loss limit; waiting for the discount to rebuild after a flush.
- **Loses:** the full cap from the start (stopped out in most stressed
  sessions); a constant moderate leverage (it bleeds in every flush and
  misses the calm carry); sizing by the discount alone (it widens into the
  flush); the mainnet funding parameters in the `/exchange` file.

## The market

- **The index** is drawn from SPCX's market calibration
  (`spcx.20261112.d1f48be2`) in regular hours: GARCH-t on one-minute
  blocks, published as a smoothed mark every 5 seconds. Two stated
  departures: SPCX's fitted session-level spread split into a calm (0.85×)
  and a stressed (1.7×) state, each with its own level draw, switching at
  the start of each minute (calm to stressed 8.5%, back 21.25%).
- **The discount** is drawn per session (0.3-1.2%), drifts toward its level
  and jitters; in stress it drifts toward 1.5× its level and churns 2.5×
  faster.
- **The flush**: at most once, odds 0.01%/s calm and 1.2%/s stressed; the
  book falls 1-3% in 2-10 s with the index, the discount closes, then
  rebuilds over 30-90 s.
- **Funding** is Phoenix's own program on the book's mark: interval 2-6 s,
  period 150-600 s, cap 0.6-1.6% of the price, set on chain per session.
- **The tape**: grader-run takers both ways every 0.75-2 s keep the mark
  fresh.

## Scoring

Valued at the scenario's book level plus unsettled funding, as in
`funding-carry-spcx`. A session whose value falls more than $1,000 below
its peak is closed by the risk desk (market orders of up to 100 SPCX, at
most 8 a step, through the book) and ends once flat; it is scored at what
the account is worth then. Going short, over 700, another market, a
refused call or ending liquidatable zero the grade.

## Baselines

Gate 0, on the calibration model (`hidden/gate0/`, USD per session, 600
held-out sessions, three seeds): max-lever -98 to -156, fixed 3× -106 to
-124, prem-scaled 78-87, reference 180-195, told 402-444, path oracle
472-509. Told makes 4.6-5.7× the best non-reading rung. On the engine (full gates, 2 sets of 50 fresh hidden sessions, USD):
reference +4,861 / +574, told +13,718 / +13,702, max-lever -16,697 / -13,753,
fixed -10,514 / -8,289, prem-scaled -7,423 / -3,223. Under Harbor the oracle
scores skill 1.0000 (reward 0.024: reference +$15,247 over 100 sessions); nop
and the cheaters score 0.

## Realism limits

- **Time is compressed.** Funding settles every few seconds over a period
  of minutes, and the volatility state switches at session timescale: both
  are stated departures so that a session minutes long holds the carry's
  economics and its regime risk.
- **Stated states.** SPCX's calibration fits one volatility state; the two
  states split its fitted session-level spread, as a stated departure.
- **The flush is a designed event**, not fitted: its odds and size are the
  family's.
- **One market, one snapshot.** Depth and fees are mainnet's on 2026-10-29.

## More

- [DESIGN.md](DESIGN.md): the design record, with Gate 0.
- [instruction.md](instruction.md): what the agent is told.

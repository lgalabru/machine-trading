# solana-foundation/funding-carry-sol

The agent's bot trades Phoenix's SOL perpetual for the funding its premium
pays: a small, slow basis, measured on mainnet, against the fees, the
index's moves, an arbitrageur that keeps the book on the index and the
bot's own price impact.

| | |
| --- | --- |
| Market | Phoenix SOL perp, mainnet snapshot of 2026-11-13 (`phoenix-perps-2026-11-13`, SOL at $79.54) |
| Sessions | 72 hidden per grade (36 with the premium's level under its measured mean, 36 at or over it); 6 practice |
| Mandate | 125 SOL position cap (about 1× collateral); $1,000 loss limit |
| Score | Scoring v2: 0 for doing nothing, 0.5 for the reference, 1 for the informed bot |
| Reference | Estimates the session's premium level from what it has seen and the stated prior, prices the funding over the rest of the session against the entry cost and the index's risk, sells the premium in 5 SOL slices up to 60% of the cap, and holds |
| Anchors | The reference (0.5); the informed bot (1) is told the premium's level, spread and half-life, funding's settings and the flow's and arbitrageur's pace, never the path or an order |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it prices the carry Phoenix will actually
pay on the session's clock, takes it only where it pays for what trading
costs, and reads how rich this session's basis is.

- **Wins:** the session's interval and period from the PerpAssetMap, not
  the `/exchange` file's hourly ones; the premium averaged, not read off a
  half-tick mid; the expected funding over the rest of the session against
  the fee and the walk; getting in slowly (or passively) so the book and
  the arbitrageur come back between slices; holding to the close, which
  costs nothing.
- **Loses:** shorting the cap at once (the walk through a thin book costs
  more than a week of funding); carrying a thin week (the funding is worth
  less than the fee); trading the premium back and forth; trading the
  index's wobbles without a view.

## The market

- **Funding** is Phoenix's own program, on the book's mark. Each session's
  interval is 3 or 4 seconds (mainnet's is 3,600), its period eight
  intervals (as on mainnet), its cap the snapshot's. A session plays 90 to
  160 intervals: four to seven days of funding in six to eight minutes.
- **The premium** has a level drawn per session (mean +1.43 bp, sd 0.67,
  from 90 days of Phoenix's SOL funding as five-day windows) and wanders
  0.84 bp around it, halving every 3.64 intervals. Longs pay shorts in
  nearly every session, as they did 92% of the measured hours.
- **The index** is SOL's fair value from its market calibration (market
  model v2), at its real pace.
- **Other traders**, from the actor library at the pace and sizes of
  Phoenix's SOL tape by kind of hours: noise takers, informed takers (about
  one order in eight, seeing the index a minute ahead), an arbitrageur that
  takes the book back to within 3.5 to 5 bp of the index within a slot or
  two, and the pinger's $3 prints.
- **Impact.** Every trade walks the book; the walk fades, halving every
  minute.

Every number, its source, and every departure from mainnet are in
[DESIGN.md](DESIGN.md).

## Scoring

Scoring v2, every session graded. The account is valued at the scenario's
book level (the index plus the premium), with funding accrued but not yet
settled. A position open at the close pays no fee. A session where more
than a tenth of the pinger's orders fail voids the grade.

## Baselines

Over 72 hidden sessions (six fresh secrets, v2 sessions, on Linux): the
reference made +$493 (per-session Sharpe 0.40), the informed bot +$850
(Sharpe 0.43); weighted as v2 weighs losses the reference is 0.555 of the
informed bot, and below it on every set of 12. On the fast gates' public
set the informed bot's loss-weighted PnL is +$14.45. Doing nothing makes
$0. The full ladder and `mtx gates funding-carry-sol` are still to run.

## Realism limits

- Funding's clock and the premium's run 900 to 1,200 times faster than
  mainnet's; the index's doesn't, so the price risk of a carry here is
  minutes of SOL, not days, and there is no spot leg to hedge it.
- The takers' sides are coin flips: the weekend's one-sided bursts (84%
  same side next) aren't modelled.
- The informed takers' horizon and hit rate are the library's defaults;
  only their share is fitted (to the tape's one-minute markout).
- The book is the snapshot's: thinner than the tape's large orders found
  it. The pinger prints about thirty times as often as on mainnet, to keep
  the mark fresh.
- The mark is the book's mid at its last trade, not Phoenix's mainnet blend
  of oracles and book (see `funding-carry-spcx`'s design record).

## More

[DESIGN.md](DESIGN.md) (why the task is built this way, every number with
its source) and [instruction.md](instruction.md) (the agent's contract).

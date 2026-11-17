# Design record: event-follow-through-tsla

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned. It is card B7 of the batch-2 cards
(`TASK_CARDS_BATCH2.md`); the departures from the card are listed last.

Status: Gate 0 passed on two fresh secrets (pooled); full build on that
reference and informed bot, frozen since Gate 0.

## Thesis

A good bot profits here because it reads which of the funds trading after a TSLA headline keeps leading the price, and positions with it before the verdict carries the jump on or takes part of it back.

- **Wins:** stay flat through the print; once the jump has landed, mark out each wallet's takes against the oracle, find the fund whose trades keep leading it, and take its side before the verdict.
- **Loses:** riding or fading every jump nets its costs; holding through the print is a coin flip; chasing the verdict after it prints is too late; following the market's net flow follows two mirrored funds and nets nothing.
- **Regimes:** the headline jumps 1-4% up or down with equal odds; a verdict 20-260 slots later carries it on, or takes back, 30-80% of the jump, with equal odds.
- **Mandate:** TSLA only, ±12 TSLA, a $600 loss limit (a breach is stopped out); a position left at the close is closed at the oracle less 5 bp.

## Expert research

A scheduled release moves a stock in two stages: the print, then the
reaction to what is behind it (the call, the guidance, the detail). Post-
announcement drift and partial reversals both happen, and which one comes is
what better-informed desks trade (Bernard and Thomas 1989 on drift; Chan
2003 on drift after news against reversal after moves without it). A desk
can't see the news behind the print, but it can see who trades: a fund whose
takes keep leading the price has information, one whose takes keep losing
doesn't (Kyle 1985; Easley, Kiefer and O'Hara on informed order arrival;
markouts are how desks score counterparties). The non-expert trades the
chart: it rides the jump, fades it, or chases the second move.

## How it differs from B2 (smart-money-decay-nvda)

Both read who is informed from wallets' records, so the difference is what
the record is read against and when it matters:

- **B7 reads markouts after a scheduled print.** The two funds' records are
  identical until the jump is done; the evidence starts at a known moment
  (the headline's window is stated), and it is the sign of each fund's
  markouts over the next tens of slots. The decision is one bet on the
  verdict's sign, worth most early, against a deadline the bot can't see
  (20 to 260 slots), with a cliff: flat earns nothing, the wrong fund's side
  loses the verdict.
- **B2 reads decay over the session.** Its wallets' edges fade at different
  times through an unscheduled session; the skill is noticing when a once
  informed wallet's record turns, and following and dropping wallets
  continuously.
- **B7's jump and verdict are event risk** B2 doesn't have: be flat through
  a print whose sign nobody knows, then act on information that arrives only
  after it.

## The central decision

After the jump: whose side to take into the verdict. Two funds trade the
same size at the same pace from the jump on, one each way; one of them knows
the verdict. Being flat earns nothing; taking the wrong fund's side loses
the verdict.

## Levers

| Lever | What the market does | What a good bot does | How an honest operator detects it |
| --- | --- | --- | --- |
| The headline | the level jumps 1-4% in 2-5 steps 3-8 slots apart, from a slot drawn in 300-600 | stays flat through it | `market.oracle` |
| The informed fund | 2-3 wallets that trade the level 30-90 slots ahead, past 2 bp, all session | follows its side into the verdict | trader accounts' positions (`getProgramAccounts`, then `getMultipleAccounts` each step); markouts of their takes |
| The contrary fund | as many wallets, the same pace and sizes; ahead of the level until the jump is done, then against it | fades its side, or ignores it | the same: its takes keep marking out negative after the jump |
| The verdict | one step, 30-80% of the jump, the jump's way or back | is positioned before it, out after | `market.oracle` |
| Chasers and noise | chasers take the way the book moved; noise either way | ignore them | their markouts are flat |

## No tells

- Every session seats the same actors: noise, chasers, the informed fund and
  the contrary fund, with the same wallet counts, pace and sizes. The roster
  the informed bot is told (the informed fund's wallets) never depends on
  the verdict.
- Before and through the jump both funds trade ahead of the level, so their
  records match until the jump is done.
- After it, each fund's flow is the other's mirror in expectation, so the
  market's net flow says nothing of the verdict.
- Follow-throughs and fades draw their share, delay and step alike; jumps go
  up and down with equal odds.

## What the informed bot is told

`{"roster": ["informed"]}`, resolved by the session to
`{"wallets": {"informed": [<authority>, ..]}}`: the informed fund's wallets,
an identity fact (`LATENT_FIELDS`' `*` roster). Not when the headline
prints, its way or size, the verdict's kind, slot or size, or how far ahead
either fund sees. It still has to watch the fund trade after the jump to
learn the verdict's way.

## Departures from the card

- **Two mirrored funds instead of one seated wallet whose role changes.**
  The card had the post-event wallet informed in follow-throughs and a decoy
  in fades, and told the bot which: that tells the session's draw (its
  aftermath), which the informed-bot boundary forbids. Here both funds sit in
  every session; the informed fund is told, and the verdict's way comes out
  of its trading in session.
- **A pause, then a one-step verdict, instead of a drift or retrace over
  100-500 slots.** TSLA's calibrated noise is under a basis point a slot, so
  a drift of 0.5-2% shows in the oracle within a few slots and a chart-reader
  catches most of it (with a three-step verdict, the chase rung made 0.9 of
  the informed bot on a 12-session development set). The verdict now lands in one step 20-260 slots after the
  jump, so only flow read before it pays.
- **The decoy turns contrary, not random.** A random-view decoy leaves the
  net flow leaning the informed fund's way. `Decoy.after = contrary` (new in
  the actor library) mirrors it.
## Gate 0

72 sessions from one fresh secret (`hidden/gate0/README.md`):

| Rung | Total PnL | Mean | Won | Weighted share of informed |
| --- | --- | --- | --- | --- |
| informed | 4277.7 | 59.41 | 72 | 1.00 |
| reference | 1725.4 | 23.96 | 30 | 0.37 |
| fade | 469.0 | 6.51 | 42 | -0.15 |
| hold | 249.2 | 3.46 | 39 | -0.39 |
| chase | -482.5 | -6.70 | 0 | -0.17 |
| net-flow | -1428.9 | -19.85 | 30 | -0.69 |
| ride | -1454.7 | -20.20 | 30 | -0.72 |

The informed bot makes 9.1x the best non-reading rung (`fade`), and doubles
it on 0.95 of bootstrapped sets; the ladder's order holds on 0.854 of them;
the reference is 0.37 of the informed bot weighted as v2 weighs losses, with
a per-session Sharpe of 0.54.

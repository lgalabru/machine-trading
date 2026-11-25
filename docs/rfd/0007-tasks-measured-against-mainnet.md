# RFD 0007: 0.15, tasks measured against mainnet

State: discussion
Authors: astra, for Ludo Galabru  ·  Started: 2026-11-24

## Problem

Since 2026-11-21 luthor has run mtx's strategies on mainnet in shadow, on
mtx-alpha. They lost. On 2026-11-24 every maker and scout together made
-$23 a day on paper after Phoenix's fees. The ten that earned made about
$1 a day each, all on markets where 96% to 100% of the prints are a
pinger's $3 lots. The same families scored well on the bench in 0.12 and
0.13: flash-crash bots made $15k to $27k over 72 sessions.

The bench didn't predict mainnet because its tasks guarantee the edge they
teach. The four Phoenix perp tasks differ from Phoenix in five ways, each
measured below:

1. **Every session has the event.** A flash-crash session has a shock or a
   fizzle in its 80 to 180 seconds, at disclosed odds. On mainnet a jump
   of 1.5% or more, with $2,000 of flow behind it, came in 16 distinct
   hours over 3 days, across 110 markets.
2. **The aftermath is the other way.** Flash-crash's shocks revert 65% of
   the time and extend 15%. Mainnet's jumps, 15 minutes on: 31% revert,
   45% extend.
3. **About 150 times the flow.** Market-making's noise takers send 0.8 to
   1.8 orders a second. SPCX's real takers sent 0.33 orders a minute in
   regular hours, 0.09 off hours, and nearly none on the weekend.
4. **Noise isn't noise.** The bench's noise takers know nothing and its
   informed ones trade only before jumps. On Phoenix the average taker is
   informed: 60 seconds on, the mid has moved its way by 3.4 bp on ANT,
   8 to 12 bp on NVDA and 1.3 bp on SOL. That is about what it paid to
   cross. A maker filled by the median taker earns nothing.
5. **No latency, no impact, no competition for the fill.** The world waits
   for the bot. The 0.13 changelog says scoring v2, the actor library,
   price impact and the transaction layer were opt-in and not used by
   these four tasks.

0.15 makes every task's market look like the market it names, measured
from a tape we recorded, and makes the bot compete for what it earns.

## The tape

luthor records every Phoenix trade the validator sees in `history.sqlite`
(`tape`: time, slot, symbol, taker's side, price, size; one row per maker
filled). `calibration/fitter/tape/study.py` reads an export of it and
writes `calibration/tape/phoenix-tape.<date>.<hash>.json`. The first is
`phoenix-tape.20261125.fc443f1e`: 543,438 rows, 3.06 days from
2026-11-21 23:45 to 2026-11-25 01:12 UTC, every one of the 110 markets. The tape itself is
kept outside git and cited by SHA-256 (`dc2b18d7…`), as the calibration
archives are.

Rules the study follows:

- **A taker order** is the fills in one slot on one side. The tape doesn't
  name the taker.
- **The pinger.** An order under $5 is the pinger's, and is left out of
  flow. It is still a price: it trades at the touch.
- **The price** is the mid of the last buyer's print, the ask, and the last
  seller's print, the bid, each under 5 minutes old. Print to print, a
  wide book's price bounces between bid and ask on every pinger print,
  which reads as jumps that revert and takers who lose.
- **Sessions** are US winter time in UTC: regular 14:30 to 21:00 on
  weekdays, weekend from Friday 22:00 to Sunday 23:00, else off.

### Flow, non-pinger taker orders

| Market | Session | Orders a minute | $ an hour | Median $ | p90 $ |
| --- | --- | --- | --- | --- | --- |
| SPCX | regular | 0.33 | 30,845 | 1,000 | 3,483 |
| SPCX | off | 0.09 | 6,599 | 918 | 3,287 |
| SPCX | weekend | 0.0007 | 11 | | |
| NVDA | regular | 0.99 | 290,408 | 1,000 | 9,898 |
| NVDA | weekend | 0.02 | 1,656 | 88 | 3,781 |
| ANT | regular | 11.6 | 503,293 | 100 | 1,001 |
| ANT | off | 7.0 | 187,270 | 130 | 1,001 |
| SOL | regular | 0.99 | 212,813 | 512 | 7,002 |
| BTC | regular | 4.7 | 1,030,963 | 601 | 5,989 |

SPCX's pinger is 89% of its orders. A $1,000 order recurs on many
equity perps: a second bot, which we count as flow.

### What takers know

The mid's move after a taker's order, in its direction, from the mid just
before it, and what it paid against that mid (bp, mean, 95% interval):

| Market | Order $ | Orders | Paid | 10 s | 60 s |
| --- | --- | --- | --- | --- | --- |
| ANT | 5-100 | 11,536 | 2.4 | +2.9 [2.7, 3.1] | +5.4 [5.0, 5.8] |
| ANT | 100-1k | 10,567 | 2.1 | +1.4 [1.2, 1.7] | +3.4 [2.9, 3.9] |
| ANT | 10k+ | 159 | 6.3 | +6.6 [3.8, 9.0] | +13.7 [7.2, 21.0] |
| NVDA | 100-1k | 498 | 8.9 | +3.6 [0.7, 7.0] | +8.1 [2.2, 15.6] |
| NVDA | 1k-10k | 543 | 8.6 | +3.7 [-0.7, 7.7] | +11.6 [3.2, 20.1] |
| SOL | 100-1k | 3,323 | 1.8 | +0.5 [0.3, 0.7] | +1.3 [0.8, 1.8] |
| SOL | 10k+ | 147 | 4.9 | +4.5 [2.2, 7.4] | +8.2 [1.7, 15.6] |
| BTC | 10k+ | 788 | 3.2 | +2.3 [1.8, 2.9] | +4.5 [3.4, 5.5] |
| SPCX | 1k-10k | 229 | -2.1 | -0.5 [-2.5, 1.6] | +0.8 [-3.1, 4.1] |

Takers pay about half the spread and get most or all of it back within a
minute; big ones get more. SPCX has too few orders to say.

### Jumps

A jump is a minute whose mid closes 1.5% or more from the last, with $2,000
or more of non-pinger flow in it. 93 on 110 markets in 3.06 days, in 16
distinct hours: they come in clusters, so think of 16 episodes, not 93.
70 were outside US regular hours.

| 15 min on | Revert (back half) | Stall | Extend (on half again) |
| --- | --- | --- | --- |
| all 93 | 29 (31%) | 22 (24%) | 42 (45%) |
| regular hours, 23 | 7 | 9 | 7 |
| off hours, 70 | 22 | 13 | 35 |
| 3% or more, 20 | 4 | 7 | 9 |

The bench's flash-crash: 65% revert, 20% stall, 15% extend.

## Sketch

One rule for every task: **a number that sets what a bot can earn comes
from the tape or a calibration file, and cites it.** Where a task departs
from the measured market to stay gradeable (a session can't wait an hour
for the event), the departure is stated in the task's instruction and
`DESIGN.md`, with the measured value beside it, and the scoring charges
for the bot's real opportunity, not the session's.

What each task becomes, to be settled as the work goes:

- **flash-crash-spcx.** The aftermath mix from the tape: extensions as
  common as reversions. Sessions where nothing happens, at a stated rate,
  so a bot that trades every wobble pays. Followers that reprice late, as
  on 2026-11-23, where the edge was. Direction is no longer a coin and
  fade is no longer the answer by construction.
- **market-making-spcx.** Takers from the actor library, scaled to the
  calibration's venue numbers, which the tape now fits: rates by session,
  sizes lognormal around $1,000. Every taker a little informed, as
  measured, not only before jumps. An arbitrageur that picks off quotes
  stale against the index, a competing maker at the measured spread, the
  transaction layer, price impact.
- **funding-carry-spcx.** The same takers and costs, funding from the
  calibration's venue section.
- **weekend-gap-spcx.** SPCX's weekend flow is nearly nothing on Phoenix
  (11 dollars an hour). What reads the reopen is the markets that trade
  through the weekend, not SPCX's own flow.
- **The three atomic tasks.** Competing searchers and liquidators at the
  rates mtx-alpha's scouts saw: 2 cycle gaps in 153,000 slots on ten pools,
  27 obligations under water in 3 days with 1 liquidated by someone else
  and 25 recovering by themselves.
- **Scoring v2 for all seven**, with an informed bot that knows the
  measured process, not the session's draws.

## Open questions

- **Which market a task names.** SPCX trades one taker order every three
  minutes in regular hours. A maker task there is a task about waiting.
  ANT trades 12 a minute with real, informed flow. Do we keep SPCX and
  stretch time, or move market-making to ANT (a new snapshot)? We find out
  by drawing both and seeing which ranks the ladder's rungs.
- **Three days.** One regime, one news day (2026-11-23), one weekend. The
  study reruns on luthor's seven-day window, and the jump mix should be
  refitted weekly until it stops moving.
- **The book.** luthor keeps no quotes, so depth and the competitors'
  quoting come from the calibration's defaults. luthor should record the
  watched markets' top of book and depth into `history.sqlite`.
- **Who the takers are.** Without taker addresses we can't tell one bot
  from many. The runtime reads transactions; it could keep the signer.

## Risks

- **A task nobody can win.** Realistic flow on SPCX may leave no edge to
  teach, and a task where `nop` is best ranks nothing. Each task must
  still separate its ladder's rungs, under `mtx gates`.
- **Fitting the bench to three days.** The point is the opposite of
  overfitting: the numbers say where the old ones were wrong by factors
  of 100, not which second matters.
- **Rewards drop and don't compare with 0.14.** They shouldn't: the old
  rewards measured a game.

## What we learned

- 2026-11-24: what the engine already has, and what it blocks (astra).
  The four perp families read no calibration: every number is a
  constant in `ShockFamily::spcx()`, `MakingFamily::spcx()`,
  `CarryFamily::spcx()` and `WeekendFamily::spcx()`. The SPCX
  calibration's venue numbers (`spcx.20261112.d1f48be2`: 1.463 trades a
  minute, median 0.021 SPCX) are the pinger's $3 prints, read from
  candles; its book is defaults. Three engine limits shape 0.15:
  - price impact is refused beside a competing maker or a closed
    period, so market-making and weekend-gap can't take it yet;
  - no perp actor sends through the transaction layer: only the lending
    liquidator and the pool actors attach to it;
  - the actor library's `scale` is filled by hand; nothing copies it
    from a calibration.

  So 0.15 adds new constructors beside the old ones (`ShockFamily` and
  `MakingFamily` first), built from the tape study, and leaves the old
  generators and their pins alone. Flash-crash and market-making are
  being built on branches of `release/0.15.0`.

- 2026-11-25: one market a task, where its edge is (Ludo, astra). SPCX
  trades one real taker order every three minutes in regular hours, so
  each perp task moves to the market where the tape shows what it
  teaches, and every one of them is a market luthor records:

  | Task | Market | Why |
  | --- | --- | --- |
  | `market-making-ant` | ANT | 11.6 real orders a minute in regular hours, informed takers |
  | `flash-crash-nvda` | NVDA | 9 of the 93 jumps, 2026-11-23's leader, a calibration with two states |
  | `funding-carry-sol` | SOL | trades through the weekend (2.6 orders a minute), led by the exchanges |
  | `weekend-gap-spcx` | SPCX | a closed-hours equity with a fitted open gap; its own weekend flow is nearly nil |

  ANT is the listed equity perp (Phoenix asset 110), not the isolated
  pre-IPO `ANTHROPIC`, and is newer than `phoenix-perps-2026-11-13`: it
  takes a new snapshot. The old task directories stay as they are, so
  0.14's pins don't move. luthor keeps every market's quotes from
  2026-11-25 (luthor #17): top-of-book half-spreads over the first 106
  minutes were SPCX 0.4 bp, SOL 0.73, NVDA 2.1, ANT 2.7.

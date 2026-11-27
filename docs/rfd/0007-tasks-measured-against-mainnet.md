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

- 2026-11-25: on ANT's real flow, no maker earns (astra, with the
  market-making agent). With every number at its measured value, over 48
  graded sessions on Linux the informed bot made +$2 and -$1 a set, and
  the reference lost $8 to $10. The loss is the informed takers: their
  60 s markout (4.4 bp) is larger than what they pay to cross (2.7 bp),
  so a maker breaks even only by dodging nine informed fills in ten.
  That is what luthor's paper makers see on liquid books. A task has to
  rank skill, so the owner chose one departure: informed takers trade
  on the index's recent move, which leads Phoenix's book, rather than
  on a private view of the future, and reading the index is the skill.
  It fits the tape only because ANT moves about three times as much as
  SPCX: per-second volatility from the tape's mid, 60 s returns, US
  regular hours, is ANT 4.45 bp, NVDA 2.98, SOL 1.60 and SPCX 1.55
  (SPCX's calibration says 1.55, which checks the method).
- 2026-11-25: grading moved to Linux. Two surfnets at once kill each
  other on macOS, and the gates need Linux. Every 0.15 grade runs on
  mtx-bench-2 in `rust:1.97.1-bookworm` containers. `weekend-gap-spcx`
  is merged; its 6-set ladder puts the reference at 0.199 of the
  informed bot, loss-weighted, with a Sharpe of 0.21, both just under
  the band, so its reference is being strengthened.
  `flash-crash-nvda`'s reference reads net taker flow off the book's
  drift from the index and lands at 0.57 to 0.63 of the informed bot
  over three hidden draws (Sharpe 0.72 to 0.85).

- 2026-11-25: market-making leaves 0.15 (Ludo). It failed on both
  measured markets, in opposite ways, and each failure is what mainnet
  shows. On ANT, busy and informed, nobody earns: with the owner's
  public-signal departure and ANT's measured volatility, the informed
  bot made +$0.86 and the reference -$6.74 over 48 sessions; quotes a
  move leaves behind are picked off whenever the arbitrageur beats the
  bot's two-slot step, and the touch (2.7 bp) is narrower than one
  index step (3 to 10 bp). On CBRS, thin and wide, everybody earns, so
  skill doesn't separate: over 48 sessions the informed bot made $184,
  the reference $106, a naive quoter $104 and a skew quoter $99. CBRS's
  takers lose 7.6 bp a minute after they trade, so any quote inside the
  competitor earns; racing a reactive competitor to its floor gives the
  width away (reference +$6, informed -$66). One number in the ANT
  version was never measured: the arbitrageur's speed, 1 to 2 slots,
  borrowed from SOL's lead behind the exchanges. luthor will record
  ANT's index against Phoenix's book at slot resolution, and
  `market-making-ant` comes back in 0.16 on the measured lead.
  `market-making-spcx` stays at 0.14.0 in the dataset meanwhile.
- 2026-11-25: weekend-gap's positioners know more. Its informed bot
  couldn't stay ahead of a reference that reads late wallets, because
  the reopen's measured residual (0.50%) swamps what positioners right
  80% of the time know. The positioners were already a stated
  departure, so their hit rate goes to 95% and every measured number
  stays. The reference's trust is derived, (2 × 0.95 + 2 × 0.5) / 4 =
  72.5%, not tuned. Over 48 sessions: reference 0.373 of the informed
  bot, Sharpe 0.31 and 0.62.
- 2026-11-25: funding-carry-sol's reference prices the carry. It
  starts from the funding study's prior on SOL's premium (mean 1.434
  bp, sd 0.665, half-life 3.64 intervals), updates the session's level
  as it watches, and holds while the funding left pays for getting in
  and the index's risk. Over 72 fresh sessions: 0.625 of the informed
  bot, Sharpe 0.79 (was 0.20).
- 2026-11-25: the atomic tasks against mtx-alpha's scouts (Claude). luthor
  runs two scouts on our validator, on paper. Both read the chain a slot
  late: a write is read one slot after it lands (p50 and p99 1 slot; a
  pool is read and quoted in 0.67 ms at the median).
  - **The arbitrage scout** watches `backrun-thin-pools`' ten pools, both
    SOL/USDC pools among them. From 2026-11-23 21:47 to 2026-11-25 06:10
    UTC (about 32 hours, 540,000 slots at 0.215 s; the node's watches
    were down from 06:10 to 22:47), 14 cycles paid 0.001 SOL or more
    after fees, in 13 slots (`arb.jsonl`):

    | Cycles | Gaps | At their open | Behind a move of | Still open a slot on |
    | --- | --- | --- | --- | --- |
    | SOL, USDC, SOL | 7 | 0.002 to 0.05 SOL on 22 to 141 SOL; one 0.61 SOL on 377 | 10 to 20 bp; 48.5 bp, a sale into Raydium | 2, 0.39 SOL on paper |
    | through GEOD | 7 | 0.002 to 0.04 SOL on 1 to 5 SOL | 42 to 287 bp | 4, 0.012 SOL |
    | through JTO | 0 | closest: 52 bp short of paying | | |

    Every gap was gone within three slots. The scout can't see a gap
    opened and closed inside one block, which is where the research's
    backruns land (23 of 26 SOL/USDC, 77 of 106 cycles), so these counts
    are what the in-block race left, not the triggers. One thing it does
    show: a backrun through a cycle leaves it at about 0 bp, and the JTO
    cycles never came within 52 bp of paying at any slot's end, so no JTO
    trigger was closed through these pools in those 32 hours (one closed
    against another venue wouldn't show). The task's reference, in
    shadow on the same pools since 2026-11-24 03:29, put orders on its
    sheet in two steps (2026-11-24 14:40 and 17:04).
  - **The liquidation scout** follows the 1,965 main-market obligations
    that had $1,000 of debt or more and stood within 25% of their limit at
    a scan on 2026-11-24 ($210M of debt), with SOL read upstream at the
    Orca SOL/USDC pool. From 2026-11-24 04:55 to 2026-11-25 06:10 UTC (25
    hours) SOL drifted from 71.8 to 68.8, -4.2%, no hour over 2%. Of the
    43 times an obligation went under water (`liq.jsonl`), 40 were two
    obligations ($2,750 and $17,539 of debt) flickering across 1, on
    which the scout prices no repay: nobody liquidated them. The other
    three were two obligations a liquidator could take. One would repay
    $2,363 and keep $11.82: under water upstream (0.99999) while Scope
    read 1.0017, it was liquidated by someone else within two slots. The
    other would repay $88 and keep $0.44: under water upstream only
    (Scope 1.0067), twice, it recovered after 87 slots and after 2.
    Seventeen more have stood under water at Scope since the first look
    (stuck, not followed); 25 sit within 5% of their limit upstream, one
    within 1%. The sketch's "25 recovering by themselves" counted these
    episodes: of the two obligations anyone could liquidate, one
    recovered.

  Against the week-long study and the tasks:

  | Task | Scouts, about a calm day | Research, a week with an 11% fall | A task session |
  | --- | --- | --- | --- |
  | `backrun-sol-usdc` | one chunk-sized gap in 32 hours (0.61 SOL behind 48.5 bp), six of 0.002 to 0.05 SOL | 21 backruns on the pair (0.10 to 4.97 SOL, gaps of 32 to 138 bp); in a burst, 20 gaps in 17 minutes | 88% of sessions inside a burst: a chunk every 30 to 200 slots, gaps of about 30 to 60 bp, cycles of 0.33 to 1.93 SOL |
  | `backrun-thin-pools` | seven gaps through GEOD (at most 0.04 SOL), none through JTO, in 32 hours | 106 multi-token cycles over all of Solana's pools (50.9 SOL); triggers moved their pool a median 545 bp | about three triggers in 400 to 800 slots, 55% on the JTO pools, moves of median 545 bp cut where the best cycle reaches about 2 SOL |
  | `liquidation-flash` | two liquidatable in 25 hours (repaying $2,363 and $88): one taken by a rival within two slots, one recovered | 5,290 on the main market in nine days; 2,521 (every market) on the day SOL fell 11%, under 60 on calm days; above $1k, 93% in the update's slot | 4 to 14 borrowers, every one within 0.01% to 4% of its limit, legs of 0.2% to 4.5% over 30 to 150 slots: 11 liquidations a session on its eight practice sessions (reference and rivals, 0 to 36) |

  Per event, what each task draws agrees with the scouts where they can
  see: a chunk opens what the research's chunks opened, every gap is
  gone within three slots, and the one contested liquidation went within
  two. Those numbers come from the week's study, and stay. The tasks
  depart in rate: each session is an event window (a burst, a run of
  triggers, a drawdown) that mainnet holds for minutes a week. And
  `backrun-thin-pools`' triggers are the week's largest over all of
  Solana's pools, put on ten pools where the scouts saw nothing over
  0.04 SOL through a thin pool and no JTO cycle at all. None of the
  three is recalibrated: one calm day, a scout blind to backruns inside a
  block, and two obligations a liquidator could take don't measure a
  trigger rate or a size distribution, and fitting the families to them
  would be this RFD's second risk. Each task states its departure in its
  instruction with these values beside it. What would measure them: a
  tape of the ten pools' transactions, as luthor keeps Phoenix's (who
  closed each gap, inside the block or not, and what each trigger moved),
  and the liquidation scout through a drawdown day.

- 2026-11-26: market-making stays in 0.15 (Ludo), on ANT, once the
  arbitrageur's speed is measured. mtx #156 tells a `touch` event (the
  index, and the best bid and ask with their sizes) every slot either
  changes, for ANT, NVDA, SOL and SPCX; luthor #18 keeps them and `luthor
  lead` reads how far the index leads the book and how fast a stale
  quote is taken. Both run on mtx-alpha from 04:20 UTC. mtx #157 retries
  a watch that couldn't start: a validator restart at 06:10 UTC on
  2026-11-25 had left the markets, pools and obligations unwatched for 16
  hours, which is a hole in the quotes this RFD's book study reads.

- 2026-11-27: funding-carry-sol's full ladder on the 0.15 release tree
  completed four fresh hidden sets of 24 sessions. Blind-short beats the
  reference on all four. Aggregate loss-weighted PnL is $979.09 for
  blind-short, $645.30 for the reference and $1,107.91 for informed.
  Informed earns 1.13 times blind-short, below the signal gate's 2 times.
  The reference's strength band passes at 0.58 of informed, with
  per-session Sharpe 0.53. The fatal-crash probe reports `no_power`:
  not every set's grade went fatal. Blind-short also exceeds the
  zero-edge and lucky-run probe limits. A larger reference position
  alone would not repair the signal gate. The family's mostly positive
  basis rewards a constant short under its compressed funding clock.
  Release scope or a task redesign remains open; no thresholds changed.
  The reference replay passes, with seven identical reports over three
  runs of six sessions. Evidence is on `mtx-bench-2`, container
  `mtx015b`, in `/work/fc-full-out/gates.json` and
  `/work/fc-full-out/ladder/funding-carry-sol/summary.json`. The wrapper's
  image checks failed in that container, which lacks Docker and Harbor
  and retains a 0.14 image tag. Those failures do not establish the
  status of the separate 0.15 Harbor runs on the host.

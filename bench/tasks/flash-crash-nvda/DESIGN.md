# Design record: flash-crash-nvda

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned.

The task is `flash-crash-spcx`'s successor, moved in 0.15.0 to NVDA, the
market where the tape's shocks were: 9 of its 93 jumps, among them
2026-11-23's leader, +4% in a minute and +16% in half an hour. Its history
before 0.15.0 is flash-crash-spcx's (Generations, below), and
flash-crash-spcx stays as 0.14.0 shipped it so its pins don't move.

## Thesis (0.15.0)

Until 0.14.0 the task's shocks reverted 65% of the time and extended 15%,
so fading them paid by construction, and its bots made $15k to $27k over
72 sessions. On mainnet the same family of strategies lost money (RFD
0007). The tape luthor recorded on mainnet says why: a jump of 1.5% or
more went on by half of itself again 45% of the time fifteen minutes on,
and gave half of itself back 31%. 0.15.0 plays the market the tape
measured, on NVDA, with the takers NVDA has and the impact a trade has, and scores
by v2: 0 doing nothing, 0.5 the reference, 1 an informed bot told who
trades on information.

## What the tape measured

The study is `phoenix-tape.20261125.fc443f1e`
(`calibration/tape/`, written by `calibration/fitter/tape/study.py` from
luthor's `history.sqlite`, SHA-256 `dc2b18d7…`): every Phoenix trade from
2026-11-21 23:45 to 2026-11-25 01:12 UTC, 543,438 rows, 110 markets. A taker
order is the fills of one slot on one side; orders under $5 (the pinger)
are left out of flow. Every number the family uses, and where it departs:

| Lever | The tape | The family (`ShockFamily::nvda()`) |
| --- | --- | --- |
| Shock's aftermath, 15 min on | 93 jumps: revert 29 (31%), stall 22 (24%), extend 42 (45%); at 5 min 29/27/44%, at 30 min 28/16/56% | revert 0.31, stall 0.24, extend 0.45 |
| What the classes mean | revert: back half or more; stall: less than half either way; extend: on half again or more | revert gives back 0.5 to 1.0 of the shock; stall moves −0.45 to +0.45 of it; extend goes on 0.5 to 1.0 of it, within 0.75 to 6% of the start (the upper bounds are defaults) |
| Leg size | jumps (a 1-minute move of 1.5% or more, $2,000 of flow behind it): smallest 1.5%, median 2.0%, p90 4.2%, largest 9.5% | Pareto, exponent 2.0 (maximum likelihood on the 93: 2.03), truncated to 1.5 to 9.5%: median 2.1%, p90 4.3% |
| Legs a shock | 93 jumps in 16 distinct hours: they cluster | one, then another with odds 0.35, up to three (kept from 0.14.0: not measured per shock) |
| Small moves | 490 moves of 0.5 to 1.5% in 50 distinct hours, 5 min on: revert 26%, stall 37%, extend 38% (from the study's run as briefed; not yet in the committed file, which keeps only jumps) | half the quiet stretches carry one, 0.5 to 1.5%, held 1.5 to 10 s, then 26% give back 0.5 to 1.0, 37% −0.45 to +0.45, 37% go on 0.5 to 1.0 again (before: every fizzle gave back 40 to 100%) |
| Direction | 57 of 93 went up, on one news day (2026-11-23) | **stated departure:** 50/50 |
| How often | 93 jumps in 3.06 days over 110 markets, in 16 distinct hours; 70 outside US regular hours | **stated departure:** three in four sessions of 80 to 180 s carry a shock; a quarter are quiet |
| When the aftermath lands | measured at 15 minutes | **stated departure:** it lands 8 to 35 s after the last leg, inside the session |
| NVDA's takers | regular hours: 0.99 orders a minute ($290k an hour), median $1,000, p90 $9,900, log-sd 2.05 (off hours 0.29 a minute) | noise at 0.99 a minute of 250 ms slots, median 6.25 NVDA ($1,000 at the session's $160), sigma 2.05 (the library counts 0.4 s slots: the rate is written ×0.625); regular hours, where NVDA's flow is |
| Flow around a move | NVDA's 9 jumps' minutes carried a median $20,500 (all 93: $8,569) | informed takers and chasers each send about 10 orders of $1,000 in their 40-slot window (about 25 times NVDA's pace while active) |
| Who takes | NVDA's takers are the most informed measured: $1k–10k orders paid 8.6 bp to cross and the mid moved 11.6 bp their way in 60 s (95%: 3.2 to 20.1); on liquid markets generally +1.3 (SOL) to +12 bp | informed takers see the path 40 slots (10 s) ahead, take its way past 40 bp, right 85% of the time one view at a time (the library's default); chasers take after a 1% move in 40 slots |
| Impact | NVDA's orders' worst fill against the mid: $1k–10k median 6.9 bp, p90 31.9 bp; $10k+ median 9.4 bp, p90 51.5 bp | a $11,200 take (70 NVDA) walked the snapshot's book 12 to 15 bp on a graded session; impact keeps the book where trades leave it, probe 6.25 NVDA (the median order), at most 50 bp |
| Impact's persistence | not measured (luthor keeps no book) | **default:** the NVDA calibration's (`nvda.20261113.903d1120`, `book.impact`): half permanent, the rest halving every 30 s (120 slots) |
| Wobble | NVDA's calibration: 0.83 bp a second in regular hours, its stressed state 1.34 times that | a tick about every 2 s moves at most 0.03% (sd 1.7 bp); at most about 0.06% from the level (SPCX's task: 0.12%, 0.25%) |
| Book | luthor's one minute of NVDA quotes: half-spread about 2.0 bp | the snapshot's book: 159.99 / 160.01 at the start, about 0.6 bp a side |
| Cap | — | 62 NVDA, $9,920 at $160: SPCX's 70 at $143 in dollars; a 9.5% leg against a full position costs $942, inside the $1,000 limit |
| Horizon and lookback | NVDA's leader on 2026-11-23: followers repriced 4 to 88 s late (luthor RFD 0003) | **default:** 10 s |

The brief that moved the task to NVDA quotes NVDA's impact as "$1k–10k
median 2.5 bp, p90 24 bp; $10k+ median 4, p90 34" and its jumps' aftermath
from the same study; the committed study file says the numbers in the
table (6.9/31.9 and 9.4/51.5). The family cites the file; `max_bps` 50 is
inside both, and the snapshot's walk of 12 to 15 bp for $11k sits between
the medians and the p90s of either.

## Expert research

- **Why it's a real expert task.** A jump on a thin perp is information
  arriving: someone knows something, and the price goes on (momentum
  traders' "time-series momentum" at short horizons; on 2026-11-23 the
  shock that paid was NVDA's +1,585 bp that held 30 minutes, with the
  followers repricing late). Some jumps are forced flow that reverts
  (Kirilenko et al., *The Flash Crash*, 2017). A desk that trades shocks
  has to tell which, size for the ones that turn, and leave alone the
  stretches where nothing is happening.
- **The expert's method.** Wait for a move to be unmistakable, above the
  noise and the small moves, then trade with it at a size the loss limit
  survives, with a stop where the moves that turn usually reach, and hold
  the ones that keep going. Watch the tape: whoever is taking ahead of a
  move, and whether their record says they knew.
- **Where non-experts go wrong.** Fading every shock (45% keep going,
  half of them by as much again); trading every wobble (a quarter of the
  sessions have nothing in them, and every round trip pays fees and
  impact); entering on the first small move; holding a position with no
  stop; betting a direction at the open.

## The central decision

When, which way and how much to trade a move whose aftermath is unknown,
in a market where the bot's own trades move the price.

| Option | Why it looks defensible | Why it isn't |
| --- | --- | --- |
| Fade the shock | 0.14.0's answer; forced flow reverts | 45% extend, 31% revert |
| Ride every move | Most moves keep going | Small moves and wobble cost a round trip each, and a quarter of the sessions are quiet |
| Ride without a stop | Extensions pay | A revert gives back the whole leg |
| Bet at the open | Shocks are coming | Crashes and squeezes are 50/50, and a quarter of sessions have none |
| Follow the net flow | The takers are informed | Chasers and noise trade too: only each wallet's record tells who knew |

**The calculation chain:** read the mark each turn → tell a shock from
the wobble and a small move (size, a single step) → take the move's
direction → full size, a stop that grows with the move → hold what keeps
going. Above it: read the takers' wallets on chain, and learn which
take ahead of the moves.

**Nothing warns the agent.** A bot that trades every move gets fills, and
pays for them only in the valuation.

## Variants

| Session | Best response |
| --- | --- |
| quiet (25%) | stay flat, or trade only what the informed flow says |
| shock, extend (~34%) | ride it and hold |
| shock, revert (~23%) | ride the leg if in before it, stop out, or flip with the informed |
| shock, stall (~18%) | ride the leg, hold or cut: little either way |
| ends mid-shock | hold what the move gave |

| Habit | Fails in |
| --- | --- |
| fade the shock | extensions, the commonest aftermath |
| trade every move | quiet sessions, small moves |
| no stop | reversions |
| bet at the open | half the shocks, and every quiet session |

## Levers

| Lever | What the market does | What a good bot does | How an honest trader sees it |
| --- | --- | --- | --- |
| aftermath mix | extend 45%, revert 31%, stall 24% | rides, with a stop | the odds are in the instruction |
| quiet sessions (25%) | nothing happens | stays flat | wobble and small moves only |
| legs 1.5–9.5%, Pareto | most legs about 2% | a threshold above the small moves | a move that holds past the wobble |
| small moves 0.5–1.5% | most don't give back | doesn't take them for shocks | — |
| gaps (70% land at once) | no ramp to read the size from | doesn't extrapolate the first step | — |
| informed takers | trade 10 s ahead of every move | read which wallets are right | their takes on chain |
| chasers | take after a 1% move | expects the book to have moved before it gets there | their takes on chain |
| impact | the book stays where trades leave it | sizes in slices, doesn't churn | the book |
| duration drawn first, path cut at the end | the end slot dates nothing | holds to the end | — |

## No tells

- Every session can be quiet, fizzle, wobble, gap or ramp; only the draws
  differ. Whether a session is quiet is drawn after its direction and
  aftermath, which a quiet session draws and never plays, so its draws
  look like any other's.
- The session's length is drawn before, and independently of, its path.
- The generator stratifies a set by quiet, direction and aftermath, in
  proportion to their odds, and that's disclosed.
- The actors' wallets come from the session's keyring (the grade's
  secret), and the order they onboard in is drawn: an address says
  nothing until its record does.

## The anchors

- **The reference (0.5)** rides a confirmed move with a trailing stop: a
  1.2% displacement from a 30-step calm mean, or a single 1.9% step,
  starts a ride at the full cap; it stops out once the mark gives back
  1.5% or 30% of the move from its extreme, whichever is more; it halts
  past a $600 drawdown. Tuned on 600 draws of the family on SPCX with a
  simulator of the path (0.985 correlated with graded PnL on 72 sessions),
  then checked on 1,200 draws of the NVDA family (62 NVDA, 8 bp a side):
  $130 and $137 a session, Sharpe 0.40 and 0.42, profitable in half,
  deepest drawdown $787. flash-crash-spcx's 0.14.0 reference (ride, then
  fade) makes −$35 and −$47 a session on the same draws, `momentum` $58 to
  $61, the dip-buyer −$42 to −$56 (these three at a 70-NVDA cap).
- **The informed bot (1)** is told the informed takers' wallets
  (`roster: ["informed"]`, resolved to their authorities as the session
  starts) and the process (`horizon_slots`, `hit_rate`, `quiet_odds`,
  `aftermath_odds`, `impact`), never the session's draws. It reads each
  informed wallet's position every step (`getMultipleAccounts` on their
  trader accounts and Phoenix's trader arenas), takes each wallet's flow
  over the last horizon as its view, and holds the full cap the way the
  views' majority points, keeping its position while they are silent. Same
  slices, step limit and halt as the reference (`rider.rs`, one file).
- **Why direction, aftermath and quiet are not in the latent.** They are
  the session's realised draws (docs/gates.md: a shock's direction, legs,
  aftermath, size and timing are withheld). What the informed bot knows is
  who knows, which a reader can learn from the chain.

## Fairness

- Every correct path runs through standard RPC and `mtx-kit`: the
  reference and the informed bot do. The informed takers' trades are
  ordinary Phoenix transactions any bot can read.
- The instruction's numbers are the family's: every range, odds and pace
  above, the impact, and each stated departure with the measured value
  beside it.
- Readings written out: the valuation and the drawdown are at the
  scenario's price, not where trades pushed the book.
- **Penalty twins:** a position at the cap passes, one lot over it is a
  breach; a send outside the bot's turn is refused, not a violation.

## Failure modes, priced

| Failure mode | How it's charged |
| --- | --- |
| fades a shock that extends | the extension × the position |
| rides a small move | the round trip, fees and impact |
| trades a quiet session | the round trips |
| holds a revert | the leg it gives back |
| breach (cap, market, drawdown, refused call, liquidatable) | v2's breach: at least the loss limit ×1.5 for the session, and 1.25 × the breach share off the score |

## Predictions

The reference scores 0.5 by construction. Bots that read only the mark land
near it; a bot that reads the takers' records can pass it. The frozen
exploit bots (0.10.0 and 0.12.0 faders) should lose money on these odds.

## Calibration

**The strength band fails on NVDA as built: the reference is about 0.15
of the informed bot.** Measured on a Mac under heavy shared load, where
about half of the grades died to surfnet port races (an infrastructure
fault, not the task's: see `crates/grader/src/snapshot.rs`, "picks its
ports by binding and releasing them").

| Set | reference PnL | weighted | Sharpe | informed PnL | weighted | Sharpe | ratio (band 0.2–0.7) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| hidden draw, 72 (local secret, the verifier's flags) | $6,534 | $4,757 | 0.28 | 30 of 72 graded before the run died: $11,394 | $11,217 | 0.92 | 0.15 on those 30 |
| six 12-session practice draws (seeds 501–506, `mtx calibrate --practice --v2-sessions`, both anchors) | $6,343 | $4,881 | 0.33 | $33,706 | $33,487 | 1.09 | 0.146 |

The reference makes $88 to $91 a session where the path simulator says
$130 to $137: on NVDA the chasers' ten orders after a move and the
reference's own $10k entry walk the book before and as it rides, which
the simulator doesn't price. The informed bot, reading about ten informed
orders before every move, makes $468 a session (88% profitable).

On SPCX's numbers (the first build of 0.15.0, before the move to NVDA)
the same anchors gave reference $11,156 ($9,571 weighted, Sharpe 0.46) and
informed $31,522 ($30,898): 0.31, in the band. The difference is NVDA's
flow: its jumps carried $20,500 in their minute against $8,569 pooled,
so the informed bot sees about ten informed orders a move, not four.

Not changed yet, by the band's rule (change the reference or the market,
never the band); the levers, in the order we'd try them:

1. **The reference reads the tape** as an expert does: net taker flow over
   the last horizon, every wallet together (the "careful blind" of
   `docs/actors.md`), entering ahead of a move when it leans one way. It
   can't tell informed from chasers, so it should land between today's
   reference and the informed bot.
2. **The informed share of a move's flow**, half today (a default, not
   measured): a quarter informed and three quarters chasers keeps NVDA's
   measured total and weakens the informed bot's signal.
3. **The informed takers' horizon** (10 s, a default): the tape's NVDA
   takers are right by 8 to 12 bp a minute on, not by a whole jump.

- **Practice** (seed 20261124, 6 sessions): reference −$311.87, informed
  $3,317.95.
- **Not run here:** the full ladder, the probe bands, `mtx gates` (fast or
  full: the fast ladder's builds and 12-session sets didn't finish under
  the machine's load), and the anchor digest (Linux).

## Generations

| Version | What agents did | What changed |
| --- | --- | --- |
| 0.1.0 | three rebounding crashes | — |
| 0.2.0 | buying every crash paid | stalls and slides; 12 stratified hidden sessions; a 100 SOL cap |
| 0.3.0 | beat the reference 5× | a stronger reference: tranches, a slide stop |
| 0.4.0 | shorted the open: 2.4× with no skill | crashes and squeezes 50/50; a hindsight bound; a drawdown limit |
| 0.5.0 | read equal steps and a fixed tail: 3.5–3.9× | uneven steps and gaps, independent legs, one range for pauses and holds, a random tail, wobble; SPCX |
| 0.6.0 | rode legs that always ran ≥ 3% in steps, dated the aftermath from the end: ~6×, profitable in every session | — (mandate only) |
| 0.8.0 | — | legs 0.6–8% log-uniform, 70% gaps, fizzles, duration drawn first; 36 hidden sessions; exploit checks |
| 0.8.0 → 0.9.0 | frozen exploit bots made 3–6× the reference on fresh sets; the old reference (two fading tranches) made $525–$759 on 36 sessions, too little to rank the ladder's rungs reliably | a lockstep world; hidden sets drawn per batch from a secret; the reference graded alongside; `skill`; the ladder with a regime oracle; a new reference that rides each move and fades it once it stops ($647–$2,777 on the same sets) |
| 0.10.0 | faded fizzles that always came all the way back, and rode moves cut into equal steps: 2.3–3.1×, profitable in up to 86% of sessions | 0.11.0: at-once moves past ~3.8% land in parts of unequal random sizes; fizzles give back 40–100% (fading them, near riskless, was most of the edge); renamed `flash-crash-spcx`; the 0.10.0 bots join the exploit checks |
| 0.15.0 | 0.12.0–0.14.0's flash-crash bots made $15k–27k over 72 sessions faded on the bench; on mainnet the same families lost (RFD 0007) | the market measured from mainnet's Phoenix tape (`phoenix-tape.20261125.fc443f1e`): aftermath revert 31% / stall 24% / extend 45%, Pareto legs, small moves that mostly don't give back, a quarter of the sessions quiet, SPCX's takers (noise, informed, chasers) and price impact; scoring v2 with an informed bot told the informed takers' wallets; a reference that rides confirmed moves with a trailing stop; the regime oracle dropped; moved to NVDA as flash-crash-nvda (flash-crash-spcx stays 0.14.0) |

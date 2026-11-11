# Design record: funding-carry-spcx

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned. It answers the "new task" issue `phoenix-funding-squeeze`
(funding against the crowded side: harvest the carry, or get out).

## Expert research

- **Why it's a real expert task.** A perp's premium over its index, and the
  funding that pays the side against it, is the core carry trade of crypto
  derivatives desks. He, Manela, Ross and von Wachter, *Fundamentals of
  Perpetual Futures* (2022, arXiv:2212.06888): perp-spot gaps are large,
  persistent and mean-reverting, and a funding-collecting position against
  the gap earns a high Sharpe. Ackerer, Hugonnier and Jermann, *Perpetual
  Futures Pricing* (NBER w32936, 2023; Mathematical Finance 2024): funding
  is what anchors the perp to its index, and the premium a no-arbitrage
  band allows. Schmeling, Schrimpf and Todorov, *Crypto Carry* (BIS Working
  Paper 1087, 2023): carry is high when leveraged, trend-chasing demand is,
  and it crashes when that demand turns. Christin, Routledge, Soska and
  Zetlin-Jones, *The Crypto Carry Trade* (CMU, 2023): funding-based carry on
  perps. The other side of the trade is price discovery: Alexander, Choi,
  Park and Sohn, *BitMEX Bitcoin Derivatives: Price Discovery, Informational
  Efficiency, and Hedging Effectiveness* (J. Futures Markets 40(1), 2020):
  the perp leads spot. A premium the perp opened because it knows first
  closes by spot following, not by the perp coming back. Hasbrouck,
  *Measuring the Information Content of Stock Trades* (J. Finance 46(1),
  1991) and Kyle (1985): informed flow moves the price permanently,
  liquidity demand transiently. Shleifer and Vishny, *The Limits of
  Arbitrage* (J. Finance 52(1), 1997): a convergence trade's risk is the gap
  widening or the fundamental moving before it closes. Phoenix's own funding
  (the `phoenix-rise` SDK's `math::funding`): the accumulator tracks
  `∑ (mark − index) · dt`, an interval's funding is it over the funding
  period, clamped to the market's cap per interval, and it settles into
  the trader's collateral on its next trade.
- **The expert's method.** Price the carry the venue will actually pay:
  the premium on the venue's mark (here, its book), times the time held over
  the funding period, capped per interval; plus the premium's convergence
  if it closes by the perp coming back. Then ask what closes the gap.
  Watch the index after the perp opens a premium: positioning demand leaves
  the index alone and bleeds out; information moves the index after a lag,
  and the perp keeps its lead while both move on. Size the carry against the
  probability the premium is informed, which comes from today's tape:
  whether the earlier premiums today closed by the perp or by the index,
  and how long the index took to follow when it did. Cut a fade as soon as
  the index starts to follow, and go with an informed premium rather than
  against it. Keep the loss limit: one informed episode against a full fade
  can cost several premiums.
- **Where non-experts go wrong.** "Funding pays the side against the
  premium, so fade it" (it pays, and informed premiums run the fader over);
  "the gap always closes" (it does, here every time, but on half the days
  mostly by the index moving to the perp); carry priced off the book's
  premium with no cap, or off the mainnet funding parameters in the
  `/exchange` file (hourly over a day: worthless), instead of the session's
  on chain; deciding each episode alone, as if the day hadn't shown which
  kind of day it is; waiting for certainty on every episode (on a
  positioning day the carry bleeds out while you wait); full size whatever
  the evidence.

## The central decision

Against a premium the book just opened: fade it, follow it, or wait, and
how big, given what the index does next and what today's earlier premiums
did.

| Option | Why it looks defensible | Why it isn't |
| --- | --- | --- |
| Fade every premium at once | funding pays the side against it, and every gap closes | on a discovery day most gaps close by the index moving 1.5-6× the premium the premium's way |
| Follow every premium | the perp leads spot | on a positioning day most premiums bleed out, and the follower pays funding all the way |
| Wait for the index to show its hand, every time | certainty before risk | the session's lag can be up to 13 s, and a crowded premium can halve in 3 s: the carry is gone by the time it's certain |
| Carry from the book's premium, uncapped | "funding is the premium over the period" | a premium past the session's cap pays the cap; the period is the session's (16-60 s), not the snapshot's day |

**The calculation chain:** read the session's funding interval, period and
cap from the PerpAssetMap → each step, the index (oracles) and the book's
mid, the premium between them → on a premium, P(informed) from the day's
evidence so far (how many earlier premiums the index followed) and from
whether the index has started to follow by now, given the lag the day has
shown → the carry a fade earns (the premium left plus its capped funding
while it bleeds) against the informed loss (the move still to come, less
the funding collected through it) → fade, follow or stand aside, sized by
the edge → re-read every step; flip to follow once the index follows.

Where an early slip spreads: reading the funding parameters from
`MTX_EXCHANGE` (mainnet's hourly interval over a day) makes the carry look
worthless, so the bot never fades and only follows; uncapped carry makes
large premiums look like free money exactly where the cap binds.

**What must be combined:** the instruction (the family's ranges, both kinds,
that funding is Phoenix's), the chain (the session's funding parameters in
the PerpAssetMap; the index and the book each step, through Hawkeye or the
SDK), and the session's own history (how earlier premiums closed, how long
the index took). No single record decides it: the premium doesn't say which
kind it is, the instruction doesn't say which day it is, and the index's
first ticks are noise until the lag has passed.

**Nothing warns the agent.** A bot that fades every premium makes money on
half the days and on most early episodes; its losses come in a few large
informed episodes, and show only in the valuation.

## The four-part shape (fair-task-design)

| Part | Here |
| --- | --- |
| An exact rule | Phoenix's funding: an interval's funding is the interval's average (mark − index) times interval over period, capped per interval, paid to the side against the gap, settled into collateral on the next trade. With the session's parameters (2-6 s over 16-60 s, cap 0.6-1.6% of the price) it changes what a premium is worth: carry per second is min(premium, cap) / period |
| An authoritative, literally true default that reads the wrong way | The instruction: "every episode ends with the gap closed: the book and the index meet", and funding "the longs paying the shorts when the mark is above the index". Both are true; the habitual reading is "fade it, it converges and pays you". On a discovery day the gap closes by the index, after a move several times the premium |
| One settling record off the main path, one ordinary pointer | The index's own path after the open, and the session's earlier episodes. The bot trades the book; the index is one read away (`Trader::oracle_usd`, or Hawkeye's `view_bbo`, which the instruction names as "the mark and the index"). Which kind of day it is lives only in this session's history |
| The rule as one step in a bigger calculation | The task asks for PnL; the funding rule prices one input (the carry) to the fade/follow/size decision |

Real-world sources of difficulty (the skill's list): imperfect information
(an episode's kind is unknowable at the open; evidence arrives with the
lag), a moving world (the carry bleeds while you wait; acting early or late
both cost), judgement without a formula (how big, how long to wait, when to
flip), and the knowledge itself (Phoenix's funding math and where the
session's parameters live).

## Failure modes

| Mode | In a bot | What invites it |
| --- | --- | --- |
| F9 convenient signal accepted | fades every premium because funding pays it and every gap closes | the two literally true lines above |
| F3 second condition missed | judges a premium by its size and the funding alone, not by which side closes the gap | premiums of both kinds open alike |
| F13 change not carried through | decides each episode as if it were the first: never updates on how the day's earlier premiums closed, or on the day's lag | the family's odds are stated per day, not per episode |
| F2 home-made convention | carry from the mainnet funding fields in `/exchange`, or uncapped | the `/exchange` file is the obvious place to look; the session's parameters are in the PerpAssetMap |

## Variants: each regime flips the best response

| Regime | Best response |
| --- | --- |
| positioning day (5-30% informed) | fade early and full once the day has shown itself; cut the rare one the index follows |
| discovery day (70-95% informed) | don't fade; follow once the index starts to follow, sooner as the day confirms |
| long lag (up to 13 s) | wait longer before reading "no follow" as crowded |
| short half-life (down to 3 s) | the carry is front-loaded: decide on the day's odds, not on certainty |
| cap binds (premium above it) | the carry is the cap; the large premium is mostly convergence risk |

| Habit | Fails in |
| --- | --- |
| fade everything | discovery days |
| follow everything | positioning days |
| wait for certainty every time | short half-lives, long lags |
| per-episode decisions only | the first fades on discovery days, the slow fades on positioning days |
| mainnet funding parameters | every crowded premium (never faded) |

## Levers

| Lever | What the market does | What a good bot does | How an honest trader sees it |
| --- | --- | --- | --- |
| the day | draws the share of informed episodes, 5-30% or 70-95% | learns it from how the day's premiums close | the index after each premium; the session's history |
| the lag | the index starts to follow an informed premium after 1.5-10 s (×0.7-1.3) | reads "no follow yet" against the day's lag | the index's move toward the book |
| the half-life | a crowded premium halves every 5-30 s (×0.6-1.4), front-loaded carry | fades early when the day says crowded | the premium bleeding, the book coming back |
| funding | Phoenix's, on the book's mark, 2-6 s over 16-60 s, capped at 0.6-1.6% | prices carry with the session's parameters and the cap | the PerpAssetMap's funding accumulator; Hawkeye's `view_funding` |
| informed drift | the index moves 1.5-6× the premium, book 40-100% of it ahead | follows; never fades it | the index moving toward the book after the lag |
| widening | 30% of crowded premiums widen 1.15-1.5× first | doesn't read widening as informed | the book moves, the index doesn't |
| fizzles | half the quiet stretches and crowded episodes: index and book move 0.15-0.6% together, hold, come back | doesn't read a fizzle as the index following | the premium doesn't change in a fizzle |
| the tape | takers both ways every 0.75-2 s | nothing to read: it keeps the mark fresh | — |
| the wobble (g1) | each hidden session draws the index's noise, 0.03-0.25% a tick, fizzles in proportion; practice holds it at 0.06% | measures it in quiet stretches and reads the index against it | the index's moves when no premium is open |

**Dropped:** a uniform informed share in [0.1, 0.9] per session (in the
model, knowing the share was worth under 2% over not knowing it: waiting a
few steps cost almost nothing); crowded premiums that hold flat before
bleeding (same: waiting was free); Phoenix's mainnet mark (see "Engine
facts": it can't follow a book premium on a surfnet); for g1, a hidden
family with wider lags (to 20 s), half-lives (3-45 s), fuller days, larger
moves and a wider funding period and cap than practice: in the model, a
fit of the practice family lost only 5% on it (371 against 392 a session):
its features generalize across those ranges. The wobble is what they don't
generalize across.

## Engine facts this is built on

- **Phoenix's mainnet mark can't follow a book premium on a surfnet.** The
  mark is a weighted median of the spot oracles plus an EMA of the book's
  gap, the other venues' perp prices, and the book. Surfpool restamps the
  oracle readings' slots to the clock before every override (so deep moves
  stay valid), which zeroes the elapsed time the EMA weighs, so the EMA
  never moves; and the oracle templates report perp = spot. With mainnet's
  weights the mark stayed within 0 to +0.1% of the index whatever the book
  did (probes on a surfnet during the design).
- **So the task prices SPCX's mark from its book** (weights 0/0/100, EMA
  0), written into the PerpAssetMap at setup the way `widen_makers` writes
  the splines. The mark is then the book's mid at its last trade, and
  Phoenix's own program computes funding on it: a 1% book premium at 4 s
  over 20 s moved the cumulative funding +290 quote lots per base lot per
  interval, and -1% moved it -290 (both ways, as Phoenix's math says).
- **The book component refreshes only on trades**, so the grader runs a
  tape: noise takers both ways every 0.75-2 s, from the market-making
  task's taker accounts and code.
- **Funding parameters** are set with Surfpool's `phoenix-market-funding`
  template one slot after the scenario registers, before the bot's first
  turn. The clock runs 250 ms a slot, so a session is minutes of funding
  at a period of seconds: time is compressed, as the realism limits say.
- **Valuation** is at the scenario's book level plus unsettled funding:
  the mark is the book's, and a position's value must count the funding
  Phoenix owes or charges it before the next trade settles it.

## No tells

- Both kinds open from the same draws: the same sizes, signs and ramps
  (`both_kinds_open_alike` checks the means).
- The tape runs in every session, both ways, the same sizes.
- The day is drawn per session; nothing in the setup depends on it.
- Crowded premiums can widen and carry fizzles; fizzles move the index
  either way, so an index move alone isn't the label.
- The session's length is drawn before everything else, and the path is
  cut where it ends.

## The practice family and the hidden family (g1)

`mtx scenario generate` and the practice files draw the **practice family**:
the hidden family with the index's wobble fixed at 0.06% a tick. Hidden
sessions draw their own wobble, 0.03% to 0.25%, and the fizzles scale with
it. The instruction says so, with both numbers. Everything else is the
same family.

Why: g0's screen showed that with the generator in hand, a frontier agent
fits the family inside the hour (all three GPT-6.1 Sol runs trained tree
ensembles on generated sessions, with lag and day features and a funding
model, and beat a reference that did the same). A lever the generator can
teach is a lever an agent fits. The wobble is the one thing an expert reads
off the session before any episode (how far the index strays when nothing
happens) and calibrates "is the index following?" against; a fit of the
practice family has only ever seen one level, so its thresholds are wrong
on quiet days (late to call a crowded premium) and on noisy days (it reads
fizzles as the index following, flips and pays the convergence).

In the calibration model (600 held-out hidden sessions, PnL per session):
g0's reference, fitted on the practice family, 211; the same model trained
on the hidden family without the session's history 350; the g1 reference
(trained on 4,400 hidden-family sessions, with the wobble it measures away
from episodes as a feature) 375; told 470.

Fairness: nothing is hidden. The instruction states the hidden range and
that the generator and practice don't cover it; a bot that measures the
session's wobble and scales its reading of the index by it handles every
session, and the agent can also widen generated sessions itself (the path
is plain JSON). The told rung is told episodes' kinds, not the wobble.

## Why the reference is a fitted model

An earlier task built the same way was fair, but its reference was weak:
every GPT-6.1 Sol run beat it 2-3× by fitting the public generator (tree
models, a replay simulator). Two causes, each answered:

1. **A weak reference.** g0's reference was already a fit of the family:
   a gradient-boosted model of what fading earns from each step to the
   episode's end, trained on 2,500 generated sessions with the episode, the
   session's history and the funding parameters as features. All three g0
   Sol runs still beat it 1.5-1.7×: they fitted short-horizon returns after
   funding (four to ten seconds) and position values with switching costs,
   on 46 features, with up to 5,000 sessions, read the book's level from the
   makers' spline centres rather than the best bid and ask, and sliced
   their orders. So g1's reference *is* the best of them, frozen: on g1's
   hidden sessions it made 1.73× g1's own retrained gradient-boosted model
   and 1.25-1.35× the other two Sol bots (one set of 24, the same secret).
   Two changes, both needed for a reference: its turn no longer stops
   slicing after 1.2 s of wall time (a session must replay the same under
   any load; parallel and sequential grades now match to the cent), and a
   `told` mode for the ladder. The told rung (the same bot told each
   episode's kind: follows informed premiums until the index catches up,
   fades crowded ones while they last) sits above it, and the other frozen
   bots and our own fits sit below it.
2. **The generator ships to the agent.** It still does (the stack's
   convention: `mtx scenario generate` is in the image); we can't stop an
   agent fitting what it generates. So the levers that matter are ones only
   the live session reveals: the day (the informed share), the lag and the
   half-life, drawn per session (g0); and from g1, one the generator
   doesn't draw at all: the index's wobble, which hidden sessions draw and
   practice holds at one level. A fit of the family that ignores the
   session's own history (`nosession`) loses to the reference, and a fit of
   the practice family (`practice`, which is g0's reference, and the frozen
   g0 Sol bots) loses more.

What this can't do within the stack's conventions: hide the family. A
frontier agent with an hour can train the same model we did on whatever the
generator draws (g0 proved it: all three Sol runs did, and beat a g0
reference that did the same, by doing it with more data and a better
execution layer). The task then measures whether it does, whether it prices
Phoenix's funding right, whether it conditions on the session, and from g1
whether it reads the session's noise instead of trusting the practice
family's. ADR 0001's private held-out set would let the hidden family
differ from the public one in more than the wobble.

## Fairness

- Every correct path runs through standard RPC and `mtx-kit`: the
  reference reads Hawkeye's `view_bbo` and `view_margin`, the PerpAssetMap
  (through `phoenix_rise`), and sends market orders.
- The instruction's numbers are the generator's and the engine's, clause
  for clause (`crates/grader/src/carry.rs`, `scenario.rs`, `session.rs`).
- The `/exchange` file's funding fields are mainnet's: the instruction says
  so, and where the session's are.
- Readings written out: valuation at the scenario's book level, with
  unsettled funding; drawdown on the same value; the tape fills resting
  orders like any taker; funding is Phoenix's own.
- Penalty twins: as in the other tasks: a position at the cap passes, over
  it zeroes; a non-standard but allowed method passes, a cheatcode zeroes.
- Void, not zero: a session where more than a tenth of the tape's orders
  fail voids the grade (the mark wouldn't be fresh).
- The reference is hidden from practice: practice files carry no
  reference PnL, and the instruction doesn't describe it.

## Failure modes, priced

| Failure mode | How it's charged |
| --- | --- |
| fades an informed premium | the index's move still to come, less the funding collected |
| follows a crowded premium | the premium's convergence plus the funding paid |
| waits for certainty on a positioning day | the carry that bled out while it waited |
| carry from mainnet parameters | the carry never taken |
| breach | zero for the whole grade |

## Ladder and ceilings

See the README for the numbers. Rungs
(`bench/checks/ladder/funding-carry.json`): nop; blind long and short;
fader (fades every premium at once, full size) and follower (follows every
premium at once), both from `bots/carry-gbm`; practice (g0's
gradient-boosted reference, fitted on the practice family); nosession (the
g1 gradient-boosted model trained without the session's history and
wobble); gbm (g1's gradient-boosted model, fitted on the hidden family:
g1's first reference); reference (`bots/carry`, the best g0 Sol bot,
frozen); told (the reference told each episode's kind); the other two
frozen g0 Sol bots; hindsight.

Order rules: blind, fader, follower, practice and gbm below the reference;
reference < told < hindsight. The frozen bots (practice, gbm, the two Sol
bots) must lose some sessions and stay below told. `nosession` is reported,
not ordered.

## Predictions

Written before g0's screen: reference reward ~0.2 (skill 1); GPT-6.1 Sol
between 0.4 and 1.1 skill. g0 proved that wrong: all three Sol runs fitted
the family better than the reference (skill 1.55-1.66 against a reference
that also raced the world). For g1, against the best of those bots frozen,
on a hidden family whose wobble the generator doesn't draw: Sol between 0.7
and 1.2 skill, the runs that measure the session's noise (or widen their
training data) near 1, the runs that fit the practice family alone below
it.

## Generations

| Version | What happened | What changed |
| --- | --- | --- |
| g0 | Calibration model (500 held-out sessions, PnL per session): fader -75, follower -19, a hand-tuned state machine 274, a Bayes-gated policy 292, a Bayes expected-value policy 309, the gradient-boosted model without the session's history 354, with it (the reference) 373, told 483. Engine ladder (one set of 24): fader -0.39, follower 0.22, nosession 0.90, reference 1.00 (reward 0.25), told 1.29, hindsight 3.97. Harbor: oracle skill 1.064 (should be 1.00), nop 0, cheater 0 (refused calls and tampering caught), sneaky 19 sessions graded normally with 0 refused, `harbor check` 10/11 (the tape's void rule wasn't in the instruction: added). Found: the funding parameters landed at the session's first step, after the bot started; the reference read them before its first turn and sometimes got mainnet's hourly ones, so it didn't replay (the oracle's 1.064). Screen, 3 GPT-6.1 Sol runs (codex, 1 h, one secret): rewards 0.304, 0.294, 0.279 (skill 1.66, 1.55, 1.55 against the raced reference); 97-100% of sessions profitable. All three generated thousands of sessions and trained tree ensembles on short-horizon returns after funding, with episode, lag and day features, and read the book from the makers' spline centres. No failing runs; none was affected by the race (all three read the funding parameters inside a turn) | — (new task) |
| g1 | Calibration model (600 held-out hidden sessions, PnL per session): g0's reference 211, the retrained gradient-boosted model 375, told 470. Engine, one set of 24: the frozen g0 Sol bots made 1.73×, 1.38× and 1.28× the retrained model, the best above its told rung, so the best became the reference. Harbor on 0.11.1: oracle reward 0.237, skill 1.0001; nop 0; cheater 0 (refused calls, tampering); leak audit clean. Screen, 3 GPT-6.1 Sol runs (codex, 1 h, one secret): rewards 0.188, 0.204, 0.249 (skill 0.96, 1.04, 1.27); 93%, 94% and 100% of sessions profitable. All three read the hidden wobble in the instruction and widened their own test or training sessions with noise ("Adding noise to scenarios", "Preparing high-wobble training data", "Retraining features with noise"). Fairness: the one run below the reference (0.96) lost on discovery days (0.81× the reference there, 1.09× on positioning days) and on quiet sessions (0.87× at a wobble under 0.10%): its hand-built strategy favoured carry and was late to follow informed premiums. It had every input the reference had (it read the index, the spline centres and the funding parameters each turn): FAIR, a skill gap. No run was affected by an engine or instruction defect | The one hardening. Hidden sessions draw the index's wobble (0.03-0.25% a tick, fizzles in proportion); the generator and practice keep 0.06%, and the instruction says so. The funding parameters land in setup and are checked, so they're in force before the bot starts. The reference is the best g0 Sol bot, frozen and made replayable (g1's retrained gradient-boosted reference made 0.58× of it on hidden sessions), with a told mode that corrects it with each episode's kind |

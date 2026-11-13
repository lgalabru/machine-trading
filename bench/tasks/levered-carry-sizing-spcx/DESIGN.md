# Design record: levered-carry-sizing-spcx

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way. It refines
card 8 of the batch-1 task cards (domain 5: risk, leverage and margin) and
records its Gate 0. The calibration model and its runs are in
`hidden/gate0/`.

## Thesis

A good bot profits here because it **sizes a funding carry to the index's
volatility state and to its distance from the stop-out**: it reads the
state live from the index (and from how the crowd's discount behaves),
holds a large long while the market is calm, and cuts it when the market
turns stressed, before the crowd flushes the book.

## Expert research

- **Why it's a real expert task.** Perp funding carry is the core carry
  trade of crypto derivatives desks, and its risk is not the funding but
  the crash of the crowded side. Schmeling, Schrimpf and Todorov, *Crypto
  Carry* (BIS WP 1087, 2023): carry is high when leveraged demand is, and
  it crashes when that demand turns. Brunnermeier, Nagel and Pedersen,
  *Carry Trades and Currency Crashes* (NBER Macro Annual 2008): carry earns
  a steady premium and loses in sudden unwinds, and the unwinds cluster in
  high-volatility states. He, Manela, Ross and von Wachter, *Fundamentals
  of Perpetual Futures* (2022): perp-spot gaps are persistent and
  mean-reverting, and the funding-collecting side earns a high Sharpe until
  it doesn't. On the sizing side: Moreira and Muir, *Volatility-Managed
  Portfolios* (J. Finance 72(4), 2017): scaling exposure by the inverse of
  recent variance raises Sharpe because volatility is persistent and
  predicts risk much better than it predicts return; Kelly/mean-variance
  sizing, `leverage ∝ expected return / variance`; Engle, *GARCH 101*
  (JEP 2001) and Hamilton (1989) for estimating clustered, regime-switching
  volatility online. A prop desk's loss limit is the stop the trader sizes
  against: the position must survive the moves the state makes likely.
- **The expert's method.** Price the carry the venue pays (the discount
  over the funding period, capped per interval: Phoenix's rule, with the
  session's parameters from the PerpAssetMap). Estimate the index's
  variance online (EWMA or GARCH on its returns, aware that it updates in
  5-second steps). Size the long by yield over variance, capped so that a
  few sigmas of the next minute stay inside the room left under the loss
  limit. Watch for a state change (the variance stepping up against the
  session's own calm level, the discount widening and churning as the
  crowd leans in) and cut before the flush, not after it. Leave a buffer:
  the stop-out closes through the book, so a breach costs the gap plus
  the close's impact, and ends the day's carry.
- **Where non-experts go wrong.** "Funding pays the longs, so lever up"
  (max size is stopped out in most stressed sessions); "be careful, use a
  constant moderate leverage" (it bleeds in every flush and leaves calm
  carry on the table, so on the family it loses money); "a wider discount
  is a better carry, add" (in stress the crowd leans in before it flushes:
  the wider discount is the warning); sizing to the snapshot's hourly
  funding parameters in `/exchange`; ignoring the loss limit until the desk
  enforces it.

## The central decision

How big a long to hold, step by step, given the yield and the volatility
state the session has shown so far.

| Option | Why it looks defensible | Why it isn't |
| --- | --- | --- |
| Full cap from the start | funding pays the longs every interval | about half the sessions stop it out; each stop-out costs the limit, the gap through it and the close |
| A constant, moderate leverage | "risk management" | the flush hits it anyway (a 1-3% drop at 3x is $300-900), and it gives up the calm carry: on the family it loses money at every constant size above a sliver |
| Size by the discount (yield) | Kelly's numerator | the stressed state widens the discount: this sizes up into the flush |
| Size by yield over variance, with a stop distance | the expert method | needs the state read online: the variance estimate lags a switch, and fat tails make single moves noisy |

**The calculation chain:** read the session's funding parameters (interval,
period, cap) from the PerpAssetMap → each step, the index (the oracles'
median) and the book (the makers' centre) → the discount and the yield per
second, capped → the index's returns, its variance (accounting for 5-second
updates and one-minute GARCH blocks) and its level against the session's
own calm → the state (calm or stressed) from the variance and from the
discount's widening and churn → target leverage from yield over variance
and from the stop distance (room under the limit over a few sigmas of the
horizon) → trade toward it with hysteresis, in slices.

**What must be combined:** the instruction (the family, the funding rule,
the stop-out rule), the chain (the session's funding parameters, the index
and the book every step), and the session's own history (its calm
variance, its discount level). No single clue decides it: the variance of
a short window is noisy and fat-tailed (a one-off tail move isn't a
switch), the discount alone reads the wrong way, and the state's switch
times are drawn.

**Nothing warns the agent before the decision.** A full-size carry makes
money in every calm session; its losses come as stop-outs, which the
report lists but which don't disqualify.

## Variants: each state flips the best response

| State | Best response |
| --- | --- |
| calm, low level | near the cap: the carry's yield over variance is high and the flush is rare |
| calm, high level | smaller: the stop distance binds |
| stressed | flat: the flush odds (1.2% a second) make the carry lose |
| just after a flush | wait for the discount to rebuild (it moves the book down again) |
| low-yield session (small discount, long period) | smaller everywhere |

| Habit | Fails in |
| --- | --- |
| full cap | stressed sessions (stopped out), flushes |
| constant moderate size | every flush, and calm carry left |
| size by the discount | stress (it widens there) |
| vol-targeting without the stop distance | high-level sessions near the limit |
| no state read (variance only, long window) | the first minutes of stress, when the flush is most likely to land before the estimate moves |

## Levers

| Lever | What the market does | What a good bot does | How an honest trader sees it |
| --- | --- | --- | --- |
| volatility state | calm 0.85x / stressed 1.7x the calibration's median, switching once a minute at 8.5% / 21.25% odds | cuts on a switch to stress, re-sizes on the way back | the index's realized variance against the session's own calm |
| session level | each state's level drawn log-normal (log-sd 0.4) | sizes by its own estimate, not the family's median | the index's variance |
| flush | at most one; 0.01%/s calm, 1.2%/s stressed; book -1-3% in 2-10 s, index with it | flat before it, in stress | the state (above) |
| crowd leaning in | in stress the discount drifts to 1.5x its level and churns 2.5x faster | reads the widening as a warning, not a better carry | the book against the index |
| funding | Phoenix's, 2-6 s over 150-600 s, cap 0.6-1.6% of the price | prices the yield with the session's parameters and the cap | the PerpAssetMap |
| stop-out | priced: the desk sells at the market, the session ends | keeps the stop distance inside the limit | the instruction; its own drawdown |
| the tape | takers both ways | nothing to read; it keeps the mark fresh | — |

**Dropped (Gate 0 iterations):** squeezes against the carry with a short
side allowed (the stressed state's expected drift would then pay a
directional bet 15x the carry: the task would reward trading the flush, not
sizing the carry; hence the long-only mandate); a stressed state 2.5-3x the
calm one with no discount coupling (the state was a label: readable from
two or three index updates, so the reference matched the told rung);
premiums of either sign (the long-only mandate needs one side; a discount
paying the longs is the persistent-crowd case of card 8, and a premium
mirror would need a short-only mandate that `max_position > 0` doesn't
allow without a core change).

## No tells

- Every session draws its states from the same chain, opens the same way
  (a discount from one range), and runs the same tape.
- A flush can happen in either state; its size and speed don't depend on
  the state.
- Each state's level is a log-normal draw: a calm session can run hotter
  than a quiet stressed one.
- The state switches only at minute marks (the model's block ends), as the
  instruction says, at drawn times; the number of switches (0-2 typically)
  is drawn.
- The session's length is drawn first.

## Detectability

At the calibration's median level the index's 5-second updates move about
0.035% (sd); stressed updates about twice that. One update separates the
states by about 0.4 sd of a log-variance estimate; ten updates (50 s) by
about 1.3 sd; the discount's churn adds a second, independent read (its
2-second-scale jitter is 2.5x larger in stress) that the instruction states.
Together a good reader separates the states within roughly 30-60 seconds of
a switch, while the flush comes 80 seconds after a switch on average:
reading fast pays, and certainty comes too late. The level draw (log-sd
0.4) blurs a single-session threshold: a reader must compare against the
session's own calm.

## Fairness

- Every correct path runs through standard RPC and `mtx-kit`: the
  reference reads the PerpAssetMap, the spline collection and its trader
  account in one `getMultipleAccounts`, and sends market orders.
- The instruction's numbers are the generator's and the engine's
  (`crates/grader/src/levcarry.rs`, the market model, `risk.rs`): the
  states' multipliers and switching odds, the discount's range and its
  stress behaviour, the flush's odds and sizes, funding, the stop-out rule
  clause by clause.
- The stop-out is stated exactly, with when it's measured, how the desk
  closes and that it is priced, not zeroed.
- Penalty twins: `stop-in` (flattens itself at $900) and `stop-out` (lets
  the desk close it at $1,000). The ladder requires `stop-out < stop-in`.
- The reference is hidden from practice: practice files carry no reference
  PnL.

## Failure modes, priced

| Failure mode | How it's charged |
| --- | --- |
| over-levered into stress | the flush or the variance takes it through the limit: the loss to the limit, the gap past it, the close's impact and fees, and the rest of the session's carry |
| under-levered in calm | the carry not earned |
| adding into a widening discount | the flush |
| re-entering before the discount rebuilds | the rebuild moves the book down |
| over-trading | fees (3.5 bp taker) and impact |
| short, over the cap, another market | zero for the whole grade |

## Gate 0 (before building)

The calibration model (`hidden/gate0/model.py`) simulates the family at
one-second steps: the SPCX calibration's GARCH-t on one-minute blocks, the
two states switching at the block ends with the built odds, the discount,
funding, the flush, costs from the snapshot's spline depth (3.5 bp fee,
~1.4 bp half-spread, ~1 bp per 80 shares of slice), and the priced
stop-out. Each run tunes the tunable rungs on 300 sessions and scores them
on 600 others. The rule: told ≥ 2x the best non-reading rung (nop,
max-lever, fixed 3x, the best constant leverage, premium-scaled; a losing
rung counts as 0).

| Seed | max-lever | fixed 3x | best constant | prem-scaled | reference | told | path oracle | told / best non-reading | order holds (bootstrap, 100-session sets) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 31 | -115 | -106 | -17 | 85 | 195 | 420 | 479 | **4.92** | 99.5% |
| 32 | -98 | -109 | -17 | 78 | 180 | 444 | 509 | **5.66** | 98.5% |
| 33 | -156 | -124 | -20 | 87 | 184 | 402 | 472 | **4.64** | 99.5% |

USD per session. Gate 0 holds on every seed and on 99.9-100% of
bootstrapped 100-session sets (5th percentile of the ratio 2.7-3.4).
Reference ≈ 0.019 reward against the house bound (0.24-0.26 against a
carry-only bound); told ≈ 2.2-2.5x the reference: the headroom a better
state reader can take. An approximate Bayes filter over the states (the
index's variance and the discount's churn) scores 0.8-1.0x the reference
in the model: most of the told rung's lead is the switch-to-flush race,
which no reader wins every time.

The same family with switches allowed every second (an earlier design,
before the infra kept switches at block ends) gave 3.36, 3.50 and 8.37.

## The practice family

The same family as the hidden sessions: the answer in each session is its
latent state path, its level and its flush, drawn per session from the
batch's secret, so practice teaches the distribution, not the draw. The
practice files carry their state path (`market_model.regimes`), which
helps an agent check its reader; hidden sessions are drawn in the verifier
and never written where a bot can read them.

## Predictions

Reference reward ~0.02 (skill 1). Frontier agents: those that build a
variance estimator and size by yield over variance with a stop distance
near 1; those that also read the discount's churn and cut early above it
(up to ~2); full-size and constant-size bots below 0.

## Generations

| Version | What happened | What changed |
| --- | --- | --- |
| g0 | Gate 0 on the calibration model (above). Built on infra/22 (priced stop-out, regime clock and stated regime sets). | — (new task) |

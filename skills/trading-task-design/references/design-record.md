# The design record

Write the task down before building it, and keep the record next to the
task as it changes. Each section answers one question a reviewer would
otherwise ask after the benchmark has already burned a night. The examples
come from a treasury-unwind execution task (sell a large SOL position on
Phoenix spot and OpenBook) and from machine-trading's own tasks.

## Contents

- Expert research
- The central decision
- Variants: each regime flips the best response
- Levers: what the market does, what a good bot does
- No tells
- Detectability
- Fairness
- Failure modes, priced
- Predictions
- Generations

## Expert research

Three parts, each with sources:

- **Why this is a real expert task.** Who does this for money, under what
  constraints. Treasury unwinds run against participation caps and price
  floors; the literature is Almgren–Chriss.
- **The expert's method.** What a professional would do: participation-rate
  execution with urgency from drift, venue routing by depth and fee,
  randomized schedules because TWAP is the canonical exploited one.
- **Where non-experts go wrong.** The habits the task should punish: dumping
  into thin books, fixed schedules, re-signing a pending transaction.

If you can't fill the third part, the task has no failure mode worth
measuring.

## The central decision

- **One decision the task is about**, and the options that look defensible
  but aren't. For an unwind: TWAP looks prudent (ignores volume, drift and
  predictability), dumping looks decisive (pays impact), a fixed rate ignores
  crashes, waiting risks the floor.
- **The calculation chain** from evidence to action, step by step, and where
  an early slip spreads (a unit error in volume makes every allowance wrong).
- **The clues that must be combined**, and where each lives (instruction,
  records, live chain). No single clue may decide it.
- **Why nothing warns the agent before the decision.** A badly paced bot
  still gets fills; its cost shows only in the valuation. That's what makes
  the task measure judgment instead of error handling.

## Variants: each regime flips the best response

List the family's regimes and the best response to each. A regime earns its
place only if it changes what a good bot does:

| Regime | Best response |
| --- | --- |
| drift down | sell fast |
| drift up | sell late |
| crash | pause |
| venue drain | reroute |
| competitor | randomize |

Then the inverse table, **which habit fails where**: TWAP fails drift down,
drift up, the competitor and the drain; dumping fails everywhere but drift
down. A habit that fails nowhere is a strategy the task can't separate from
skill. Changing the seed within a regime should change the path, not the
best response.

## Levers: what the market does, what a good bot does

For every mechanic that creates difficulty, one row:

| Lever | What the market does | What a good bot does | How an honest operator detects it |
| --- | --- | --- | --- |
| the stalker | sells ahead of the bot at 1.5–2.5× its learned pace | randomizes, pauses after being traded ahead of | another seller's fill lands just before the bot's own, sized at a multiple of its pace, on the tape |
| shakeout | a dip that turns into a climb | holds through the dip | taker sell share by count ~0.38 against ~0.60 in a real fall |

The last column is the fairness argument: a lever without an honest way to
see it is a hidden fact, and the task then measures guessing. Record the
levers you dropped and why ("broke the ladder", "frontier bots lost little
there"): each extra lever widens the simple baselines' variance.

## No tells

Every mechanic must be present, or equally possible, in every session; only
its intensity may depend on the regime. Otherwise its presence is the tell:

- A grader-run wallet (competitor, predator, informed taker) is funded,
  seated and onboarded in every session, even when it won't trade.
- Noise sources run in every session; only their means differ by regime.
- Sizes come from the same grid with the same jitter in every regime.
- Opening prices are drawn the same way in every regime.

machine-trading's equivalent lesson: informed and noise takers share one
account pool and one size distribution, or `getBlock` reads the regime.

## Detectability

For each signal the bot needs, estimate how separable it is at the scored
mix, in standard deviations over a stated window: "over 10 blocks the tape
alone separates a shakeout from a fall by about 2.5 sd, the book by about 2;
together they settle it within the dip, the price alone cannot." Below ~1 sd
the regime is noise and the task rewards luck; far above ~4 sd it's a label
and the task rewards reading it once. Check the practice files and any
history you ship carry the signal at the same strength as the scored set.

## Fairness

- **Every correct path is executable** with the agent's own tools: the
  reference does it all through standard RPC and the SDK.
- **The house text is literally true.** Every formula in the instruction is
  the scorer's formula, clause for clause. Check it against the code, not
  memory.
- **Every plausible reading** of each rule is written out: "25% of volume,
  own fills included", "the closing price is the oracle at the last block".
  A reading that flips the outcome is a task bug.
- **Penalty twins.** For each hard rule, a pair of bots on either side of
  it: a fill at the floor passes, one below it zeroes; a cheatcode zeroes, a
  merely non-standard call doesn't; an over-cap sale is charged, not zeroed.
  Run both.
- **Fair failures, told apart.** A bot that doesn't build or makes a unit
  error scores near 0 fairly, but the review must separate those from
  trading failures. Many bots failing the same way triggers an audit of the
  task, not the models.

## Failure modes, priced

Every failure mode is charged through the one reward, in money, and the
record says how:

| Failure mode | How it's charged |
| --- | --- |
| dumps into thin books | impact |
| pays the higher-fee venue when the cheaper one has room | the fee difference |
| leaves a remainder | the remainder's penalty |
| breaks a hard rule | zero |

Check the order matches the business consequence: the worst behaviour must
cost the most. A breach must never score better than complying, so it voids
the grade rather than counting as doing nothing.

## Predictions

Before running models, write down the expected score of each model tier and
of each ladder rung (see [ladder-and-ceilings.md](ladder-and-ceilings.md)).
A result far from the prediction is a finding about the task first.

## Generations

When real bots beat a version, start a new generation and record:

- **Why**: what the frontier bots did, read from their source as evidence,
  never as an answer key ("participation sellers at 84–94% of the cap, a
  12-block slope rule, none reads the tape's composition").
- **Where the old version left their edge**: which lever they never
  triggered.
- **Families tried, in order**, one row each with what it tested and the
  result, including the ones dropped. This is the cheapest record to keep
  and the one most often lost.

machine-trading's exploit checks (`bench/checks/exploits/`) hold the same
idea as frozen bots; the generation log holds the reasoning.

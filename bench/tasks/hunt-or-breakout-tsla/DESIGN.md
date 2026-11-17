# Design record: hunt-or-breakout-tsla

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned.

## Thesis

A good bot profits here because it reads who leads a run through a round level (one hunter wallet pushing, or informed wallets building) and whether the oracle follows, then fades the hunt or follows the break before it resolves.

- **Wins:** fading a hunter push while the book is through the level; following informed flow into a break before the level jumps
- **Loses:** fading every run loses the breaks; following every run loses the hunts; waiting for the oracle enters breaks late and misses hunts that end first
- **Regimes:** every session: 1-3 stop hunts and 1-3 real breaks through a 2-dollar grid, mirrored
- **Mandate:** 12 SPCX position cap, $500 loss limit, 72 hidden sessions per grade.

## Expert research

<!-- TODO: why this is a real expert task, the expert's method, where non-experts go wrong, with sources. -->

## The central decision

<!-- TODO: the one decision the task is about, and the options that look defensible. -->

## Variants

<!-- TODO: each regime and the best response to it; a table of which habit fails where. -->

## Levers

<!-- TODO: what the market does, what a good bot does, and how an honest operator detects each lever. -->

## No tells

<!-- TODO: the audit: nothing in the path's shape gives away what a trader couldn't see. -->

## Detectability

<!-- TODO: how early a good bot can tell the regime, measured on generated sets. -->

## Fairness

<!-- TODO: what the agent is told, what it can learn from practice, and that the reference stays inside the mandate. -->

## Failure modes, priced

<!-- TODO: each way a bot can lose, and what it costs. -->

## Predictions

<!-- TODO: what you expect each model tier to score, before running them. -->

## Generations

<!-- TODO: what each version learned. -->

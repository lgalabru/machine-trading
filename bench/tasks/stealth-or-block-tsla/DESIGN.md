# Design record: stealth-or-block-tsla

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned.

## Thesis

A good bot profits here because it tests the tape by trade size, finds the class whose flow the price follows, and follows only that class.

- **Wins:** it reads markouts by size class in blocks and commits to the class the price rewards
- **Loses:** it follows all flow, one fixed class, or the price itself
- **Regimes:** the informed trade small clips or large blocks, 50/50; a rebalancer works both classes
- **Mandate:** 10 SPCX position cap, $250 loss limit, 72 hidden sessions per grade.

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

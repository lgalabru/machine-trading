# Design record: front-run-aware-unwind-spy

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned.

## Thesis

A good bot profits here because it infers from its own fills when a predator trades ahead of it, and paces and shapes its selling to stay under the trigger the predator reads, leaving the broker only what is cheaper to leave.

- **Wins:** selling at the largest pace the seated front-runner does not read; resting asks; leaving the rest to the broker
- **Loses:** a deadline TWAP or a randomised one that a window-reading predator still sees; dumping into the backstop; slicing so small the broker takes most of the order
- **Regimes:** one front-runner seated in every session, its type drawn per session from a fixed menu, dormant in about a sixth
- **Mandate:** 14 SPCX position cap, $500 loss limit, 72 hidden sessions per grade.

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

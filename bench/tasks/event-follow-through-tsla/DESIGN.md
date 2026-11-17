# Design record: event-follow-through-tsla

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned.

## Thesis

A good bot profits here because it reads which of the funds trading after a TSLA headline keeps leading the price, and positions with it before the verdict carries the jump on or takes part of it back.

- **Wins:** stay flat through the print; after the jump, mark out each wallet's takes, find the fund whose trades keep leading the oracle, and take its side before the verdict
- **Loses:** riding or fading every jump nets its costs; holding through the print is a coin flip; chasing the verdict after it prints is too late; following the market's net flow is following two mirrored funds
- **Regimes:** the headline jumps 1-4% up or down with equal odds; the verdict, a while later, follows through or fades by 30-80% of the jump, with equal odds
- **Mandate:** 12 SPCX position cap, $600 loss limit, 72 hidden sessions per grade.

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

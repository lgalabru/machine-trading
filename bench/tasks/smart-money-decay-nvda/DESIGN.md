# Design record: smart-money-decay-nvda

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned.

## Thesis

A good bot profits here because it keeps a record of each wallet on the tape, follows the ones the price rewards, notices when a record turns, and tells flow that leads a move from flow that follows it.

- **Wins:** per-wallet markouts with a decaying record, following credible wallets net of chasers, exiting at the information horizon
- **Loses:** following the market net flow or the price move pays costs on chasers and decoys; following a wallet on its lifetime record keeps following decoys after they turn
- **Regimes:** every session makes 3 to 6 sharp moves up or down with equal odds; 1-3 informed, 1-3 decoy, 2-4 chaser and 3-6 noise wallets are seated in every session
- **Mandate:** 30 SPCX position cap, $500 loss limit, 72 hidden sessions per grade.

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

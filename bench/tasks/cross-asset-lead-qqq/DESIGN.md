# Design record: cross-asset-lead-qqq

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned.

## Thesis

A good bot profits here because it reads which NVDA wallets trade ahead of moves, and carries their signal into QQQ, sized by how much of NVDA s moves QQQ makes and net of QQQ s costs.

- **Wins:** per-wallet markouts on NVDA with a decaying record, an estimate of QQQ s share of NVDA s moves from the two oracles, and trading QQQ only when that share of the move clears its costs
- **Loses:** following NVDA s or QQQ s net flow, or either price s move, pays costs on chasers and decoys; trading QQQ at the full size on every NVDA signal pays costs where QQQ carries little
- **Regimes:** every session makes 3 to 6 sharp NVDA moves up or down with equal odds, and QQQ carries 0.35 to 0.75 of each; 1-3 informed, 1-3 decoy, 2-4 chaser and 3-6 noise wallets trade NVDA; QQQ s tape is noise and chasers
- **Mandate:** 10 SPCX position cap, $500 loss limit, 72 hidden sessions per grade.

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

# Design record: copy-shy-informed-nvda

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned.

## Thesis

A good bot profits here because it finds the informed fund from its trades and copies it small enough that the fund keeps trading.

- **Wins:** copying the fund it identified at a footprint under its detection threshold, episode after episode
- **Loses:** copying at full size (the fund hides after one episode), following decoys or chasers, or trading the tape blind
- **Regimes:** opening-hour jumps (2.75 an hour) with one shy fund, 1-2 shy decoys, chasers and noise
- **Mandate:** 10 NVDA position cap, $200 loss limit, 72 hidden sessions per grade.

## The fund's read (checklist 16d)

The fund and the decoys use the actor library's `hit_rate`, left at its default of 0.85, with `view_noise_bps` at 0. Each wallet reads the
move ahead correctly on 85% of its horizons. On the other 15% it is wrong for a whole horizon and trades the other way, and each
wallet errs on its own. The fund has a single wallet, so the rule "the informed wallets must agree" changes nothing here. The
copiers (reference, informed and full-size) read the fund's net flow over 240 slots, about one horizon. They enter only the way that flow
points and never flip after entry. The instruction may state the 85%.

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

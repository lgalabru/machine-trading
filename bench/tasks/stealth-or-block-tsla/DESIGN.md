# Design record: stealth-or-block-tsla

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned.

## Thesis

A good bot profits here because it tests the tape by trade size, finds the class whose flow the price follows, and follows only that class.

- **Wins:** it reads markouts by size class in blocks and commits to the class the price rewards
- **Loses:** it follows all flow, one fixed class, or the price itself
- **Regimes:** the informed trade small clips or large blocks, 50/50; a rebalancer works both classes
- **Mandate:** 10 TSLA position cap, $250 loss limit, 72 hidden sessions per grade.

## Market and flow (gate 0 prototype)

- **Session:** 3,000 slots (20 minutes) from 09:31 New York on a drawn trading day, `phoenix-perps-2026-11-13`, TSLA calibration `tsla.20261113.8b7bca8e`.
- **Volatility and jumps:** `vol_mult` 2.2 (the opening 1 m sd net of jumps, 19.1 bp against the calibration's 8.8); `jump_rate_per_hour` 3.9 and `[jump_size]` median 0.382% / p90 0.652%, the measured TSLA opening-hour values (`JUMP_REGIME_MEASUREMENT.md` Table 2, cited in `source`). These are stated directly: since infra/58 as merged (#111) a stated `jump_size` is not scaled by `vol_mult`. The first prototype ran before that fix with the sizes stated as measured / 2.2 (0.1736% / 0.2964%), which drew the same jumps. No scheduled events.
- **Pairs:** sessions come in pairs; the second runs the first's path mirrored, with the other class informed.
- **Sizes:** every actor's sizes are the calibrated lognormal (median 0.008, sigma 1.1) times 5, so the smallest clips clear Phoenix's 0.001 TSLA lot (a stated departure). Small class 0.5x the median, large class 4x; the class cut a reader uses is 2x the median (0.08 TSLA).
- **Flow:** noise `flow_mult` 4 on the calibrated pace times the opening hour's share (x1.28); informed 2 wallets, `intensity_mult` 16 (the card's literal 8 a wallet), horizon 60-150 slots, edge 20-30 bp; 2 chasers (15-25 bp); refill half-life 4-40; one pool of 8 wallets for noise, informed and rebalancers.
- **Matched counts (the generator flag):** the card's literal sizes (0.5 x 8 stealth, 4 x 1 block) make the small bucket's count read the regime. Here the informed keep one pace in both regimes, and the rebalancers (faded decoys with bursts, at the informed's density) are split so each class holds the same directional activity whichever is informed: 0.5x the informed's activity in the informed's class, 1.5x in the other (2x in all, the feasibility model's condition). Measured on 72 recorded sessions: small bucket 183 vs 174 takes, large 103 vs 92 (informed small vs large; sd 65-95).
- **Told (informed bot):** only `informed_class` (`small` or `large`), a process fact. Never the path, the jumps, a side, timing or a wallet.


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

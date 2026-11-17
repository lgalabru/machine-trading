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

## Stylisations

- **The fund's and the decoys' pace (32x the market's taker rate while they trade) is a stylisation.** Phoenix NVDA's calibrated flow
  is 1.24 trades a minute, so a fund trading at the calibrated pace would give no session enough evidence. Every actor's size is 40x
  the calibrated median (0.015 NVDA). Gate 0's smoke checks that the fund's share of takes stays plausible (under about 30%).
- **The market itself is not stylised.** The opening hour's measured jump rate is 2.75 an hour, with a median of 0.305% and a p90 of
  0.533%. Its robust 1 m sd is 1.99 times the regular session's. Sessions run 20 to 30 minutes (4,800 to 7,200 of the market model's
  0.25 s slots).

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

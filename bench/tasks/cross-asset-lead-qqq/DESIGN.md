# Design record: cross-asset-lead-qqq

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned.

## Thesis

A good bot profits here because it reads which NVDA wallets trade ahead of moves, and carries their signal into QQQ, sized by how much of NVDA's moves QQQ makes and net of QQQ's costs.

- **Wins:** per-wallet markouts on NVDA with a decaying record, an estimate of QQQ's share of NVDA's moves from the two oracles, and trading QQQ only when that share of the move clears its costs
- **Loses:** following NVDA's or QQQ's net flow, or either price s move, pays costs on chasers and decoys; trading QQQ at the full size on every NVDA'signal pays costs where QQQ carries little
- **Regimes:** every session makes 3 to 6 sharp NVDA moves (1.5 to 3 median jumps since the rework) up or down with equal odds, and QQQ carries 0.35 to 0.75 of each; 1-3 informed, 1-3 decoy, 2-4 chaser and 3-6 noise wallets trade NVDA; QQQ's tape is noise and chasers
- **Mandate:** 10 QQQ position cap (NVDA 0.001, read only), $500 loss limit, 72 hidden sessions per grade.

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

### Gate 0 (prototype, 2026-11-17)

The informed bot is told only NVDA's roster (which wallets are informed and which are decoys). It is never told the moves, their timing, or QQQ's share of them (`beta`, drawn per session). The diffusion correlation is the fitted us-eq entry (0.647), a fixed process parameter. Every bot trades QQQ through `mtx_kit::run`, pointed at QQQ and its cap. The read-outs are in `hidden/gate0/`.

**v0: NVDA's moves as in smart-money-decay (0.9 to 2.1 median jumps). Failed the strength band.** One fresh 72-session set, all seven rungs:

| Rung | Total | Mean | Sharpe | v2-weighted share of informed |
| --- | --- | --- | --- | --- |
| informed | +1,968 | +27.33 | 1.49 | 1 |
| reference | +125 | +1.73 | 0.14 | 0.01 |
| nop | 0 | 0 | | 0 |
| twap | -751 | -10.42 | | -0.65 |
| follow-nvda-flow | -783 | -10.88 | | -0.64 |
| momentum | -3,080 | -42.78 | | -2.36 |
| follow-qqq-flow | -3,825 | -53.12 | | -2.92 |

The signal gate held: every non-reading rung lost, and informed beat 2x the best of them (nop at 0) on 1.000 of bootstrapped sets. The order held on 0.902. The band failed: the reference made 0.01 of informed v2-weighted (0.06 raw), with a Sharpe of 0.14.

The cause is QQQ's round trip, about 18 bp. That is two taker fees, the spread, and the impact of 10 QQQ on a thin near-touch book. It takes most of the 20 to 25 bp of each NVDA move that QQQ carries. A read that is right nets little, while a misread (a decoy after it turns, or a wallet followed before its record settles) costs a full round trip. The reference's learning errors therefore cancel what it earns. On a 16-session tuning set, a stricter record (40 bp mean markout over 3 marked takes) was the best the reference could reach: 0.15 of informed raw. Halving the size raised its Sharpe to 0.35 but left it at 0.19 of informed.

**Rework: NVDA's moves are its larger ones (1.5 to 3 median jumps, 0.43 to 0.85%). Passed on a reduced ladder.** One fresh 72-session set; the rungs were nop, follow-nvda-flow, reference and informed (`hidden/gate0/ladder-rework.json`):

| Rung | Total | Mean | sd | Win % | v2-weighted share |
| --- | --- | --- | --- | --- | --- |
| informed | +4,034 | +56.03 | 27.10 | 100 | 1 |
| reference | +1,487 | +20.66 | 29.55 | 74 | 0.34 |
| nop | 0 | 0 | 0 | 0 | 0 |
| follow-nvda-flow | -352 | -4.89 | 21.27 | 32 | -0.18 |

The band is 0.34 v2-weighted (0.37 raw), and the reference's Sharpe is 0.70. The order and the 2x gate hold on 1.000 of bootstrapped sets. momentum, twap and follow-qqq-flow were not re-run on the rework: on v0 they lost $10 to $53 a session, and on a 16-session screen of the rework momentum lost $40 a session.

**Realism (checklist item 12): the rework's moves sit outside the measured NVDA range** (`JUMP_REGIME_MEASUREMENT.md`):
- **Size.** 1.5 to 3 median jumps is 0.43 to 0.85%. Ordinary-day NVDA jumps have a median of 0.28% and a p90 of 0.48%; event days 0.34 and 0.53%. Only NVDA's own earnings-reaction day (median 0.61%, p90 0.92%, one day) and the top-vol-decile days (median 0.52%) reach this range. Most of the rework's moves are at or above the ordinary p90.
- **Count.** 3 to 6 such moves in an 8 to 13 minute session is about 15 to 40 an hour. The measured rate is 0.4 to 0.6 an hour all day, and 2.75 in the opening hour.
- **Co-movement.** The family has QQQ carry 0.35 to 0.75 of every NVDA move. The measurement finds that only 21 to 24% of NVDA's jumps coincide with a QQQ jump, although 94% of those that do go the same way.

v0's moves (0.9 to 2.1 median jumps, 0.26 to 0.60%) sit within the measured size range, but they share the count problem with smart-money-decay. So the rework passes Gate 0 only on a regime the data doesn't support. With realistic move sizes, cross-asset following doesn't clear QQQ's costs well enough to leave room for a reference: that is v0's result.


<!-- TODO: what each version learned. -->

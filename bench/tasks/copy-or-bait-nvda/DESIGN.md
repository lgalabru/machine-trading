# Design record: copy-or-bait-nvda (retired at the Gate 0 smoke)

Batch 7 card 1. A public "smart-money" wallet on Phoenix NVDA perps trades
its views in runs; on half the days the owner turns the wallet into bait
once one account has followed `matches` of its last 8 runs within
`follow_slots` of each run's start (net flow on the run's side at
`match_share` of the run's first 3 takes or more). After that the wallet
keeps its pace, sizes and timing but is right half the time.

## What was built

- `bait` on the library's `informed` actor (`crates/grader/src/actors/library/bait.rs`),
  with unit tests and a tells test (pace, sizes and timing unchanged once set).
- `actors.public`: kinds whose wallets every bot is told, as `MTX_PUBLIC_WALLETS`.
- The `copy-or-bait` generator: NVDA open, 4,500 slots, `vol_mult` 2.4,
  measured opening jumps, mirrored pairs, half the pairs bait days; theta
  0.6 to 1.0, k in {3, 4}, window 10 to 40 slots, cap 25 NVDA (about 42 of
  the leader's median takes), $400 drawdown with stop-out. On a genuine day
  the rule runs too, with `hit_rate_after` 0.85, so both days run the same
  machinery.
- The informed bot is told the rule (window, share, k, 8 runs, 3 takes, run
  length), never the day. Rungs as components: informed, informed-day (a
  diagnostic told the day), copy-all, copy-1in8, copy-late, copy-small,
  safe-blind (two early copies in a session, the rest after the widest
  window), twap, nop.

## Gate 0 smoke (16 sessions, runner-18, fresh secret each)

Run 1: leader edge 20 to 30 bp, pace 48x while it sees a move, decoys 4x.

| Rung | Total $ | v2-weighted / informed | Bait set |
| --- | --- | --- | --- |
| informed-day (diagnostic) | 86 | 1.28 | 2/16 |
| informed | 71 | 1.00 | 0/16 |
| copy-all | 61 | 0.59 | 2/16 |
| copy-1in8 | 33 | 0.32 | 0/16 |
| safe-blind | 30 | -0.08 | 0/16 |
| copy-late | 12 | -0.37 | 0/16 |
| nop | 0 | 0 | 0/16 |

Informed / best blind (copy-all) = 1.16 raw, 1.7 v2-weighted: under 2.5.
The leader reads 0 to 10 runs a session (median about 2, not the model's
10.7), so a copier rarely follows 3 or 4 of them and the bait seldom sets.
No false triggers from noise, chasers or decoys (nop 0/16).

Rework (the one allowed): edge 12 to 18 bp, 9 to 23 runs a session.

| Rung | Total $ | Bait set |
| --- | --- | --- |
| copy-1in8 | -87 | 0/16 |
| informed | -225 | 2/16 |
| safe-blind | -313 | 0/16 |
| copy-all | -548 | 14/16 |

Every copier loses: at 12 to 18 bp the leader's moves don't pay a 25 NVDA
copy's round trip. No signal. Retired.

## Why it can't pass as specified

With the day untold, the informed bot's edge over a careful blind copier is
only the exact rule: the blind copier stays under the smallest stated k and
copies late past the widest stated window. In the feasibility model
(`feas_b7`, rule-told informed against "two early, the rest late") that
ratio is 1.3 to 1.8 even with 10 runs a session. A pass needs the day told
(the card's design, which the batch's rules treat as a realised draw), or
runs big enough to pay costs and frequent enough for the bait to bind.

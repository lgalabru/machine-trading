# Gate 0: the engine's ladder on one hidden set

The prototypes graded with `mtx grade --draw event-follow-through --count 72
--v2-sessions --trusted` on `phoenix-perps-2026-11-13`, every rung on the same
72 sessions (one fresh secret, used for nothing else), then `summary.py` on
their reports (`gate0-run1.txt`):

- `informed` (`checks/bots/informed`, `--tell-latent`): told the informed
  fund's wallets (`{"roster": ["informed"]}`) and nothing else;
- `reference` (`reference/`), untold: marks out every account's takes after
  the jump and follows the accounts that lead the oracle (and fades those
  that lag it);
- the non-reading rungs, `checks/bots/naive` with `MTX_NAIVE`: `ride` and
  `fade` (the cap with or against the jump once it is done), `hold` (the cap
  long through the print), `chase` (follows the first 0.2% move after the
  jump, the verdict, for 150 slots) and `net-flow` (follows every account's
  net buying, without telling them apart).

The parameters were set on a separate 12-session development secret before
this run.

On this set the verdict followed through in 30 sessions and faded in 42,
which is why `fade` is positive and `ride` negative (each nets about minus
its costs in expectation). The reference read the funds in time in 35
sessions, won 30 of them, and took 79% of the informed bot's PnL there; in
the rest the verdict came before it had read anyone (settle to verdict: 14
to 248 slots, median 122), and it stayed flat.

The ladder's order holds on 0.854 of 2000 bootstrapped sets (0.841 of 5000
with another seed): just at the bar, because the lucky `fade` and `hold`
sometimes beat the reference in a resample. The informed bot doubles the
best non-reading rung on 0.95 of them.

## A second set, and both pooled

The order bootstrap was at the bar, so the same binaries graded a second
fresh 72-session secret, with no tuning between the sets
(`gate0-run2.txt`), and `summary.py` ran on both sets pooled, 144 sessions
(`gate0-pooled-144.txt`; session names prefixed by set):

| | Second set | Pooled, 144 |
| --- | --- | --- |
| informed / best non-reading | 21.5 (hold) | 19.0 (hold) |
| informed >= 2x best non-reading, bootstrap | 0.922 | 0.989 |
| ladder order, bootstrap (2000; 5000 seed 11) | 0.898; 0.910 | 0.968; 0.972 |
| reference, weighted share of informed | 0.44 | 0.40 |
| reference per-session Sharpe | 0.56 | 0.55 |

## Re-gate after main's #114 and the reference fix (fails the order bar)

Main's #114 gives the informed traders and decoys a `hit_rate` of 0.85 (each
fund misreads the move for a horizon at a time). The reference now enters only
on a reading held three steps, keeps an account's reading until its markouts
fall under one standard error, and never turns round after entry. One fresh
72-session secret, all rungs rebuilt (`gate0-run3-after-114.txt`):

- informed / best non-reading (fade): 13.1; ≥ 2x on 0.936 of bootstraps;
- ladder order: **0.796** of 2000 bootstraps (0.803 of 5000): under 0.85;
- reference 0.26 of informed (loss-weighted), per-session Sharpe 0.25.

The informed bot halved ($30.65 a session against $56 before #114): it
changed side 50 times and lost 10 sessions ($799). The reference traded 33
sessions and won 23 of them (30 of 35 before).

## Longer reading window (fails; the card retires)

Both the informed bot and the reference read a fund's net flow over 90 slots
(the funds' longest horizon) past 2 TSLA, confirmed three steps, and never
turn round; `fund_hit_rate` stays 0.85. One fresh 72-session secret
(`gate0-run4-longer-window.txt`):

- informed $910 ($12.64 a session; lost 25 sessions, -$1,680) against the
  best non-reading rung, fade, $182: 5.0x, but only 0.353 of bootstraps;
- ladder order 0.134 of 2000 bootstraps (0.139 of 5000);
- reference $85, Sharpe 0.02, below its band.

With every wallet misreading a horizon at a time, a fund's flow after the
jump no longer carries the verdict's sign reliably within the 20-260 slots
before it lands, for the told bot or a reader. The card retires.

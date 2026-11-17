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

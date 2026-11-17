# Gate 0: the engine's ladder on one hidden set

The prototypes graded with `mtx grade --draw hunt-or-breakout --count 72` on
`phoenix-perps-2026-11-13`, every rung on the same 72 sessions (one fresh
secret), then `summary.py` on their reports:

- `informed` (`checks/bots/informed`, `--tell-latent`): told the informed and
  hunter wallets, the hunter's hold and trigger, the grid and the informed
  horizon;
- `reference` (`reference/`), untold;
- `fade`, `follow` (non-reading) and `wait-oracle` (reads the oracle only):
  `checks/bots/naive` with `MTX_NAIVE`.

`gate0-run1-reference-v1.txt` is the first reference; `-v2` the one rework
(it reads the crowd before chasers can join, and leaves runs nobody led),
graded on the same sessions as the other rungs' v1 reports.

`gate0-final.txt` is the full build (main merged, reference as shipped), every rung re-graded on the same 72 sessions as run 1.
`gate0-redesign.txt` is after the first screen's fix (breakers' sweeps, one wallet each, the identity-learning reference, the informed bot riding pushes), on fresh sessions, with `push-fade`, the first screen's bot re-implemented without identity.
`gate0-main114.txt` is the same design on main with #113 and #114, on fresh sessions.

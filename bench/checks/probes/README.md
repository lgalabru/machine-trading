# Probe bots

Reward hacking can't be ruled out, only tested: scoring v2 must pay for skill
and for nothing else. A probe is a bot with no skill, a known small amount, or
a way to game the rule. The ladder (`../ladder`) grades each on the task's
hidden sets and `mtx gates` fails the task when one lands outside its band
(`probe-bots`, in "Graded correctly").

The probes are real process bots on `mtx-kit`'s run loop, one crate for every
perp task, chosen and tuned by `MTX_PROBE_*` variables the ladder sets per rung
(`env`). The variables and what each probe does are in `src/lib.rs`'s header.
Market making is out: its redesign has its own probes.

| Probe | Rung(s) | Where it must land (v2 `raw`: 0 = doing nothing, 0.5 = the reference) |
| --- | --- | --- |
| naive full-cap long and short | `blind-long`, `blind-short` | at most 0.02 |
| zero-edge coin flip, half and full cap | `coin-flip-half`, `coin-flip-full` | at most 0.02 |
| martingale (doubles after a loss, capped) | `martingale` | at most 0.02 |
| limit-hugger (rests at the best prices) | `limit-hugger` | at most 0.02 |
| stop-loss lotto (coin plus a trailing stop at 0.15 of the drawdown limit) | `stop-lotto` | at most 0.02 |
| levered reference ×1.5, ×2, ×3, clipped to the cap by the run loop | `lever-*` | weighted PnL at most m × the reference's |
| occasional breacher: the ×3 lever sending past the cap, straight to the venue, in 3 sessions a set, and keeping its strategy | `breacher` | no more than its clipped twin `lever-3` and under the reference; it must breach in every set |
| crash-when-losing (the honest bot, closing out and exiting once 0.03 of the limit down; `fatal` first makes a call the guard refuses) | `crash-honest`, `crash-fatal` | under the honest bot it wraps (`honest-0.2`), once they differ in 3 sessions; the fatal grade must be fatal |
| honest but noisy (a blend of reference and coin flip, labelled with a target per-session Sharpe 0.1, 0.2, 0.3, 0.5) | `honest-*` | judged on the Sharpe it measured, not its label: from 0.2 it must beat doing nothing; one must measure between 0.05 and 0.35 |
| the reference | `reference` | 0.5 |

The probes, the bands, the 0.02 tolerance, the 95% confidence and the 0.2 pay
line are fixed in `bench/checks/ladder/ladder.py`: a task's ladder file only
supplies the rungs (a `probes` section, `{}`), and a required rung it lacks, or
a twin that isn't one, fails. A task that says nothing about probes fails
`mtx gates`; one that opts out says why (`"probes": {"exempt": "why"}`).

A rule fails when a bootstrap over the sessions (scored as the grader does,
near-breach charge and breach cap included) shows it broken with 95%
confidence. A point estimate that breaks it without that is `inconclusive`: a
fast `mtx gates` tolerates it, the full one fails it. A probe that proved
nothing (a breacher that never breached, a crash that never came, a fatal probe
whose grade wasn't fatal) is `no_power` and fails.

## What the wrappers do

- **Lever** shows the reference a market with its position divided by `m` and
  multiplies the reference's orders by `m`. The run loop cuts the sheet to the
  mandate's cap, so a lever works below the cap.
- **Breacher** is that same lever in a seeded share of sessions (`MTX_PROBE_BREACHES`
  a set; the ladder tells it the set's size as `MTX_PROBE_SESSIONS`) sending its
  orders straight to the venue, past the cap, and keeping its strategy after
  the breach. Its twin is the clipped lever. A cap breach scores at least the
  whole limit lost times `k` whatever the session made, so on a task whose
  reference already rides the cap the breacher is below its twin with or without
  the run cap; the ladder reports its raw without the cap beside it.
- **Honest** follows the reference in a share `q` of sessions (a seeded draw)
  and holds a full-cap coin flip in the rest: a mixture of a bot with edge and
  one without. Its per-session Sharpe is between the two and depends on the
  task's reference. `calibrate.py` solves `q` for a target Sharpe from a
  finished ladder's `reference` and `coin-flip-full` reports; the gate judges
  each honest rung on the Sharpe it measured.

## A task opts in

1. A rung crate `bench/tasks/<task>/checks/rungs/probe` (copy flash-crash's:
   `Cargo.toml` with the task's reference as a dependency, a five-line
   `main.rs` passing `Some(Box::new(Reference::default()))` to
   `mtx_probes::main_with`, and `build.sh`, `run.sh`, `.gitignore` as in
   `mark-pusher`). `just ladder` builds it.
2. The required rungs in `checks/ladder.json` (names fixed: copy flash-crash's,
   which has all of them) and `"probes": {}`.
3. Run `just ladder <task>` with `coin-flip-full` and `reference`, then
   `python3 -I bench/checks/probes/calibrate.py target/ladder/<task>` and set
   each honest rung's `MTX_PROBE_BLEND`.

The task's v1 grade is unchanged: the ladder only adds `--scoring v2` and the
reference's own run to each rung's grade.

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
| occasional breacher (the reference, with 3 sessions a set over the position cap) | `breacher` | under the reference, and it must have breached |
| crash-when-losing (an honest bot that exits once 0.3 of the limit down; `fatal` also makes a call the guard refuses) | `crash-exit`, `crash-fatal` | under the honest bot it wraps |
| levered reference ×1.5, ×2, ×3 | `lever-*` | weighted PnL at most m × the reference's; if it breached, under the reference |
| honest but noisy (per-session Sharpe 0.1, 0.2, 0.3, 0.5) | `honest-*` | from Sharpe 0.2, over doing nothing; 0.1 is reported only (v2's break-even is about 0.16) |
| the reference | `reference` | 0.5 |

A rule fails only when a bootstrap over the sessions shows it broken with 95%
confidence; a point estimate that breaks it without that is `inconclusive`
(never a failure), and a breacher that never breached is `no_power` (a
failure: it proved nothing).

## What the wrappers do

- **Lever** shows the reference a market with its position divided by `m` and
  multiplies the reference's orders by `m`. The run loop cuts the sheet to the
  mandate's cap, so a lever works below the cap; going past it is a mandate
  breach, which is the breacher's job.
- **Breacher** takes the position 20% over the cap, with orders sent straight
  to the venue, in a seeded share of sessions (`MTX_PROBE_BREACHES` a set; the
  ladder tells it the set's size as `MTX_PROBE_SESSIONS`), and holds it.
- **Honest** follows the reference in a share `q` of sessions (a seeded draw)
  and holds a full-cap coin flip in the rest: a mixture of a bot with edge and
  one without. Its per-session Sharpe is between the two and depends on the
  task's reference. `calibrate.py` solves `q` for a target Sharpe from a
  finished ladder's `reference` and `coin-flip-full` reports; the ladder
  reports the Sharpe each honest rung achieved beside its target.

## A task opts in

1. A rung crate `bench/tasks/<task>/checks/rungs/probe` (copy flash-crash's:
   `Cargo.toml` with the task's reference as a dependency, a five-line
   `main.rs` passing `Some(Box::new(Reference::default()))` to
   `mtx_probes::main_with`, and `build.sh`, `run.sh`, `.gitignore` as in
   `mark-pusher`). `just ladder` builds it.
2. Rungs in `checks/ladder.json`, as flash-crash's, and a `probes` section:

   ```json
   "probes": {
     "zero_edge": ["blind-long", "coin-flip-full"],
     "pairs": [["breacher", "reference"], ["crash-exit", "honest-0.2"]],
     "breaches": ["breacher"],
     "levered": [{"rung": "lever-2", "m": 2.0, "base": "reference"}],
     "honest": [{"rung": "honest-0.2", "sharpe": 0.2}]
   }
   ```

   Under v2's default ceiling (the cost-aware hindsight bound) a task needs no
   `eta`; a task with an informed bot gets that as its 1.
3. Run `just ladder <task>` with `coin-flip-full` and `reference`, then
   `python3 -I bench/checks/probes/calibrate.py target/ladder/<task>` and set
   each honest rung's `MTX_PROBE_BLEND`.

The task's v1 grade is unchanged: the ladder only adds `--scoring v2` and the
reference's own run to each rung's grade.

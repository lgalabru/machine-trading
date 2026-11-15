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
| stop-loss lotto (coin plus a trailing stop at 0.15 of the drawdown limit) | `stop-lotto` | pays no more than its Sharpe: if it measured under v2's break-even Sharpe (about 0.16), at most 0.02; it has a real, weak edge on trending paths (TUNE2 §3) |
| levered reference ×1.5, ×2, ×3, clipped to the cap by the run loop | `lever-*` | weighted PnL at most m × the reference's, and no more than its Sharpe pays |
| breacher: the ×3 lever sending past the cap, straight to the venue, in every session it can, and keeping its strategy. The ladder composes runs with exactly b = 1, 2 and 3 breach sessions from its graded rows and its clipped twin's | `breacher` | two gates per b: the run cap (a bot levering everywhere that breaches in b sessions, capped, scores at most `lever-3`) and the per-session floor (the twin's run with b sessions swapped for breach sessions, uncapped, scores at most the twin). No power if it never breached, was fatal, or no set has b breach sessions |
| crash-when-losing: the honest bot, closing out and exiting once 0.03 of the limit down; `crash-fatal` first makes a call the guard refuses | `crash-honest`, `crash-fatal` | `crash-fatal` (grade must be fatal) under the honest bot it wraps (`honest-0.2`), once they differ in 3 sessions. `crash-honest` is a stop-loss, a risk control a desk rewards: reported, never gated |
| honest but noisy (a blend of reference and coin flip, labelled with a target per-session Sharpe 0.1, 0.2, 0.3, 0.5) | `honest-*` | judged on the Sharpe it measured, not its label; a probe that never traded or went fatal, or that played exactly the sessions another honest probe did, is `no_power`; each honest rung has its own `MTX_PROBE_SALT` (on a small set, close blends draw the same sessions): from 0.2 it must beat doing nothing; one must measure between 0.2 and 0.40, so it is judged: the fast set is a fixed public seed where `honest-0.3` measures 0.35 |
| the reference | `reference` | 0.5 |

The probes, the bands, the 0.02 tolerance, the 95% confidence and the 0.2 pay
line are fixed in `bench/checks/ladder/ladder.py`: a task's ladder file only
supplies the rungs (a `probes` section, `{}`, or `{"exempt": "why"}` with a non-empty reason), and a required rung it lacks, or
a twin that isn't one, fails. A task that says nothing about probes fails
`mtx gates`; one that opts out says why (`"probes": {"exempt": "why"}`).

For the zero-edge gamblers (not the stop-lotto, which is held to its Sharpe: TUNE2 §4 shows the strict C2 fails it on a fair task) TUNE2's C2 also holds: the headline of a 5-run model (clamp of the mean raw), over 5-run sets that made money, is at most 0.05.
The matched-Sharpe long-vol/short-vol pair is not produced: it is synthetic (equal mean and spread by construction), and cannot come from bots trading replayed paths. `test_ladder.py` checks the relation TUNE2 expects (long-vol scores over short-vol) on the scoring function instead.

The break-even Sharpe depends on the shape of the P&L (SCORECARD #8: about 0.07 to 0.20), so `pays_its_sharpe` fails only with bootstrap confidence: a point estimate near the line is inconclusive even in a full `mtx gates`. A floored reference anchor makes the reference check `no_power`, not a pass.

A rule fails when a bootstrap over the sessions (scored as the grader does,
near-breach charge and breach cap included) shows it broken with 95%
confidence. A point estimate that breaks it without that is `inconclusive`: a
fast `mtx gates` tolerates it, the full one fails it. A probe that proved
nothing (a breacher that never breached, a probe that never traded or went fatal, a fatal crash that never came, a fatal probe
whose grade wasn't fatal) is `no_power` and fails.

## What the wrappers do

- **Lever** shows the reference a market with its position divided by `m` and
  multiplies the reference's orders by `m`. The run loop cuts the sheet to the
  mandate's cap, so a lever works below the cap.
- **Breacher** is that same lever, sending its orders straight to the venue,
  past the cap, and keeping its strategy after the breach: in a share of
  sessions `MTX_PROBE_BREACHES` over the set's size (`MTX_PROBE_SESSIONS`,
  told by the ladder); the ladder file sets it far above the size, so every
  session breaches that can. Its twin is the clipped lever. A bot sees one
  session and sessions are isolated, so a run with exactly b breaches is
  composed from the graded rows: the ladder scores it deterministically for
  b = 1, 2 and 3, once with the run cap (the cap holds a bot that levers
  everywhere and breaches in b sessions to its twin) and once swapped into the
  twin's run without it (the per-session floor: a breach session scores at
  most -k times the limit). A regression of either rule fails the gate.

  Each check runs only where it can fail. The run cap check needs a run that
  beats its twin without the cap; the floor check needs the swapped run to beat
  the twin with the breach sessions unfloored (their PnL over nop's, weighted).
  Where it doesn't, the check is `not_exercised` (reported, never a pass, and it doesn't fail the gate:
  `no_power` is for a breacher that never breached or was fatal). **On flash-crash the
  run cap has no power by construction**: the breacher breaches in nearly every
  session (11 of 12 on the fast set), the one clean session is flat, and a
  breach there already scores at most the limit lost times `k`, below its twin
  (GRADING_DECISIONS, "Breacher on flash-crash"). The run cap is covered by the
  grader's own test instead (`the_run_cap_holds_a_bot_that_levers_everywhere`
  in `crates/grader/src/scoring_v2.rs`, which scores composed sessions where
  the cap binds and fails if the cap is removed); the ladder's copy of the
  scoring is checked against the grader on every grade. A task whose reference
  doesn't ride the cap gets real power from the same checks.
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

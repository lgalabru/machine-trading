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
| breacher: the ×3 lever sending past the cap, straight to the venue, in every session it can, and keeping its strategy. The ladder composes runs with exactly b = 1, 2 and 3 breach sessions from its graded rows and its clipped twin's | `breacher` | two gates per b: the breach-rate penalty (the composed run, a bot levering everywhere that breaches in b sessions, scores strictly below `lever-3` and loses exactly c·b/n to the penalty, c = 1.25) and the per-session floor (the twin's run with b sessions swapped for breach sessions, without the penalty, scores at most the twin). No power if it never breached, was fatal, or no set has b breach sessions |
| crash-when-losing: the honest bot, closing out and exiting once 0.03 of the limit down; `crash-fatal` first makes a call the guard refuses | `crash-honest`, `crash-fatal` | `crash-fatal` (grade must be fatal) under the honest bot it wraps (`honest-0.2`), once they differ in 3 sessions. `crash-honest` is a stop-loss, a risk control a desk rewards: reported, never gated |
| honest but noisy (a blend of reference and a coin flip sized to the reference's risk, labelled with a target per-session Sharpe 0.1, 0.2, 0.3, 0.5) | `honest-*` | judged on the Sharpe it measured, not its label; a probe that never traded or went fatal, or that played exactly the sessions another honest probe did, is `no_power`; each honest rung has its own `MTX_PROBE_SALT`; from a measured 0.2 it must beat doing nothing; one must measure between 0.2 and 0.40, so it is judged. On flash-crash at 4 sets of 72 sessions (fresh secret): `honest-0.1`'s blend, 0.05, is set by hand: `calibrate.py` returns 0.000 for a target of 0.1 (the coin alone already measures about that, so no blend is needed), and the rung is reported only. `honest-0.2` and `honest-0.3` measure 0.20 and score +0.06 and +0.10, `honest-0.5` measures 0.52 and scores +0.49, none breached in 288 sessions, and the charge was $38 and $146 against $5.9k and $7.8k of weighted profit |
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
near-breach charge and breach-rate penalty included) shows it broken with 95%
confidence. A point estimate that breaks it without that is `inconclusive`: a
fast `mtx gates` tolerates it, the full one fails it. A probe that proved
nothing (a breacher that never breached, a probe that never traded or went fatal, a fatal crash that never came, a fatal probe
whose grade wasn't fatal) is `no_power` and fails.

## One-sided mandates

A probe's random component stays inside the mandate. The probe reads the
session's own instrument allowance (its `sides`, `min_position` and
`max_position`) and derives which sides the account may hold: both, long only
(a client order to buy: `min_position` 0 or `sides: [buy]`) or short only.
Nothing is configured per task. On a two-sided mandate the coin flip, the
honest mixture's coin and the crash probes' coin are unchanged (-1 or +1 of the
cap). On a one-sided one:

- `coin-flip` (and so the honest blend's random sessions and `crash-*`) holds a
  random size, drawn evenly from nothing to the cap, on the allowed side: a
  random buy schedule with random timing and size, no edge;
- `martingale` and `stop-lotto` bet on the allowed side or sit out a block (a
  coin for timing);
- `hugger` rests only the allowed side of the book.

Without this a buy-only task's probes shorted half the time, every short was a
breach, and every honest probe scored -1 without ever testing the pay line.
The shared `blind-short` rung of the ladder is outside this: a buy-only task
supplies its own (see the task's ladder file).

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
  b = 1, 2 and 3, once as the bot that levers everywhere and breaches in b
  sessions (the breach-rate penalty must hold it below its twin) and once
  swapped into the twin's run without the penalty (the per-session floor: a
  breach session scores at most -k times the limit). A regression of either
  rule fails the gate.

  The penalty check always binds: it reads the scored run against the same run
  without the penalty and wants exactly c·b/n between them (not where the -1
  floor takes the score), and the breacher strictly below its twin. The floor
  check runs only where the floor can fail: the swapped run with the breach
  sessions unfloored (their PnL over nop's, weighted) must beat the twin;
  where it doesn't the check is `not_exercised` (reported, never a pass, and
  it doesn't fail the gate; `no_power` is for a breacher that never breached or
  was fatal). **On flash-crash the floor has no power by construction**: the
  breacher breaches in nearly every session (11 of 12 on the fast set), a breach
  there already lost more than the floor, and the floor only takes back a loss
  (GRADING_DECISIONS, "Breacher on flash-crash"). The grader's own tests cover
  the penalty (`the_breach_penalty_holds_a_bot_that_gains_a_little_and_breaches_a_little`
  in `crates/grader/src/scoring_v2.rs`, which scores composed sessions and
  fails if the penalty is removed); the ladder's copy of the scoring is checked
  against the grader on every grade.
- **Honest** follows the reference in a share `q` of sessions (a seeded draw)
  and holds a coin flip in the rest, sized (`MTX_PROBE_SIZE`) so its
  session-PnL spread and drawdowns are no larger than the reference's (the smaller of the two matches): a mixture of a bot with edge and one
  without, at the reference's risk. A full-cap coin gambles far past it: its
  drawdowns are the near-breach charge's to tax and some of its sessions
  breach, so a probe built on it scores at or below nothing whatever its edge
  (the first 72-session run showed it). Its per-session Sharpe is between the
  two and depends on the task's reference. `calibrate.py` solves the size and
  `q` for a target Sharpe from a finished ladder's `reference` and
  `coin-flip-full` reports; the gate judges each honest rung on the Sharpe it
  measured.

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
   each honest rung's `MTX_PROBE_SIZE` and `MTX_PROBE_BLEND`.

The task's v1 grade is unchanged: the ladder only adds `--scoring v2` and the
reference's own run to each rung's grade.

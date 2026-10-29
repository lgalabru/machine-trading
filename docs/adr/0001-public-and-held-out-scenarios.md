# 0001. Public scenarios for development, a private held-out set for rankings

- **Status:** Accepted (2026-10-28). The held-out set is deferred until the
  first leaderboard.
- **Scope:** every task in `bench/tasks`.

## Context

Each task's hidden scenarios ship in its `tests/` directory. Harbor copies
them into the verifier only after the agent has finished, so an agent under
evaluation never sees them. But the task is public, so **people** can:
anyone who downloads it from Harbor Hub or GitHub has every scored
scenario.

That's fine for development. It breaks down once scores are compared across
teams:

1. **Overfitting.** A developer who reruns and tunes until the score climbs
   is fitting the twelve scored paths, not the scenario family. Their score
   overstates how their bot would do on fresh draws. This is the multiple
   testing problem from quant finance (see the
   [trading-task-design skill](../../skills/trading-task-design/references/quant-pitfalls.md)).
2. **Contamination.** Public benchmark data ends up in training corpora.
   Within a model generation or two, a model may have seen the exact price
   paths and reference PnLs. SWE-bench Verified was retired over this.
   The canary string in each `instruction.md` discourages it, but doesn't
   prevent it.
3. **Special-casing.** A submitter can recognize which known scenario is
   playing from its first few slots and replay the best trades for it.
   Nothing on the public set stops that.

The people hurt are the honest ones: a real 0.5 ends up ranked below an
overfit 0.9.

## Decision

Split scenarios by purpose, as quant competitions split public and private
leaderboards (Kaggle, Numerai):

| Set | Who has it | Used for |
| --- | --- | --- |
| **Practice** (`environment/practice`) | Agents, during the task | Honest research inside the task |
| **Public hidden** (`tests/scenarios`) | Everyone who downloads the task | Development: iterate, debug, compare locally, train against |
| **Private held-out** (a private Harbor dataset) | Maintainers only | Official rankings and published results |

- The **public dataset** (`solana-foundation/machine-trading`) stays as it is.
  Its README says scores on it are a development signal, not a held-out
  evaluation.
- The **private dataset** has the same task, environment, grader and
  scenario family, with freshly drawn, stratified, calibrated scenarios.
  Their seeds are never recorded. Official numbers come from running
  submitted agents on it without exposing the scenarios: hosted Harbor jobs,
  or maintainers running submissions.
- **Rotation:** each release draws a new private set. A retired private set
  can become the next public one, so every scenario eventually goes public.
- **Deferred:** build the private dataset when the first leaderboard or
  results paper is announced, not before. Until then nobody ranks against
  the scores, and the overhead buys nothing.

## Alternatives considered

- **Public only.** Simplest. Fine for a development tool, but rankings
  built on it can't be trusted (Context, 1–3).
- **Generate hidden scenarios at grade time from a secret seed.** The task
  stays fully public, and the verifier draws its scenarios when it runs from
  a seed only the official runner holds. No scenario is ever published. But
  the reference has to be calibrated on the fly, which adds about 20 minutes
  per trial, and the secret has to reach the verifier without reaching the
  agent. A good option if a second dataset turns out to be too much process.
- **Live markets.** Contamination-free by construction (LiveTradeBench,
  Alpha Arena), but a single path that can't be reproduced, with too few
  independent bets to rank anyone. It complements this benchmark rather
  than replacing it.

## Consequences

- Developers keep the full public loop: download, run, read the hidden
  scenarios, rerun. Nothing changes for them.
- Published rankings become meaningful, at the cost of a second dataset to
  calibrate and publish per release, and a way to run submissions privately.
- A held-out scenario costs the same to make as a public one:
  `mth scenario generate` with a private seed, then `mth calibrate`. The
  extra work is process, not code.
- The canary GUID stays in every instruction, so contaminated training data
  can be detected.

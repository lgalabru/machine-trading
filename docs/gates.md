# Gates

`mtx gates <task>` runs a task through the quality bar and writes one report,
`target/gates/<task>/gates.json` and `gates.md` (`--out` moves it). It exits 1
if any check fails. Two statuses never fail it. `not run` marks a check
whose tool isn't in the build or that needs the full run. `inconclusive`
marks a check whose sample was too small to decide.

```sh
mtx gates flash-crash-spcx --fast   # task files + a one-set, 12-session public ladder
mtx gates flash-crash-spcx          # + the image checks; 4 sets of the task's sessions
```

Full mode builds the task image and runs Harbor, so run it on a machine with
Docker. `--sets`, `--sessions` and `--ladder-summary <summary.json>` (reuse a
finished ladder) adjust a run.

## What it checks

| Section | Check | Source |
| --- | --- | --- |
| Task files | design, family, reference (the oracle is the reference bot's `lib.rs`), anchor, practice, instruction, README, CI and dataset, lint | `mtx task status` |
| Solvable and fair | the reference makes money on every set (it is the 1.0 line); doing nothing scores 0 | ladder |
| | the skill ladder keeps its order, on every set and in the bootstrap | ladder |
| | the best informed policy makes at least 2x the best non-reading one | ladder rungs |
| | the oracle scores 1.0 and nop 0 under Harbor | full: `just harbor-checks` |
| Graded correctly | the exploit bots stay inside their limits; no rung is refused or disqualified | ladder |
| | the component cheater scores 0, the process cheaters and sneaky bot aren't graded | full: `just harbor-checks` |
| | the agent's image holds no answer; a bot can't read the grader's keypairs; `time-left` tracks the clock if the task opts in | full: `just leak-audit`, `keypair-probe`, `clock-probe` |
| | replays are byte-identical: the reference, graded again on the same fresh hidden set (fast: 2 sessions twice; full: 6 sessions three times on a loaded machine) | `mtx grade --replay`, when the build has it |
| Realistic | the family's market matches its asset's latest `calibration/market/<asset>.*.toml` (its 300-session draw in both modes) | `mtx market-cal check <family>`, when the build has it |

## Fast mode

A fast ladder is one set of 12 sessions. It is drawn from a fixed public label
(`gates-fast-v1`, the ladder's `--public-seed`) rather than a fresh secret, so
every run grades the same sessions and a verdict doesn't flip with the draw.
It is never a hidden set: hidden sets come from fresh 256-bit secrets, and
`MTX_EVAL_SEED` is never used here.

Twelve sessions can't carry the full ladder's thresholds, so fast mode
loosens two of them. The bootstrap floor drops to 0.5: the order must still
hold on the set itself. An exploit bot over a limit fails only when the
sample shows it with 95% confidence. For its profitable share, that is a
one-sided binomial test at the limit: at 12 sessions and a 75% limit, only
12 of 12 fails. For its total against the `below` rung, it is a paired
bootstrap over the sessions. Anything short of that is `inconclusive at fast
size`, and the full ladder decides. The full ladder keeps the task's floor
and limits as measured.

## The signal gate

A task is about reading something: the flow tape, the regime, the funding
episode. Its ladder file (`bench/checks/ladder/<family>.json`) says which
rungs read it and which trade without:

```json
"gates": {
  "informed": ["told"],
  "non_reading": ["nop", "follow", "fade"]
}
```

The gate passes if the best informed rung's total PnL over all sets is at
least `min_ratio` (default 2) times the best non-reading rung's, and the
informed rung makes money. A losing or flat baseline counts as zero. If the
informed policy can't double a policy that ignores the signal, the task isn't
testing the signal.

## Adding a check

The replay and calibration checks run only when this build has
`mtx grade --replay` and `mtx market-cal check`; until then they report
`not run`. A failed realism check exits nonzero and its last lines go in the
report. Other checks go in `crates/cli/src/gates.rs`,
under the section of the bar they serve.

## CI

The `gates` job in `ci.yml` runs `mtx gates --fast` on each task that a pull
request or a push to main changes. Tasks that were deleted or renamed away are
skipped. It writes the reports to the job summary. Run by hand
(`workflow_dispatch`, optionally with a `task`), it covers every task and
uploads the reports as an artifact. The fast ladder takes about half an hour a
task on 16 cores, so the weekly run skips it, and the job stops after four
hours. It runs in the CI image (see [development](development.md)) as root,
because bots run as their own users.

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
| | a v2 task: both anchors named in its ladder file, the informed bot written; the verifier grades with `--scoring v2-headline` (v2 as the one score) and the instruction's scoring rules are current; it opts into the probe gate | `mtx task status` |
| Solvable and fair | the reference makes money on every set (it is the 1.0 line); doing nothing scores 0 | ladder |
| | the skill ladder keeps its order, on every set and in the bootstrap | ladder |
| | the best informed policy makes at least 2x the best non-reading one's graded total, or a $25 floor when every non-reading rung does worse | ladder rungs |
| | the oracle (the reference) scores skill 1.0, or under scoring v2 0.5 before the near-breach charge and any breach penalty (`score_v2_raw_before_charge`), with `reward` v2's score and no `skill` beside it; the charge, anchored on the informed bot's drawdowns, can take a reference that draws down deeper under 0.5 by design, and is reported beside it, not checked; nop scores 0 under Harbor | full: `just harbor-checks` |
| | a v2 task's reference is in the strength band against its informed bot: weighted as v2 weighs losses, 0.2 to 0.7 of it, never `anchor_floored`, per-session Sharpe at least 0.25 (fast mode: a miss is inconclusive, unless the reference loses money with confidence, which fails) | ladder, per-session PnL |
| | no task's verifier tells the graded bot anything (`--tell-latent`, `--tell-regime`) | `tests/test.sh` |
| | a v2 task's verifier runs its informed bot on the hidden set: `tests/test.sh` draws its set, grades only a component and passes `--informed /opt/mtx/anchors/<anchor>-informed`, which `bench/image/anchors.txt` builds from the ladder's informed anchor, and that bot is written (v2 doesn't fall back to the hindsight ceiling) | `tests/test.sh`, `anchors.txt` |
| Graded correctly | the exploit bots stay inside their limits (a pushing bot that moved the mark too little is `no_power`, and fails); no rung is refused or disqualified | ladder |
| | the probe bots land in their scoring v2 bands, graded with the task's own v2 settings (the verifier's `--loss-weight`, `--risk-weight`, `--anchor-floor` and `--informed`), and a probe with no power (a breacher that never breached) fails | ladder, when the task declares `probes` (`bench/checks/probes/README.md`) |
| | the component cheater scores 0, the process cheaters and sneaky bot aren't graded | full: `just harbor-checks` |
| | the agent's image holds no answer, and a guarded task's `mtx` offers only the allowlisted commands; a guarded task's agent can't reach ghcr.io or github.com; a bot can't read the grader's keypairs; `time-left` tracks the clock if the task opts in | full: `just leak-audit <task>`, `network-probe <task>`, `keypair-probe`, `clock-probe` |
| | replays are byte-identical: the reference, graded again on the same fresh hidden set (fast: 2 sessions twice; full: 6 sessions three times on a loaded machine) | `mtx grade --replay`, when the build has it |
| Realistic | the family's market matches its asset's latest `calibration/market/<asset>.*.toml` (its 300-session draw in both modes) | `mtx market-cal check <family>`, when the build has it |

## Fast mode

A fast ladder is one set of 12 sessions. It is drawn from a fixed public label
(`gates-fast-v1`, the ladder's `--public-seed`) rather than a fresh secret, so
every run grades the same sessions and a verdict doesn't flip with the draw.
It is never a hidden set: hidden sets come from fresh 256-bit secrets, and
`MTX_EVAL_SEED` is never used here.

A reference that loses money on a fast set fails only when the loss is
confident (a one-sided Student-t test at 5%: t under minus the critical value at
n - 1 degrees of freedom, 1.796 for twelve sessions); a loss within a short set's noise (market-making's -$38 over twelve
sessions, t = -0.9) is `inconclusive`. The full ladder fails any set where it
loses. The strength band follows the same rule. A noisy reference loss makes
inconclusive only the order rules that name `reference`; other broken rules
still fail. A reference whose edge is a few large winning sessions can fail
fast mode on its fixed public seed while passing the full ladder, because the
12 sessions may miss the big wins; the full ladder's verdict is the remedy.

Rungs are compared on the grade the task gives them: under v1 a rung with a
disqualified (breached) session totals 0, under a v2 task the sum of its v2
session values (loss-weighted, a breach floored; before the near-breach charge
and the breach-rate penalty), with the hindsight bound as the sum of the
reference's per-session bounds. The raw PnL (`raw_totals`) and the breach
counts (`breaches`) are in the summary as information. An exploit bot's multiple
of the reference is tested only when the ladder file sets `exploits.max_ratio`;
otherwise exploits are bounded by the `below` oracle.

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
episode. Its ladder file (`bench/tasks/<task>/checks/ladder.json`) says which
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

## Scoring v2's anchors

A task whose headline is scoring v2 (`headline = "v2"` in `[metadata.mtx]`)
names its two anchors in its ladder file, each a rung:

```json
"anchors": { "reference": "reference", "informed": "informed" }
```

The task-files checks refuse the task until both bots exist and the informed
one is no longer the scaffold's copy of the reference. The strength band
weighs each anchor's sessions as v2 does (losses `k` times, from the
verifier's flags) over all the ladder's sets, and needs the informed bot
ahead of the reference, the reference at 0.2 to 0.7 of it (`"band": [low,
high]` overrides), never under the anchor floor `φ`, and with a per-session
Sharpe of at least 0.25 (`"min_sharpe"`). Each anchor must be the task's own
bot: the reference rung runs `bench/tasks/<task>/reference` (the crate the
image builds into the verifier's anchor), untold; the informed rung runs
`checks/bots/informed` with `tell_latent: true`; neither sets `env` or
`may_breach` (the ladder leaves a `may_breach` rung's breaches out of its
clean runs). A v2 task is released on v2
alone, so either miss refuses it: there is no fallback to v1 or to the
hindsight ceiling. A v1 task (the four that move in 0.13) reports both v2
checks `not run`.

The verifier measures the 1 on the hidden set it grades: `mtx grade
--informed <dir>` runs the informed bot on the same sessions after the
reference and before the bot, and v2 anchors its 1 to that PnL. The informed
bot is handed each session's latent state as `MTX_REGIME`: only facts
knowable in principle (WAVE2_11 Part B, change 8), never the session's
realised draws. Each field is classified in `LATENT_FIELDS`
(crates/grader/src/generators.rs), and a test fails on any field that
changes from session to session without a class:

| Family | Field | Class | Why |
| --- | --- | --- | --- |
| funding-carry | `informed_share`, `lag_slots`, `hold_slots` | process | how the session's episodes are drawn and behave, not which ones come |
| funding-carry | `funding`, `cap_pct`, `noise_pct` | process | the funding process and the index's noise |
| weekend-gap | `informed`, `informed_account`, `worker`, `worker_account` | identity | whether an informed trader and a one-sided worker are seated, and from which of the grader's accounts |
| weekend-gap | `impact_pct`, `half_life_slots` | process | the impact curve |
| flash-crash | none | | the shock (direction, legs, aftermath) is the session's one draw |
| market-making | none | | whether an order is informed is the order's own draw |

Withheld as realised draws: a shock's direction, legs, aftermath, size and
timing; carry episodes (kind and timing); which way weekend-gap's worker
trades (`worker_side`), where the weekend took SPCX, anyone's order slots,
and every path. The bot is handed nothing, and a task's `tests/test.sh` may
not pass `--tell-latent` or `--tell-regime` (`bot-told-nothing`). The
informed bot's logs are read into the grader's memory and their directory
removed before the bot's first session, then written beside the others once
every bot has stopped; its sessions have their own keys and surfnets, so the
bot's sessions are the same with or without it. The ladder's informed rung
is told the same (`"tell_latent": true`). It costs one more bot run per
session.

The oracle installs `solution/src` into the agent's starter, which enables no
cargo features, so the solution must export its strategy component there
(`solution-exports`): `solution/src` holds an `mtx_kit::export!` call that no
positive `feature` gate hides. `not(feature = …)` and target gates are fine.
The check is static; it reads the source, not a build.

Gates grade each task on its own snapshot: `[metadata] snapshot` in its
`task.toml`, else the `MTX_SNAPSHOT` its verifier image sets, else the
Justfile's.

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

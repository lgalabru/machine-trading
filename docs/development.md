# Development

## Prerequisites

- Rust 1.97.1 (pinned in `rust-toolchain`), [`just`](https://github.com/casey/just),
  Docker with buildx, and [Harbor](https://harborframework.com)
  (`uv tool install harbor`).
- Surfpool comes through Cargo, from the branch of
  [surfpool#902](https://github.com/solana-foundation/surfpool/pull/902)
  (cheatcode locks, SDK snapshots, clock restore), until it ships in a
  release.
- The snapshot's `surfnet.json`, which is too big for git. `just
  snapshot-pull` copies it out of the published task image. To fork a fresh
  one instead, put a mainnet RPC URL in `.env` as `MTX_FORK_RPC_URL` and run
  `just snapshot-prepare`.

## Everyday loop

```sh
just bots                                          # build every bot's bin/bot
just grade bots/reference bench/tasks/phoenix-flash-crash/environment/practice/*.json
just test lint                                     # unit tests and clippy
```

`mtx grade` runs exactly what the verifier runs, and bot logs land in
`target/logs/`. Don't run heavy builds while grading: a starved machine
slows the grade.

## Run, review and publish a bench

```sh
just bench                                   # a batch, in jobs/bench/<batch>/
mtx bench review jobs/bench/<batch>          # browse it (or `just review …`)
mtx bench publish jobs/bench/<batch> 0.9.0   # keep it, in bench/results/0.9.0/
mtx bench review 0.9.0                       # browse a published release
mtx bench compare 0.9.0...0.8.0 [--open]     # skill by task and model, and the change
```

- **Review page:** a breadcrumb (`mtx › release › task › trial ›
  session`, with menus to switch release or task), the tasks as tiles, and
  an overview of the bench: a leaderboard of skill by model and task. A
  task adds its equity curves, with the models as cards on the right. Below
  them, each trial's sessions against the reference, the agent's
  trajectory, the bot it wrote, and a step-by-step timeline of every session
  (prices, orders, position, PnL and decoded transactions). Timelines need
  the detail files that the verifier writes with `mtx grade --details`.
- **Published releases:** small enough to commit, at a few hundred KB a
  trial. A release holds the results, reports, session detail files, the
  bot's source and the trajectory, gzipped. Publishing refuses any release
  that would hold the batch's `.seed` or an API key. The review page lets
  you switch between releases and compare them.

## Author and play scenarios

```sh
mtx scenario new "simulate a crash for SPCX" -a claude-code   # writes scenario.json
mtx scenario play "simulate a crash for SPCX" -a claude-code  # authors it, then plays it
mtx scenario play scenario.json                               # replays a saved one
```

- **`new`:** a coding agent (`claude-code` or `codex`, over
  [ACP](https://agentclientprotocol.com)) writes a
  [Surfpool scenario](https://github.com/solana-foundation/surfpool/tree/main/crates/core/src/scenarios).
  The agent works against a surfnet restored from the snapshot. `mtx` checks
  what it writes and sends problems back until the scenario passes.
- **`play`:** serves a page with a chart per venue and dimension, and every
  override on a timeline. It has Play, Pause, Reset and Stop, plus a
  workbench with the surfnet's URLs and a live websocket stream. `-v` shows
  the logs instead of progress lines.

## Change a task

```sh
just ladder <family>                   # the skill ladder on fresh hidden sets
just image && just harbor-checks       # oracle skill 1; nop, cheater, sneaky 0
just leak-audit                        # the agent's image holds no answer
harbor check bench/tasks/<task>        # LLM review against Harbor's rubric
```

After changing `bots/reference`, run `just solution` and recalibrate the
practice scenarios (`just calibrate bots/reference FILES`). Hidden sets need
no calibration, because the verifier grades the reference on them every
time. Record the change in [the changelog](../bench/tasks/CHANGELOG.md).

## Add a task

Pick an issue labelled
[`new task`](https://github.com/lgalabru/machine-trading/issues?q=label%3A%22new+task%22),
and follow the [`trading-task-design`](../skills/trading-task-design/SKILL.md)
skill, which Claude Code loads in this repo. Write the task's README to
[the template](../bench/tasks/README.md#task-readmes).

## Release

Bump `tag` in the Justfile, build the images and run the checks on them
(`just image && just harbor-checks`), commit, then:

```sh
just release     # push both images (amd64 + arm64, ~40 min on Apple silicon),
                 # pin every task to its digest, lint, sync digests, publish
```

Commit the pins and digests it writes. CI checks every push (fmt, clippy,
tests, lint, calibrated scenarios), and once a week it builds the images and
runs the oracle through Harbor.

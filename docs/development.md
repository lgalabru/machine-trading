# Development

## Prerequisites

- Rust 1.97.1 (pinned in `rust-toolchain`), [`just`](https://github.com/casey/just),
  Docker with buildx, and [Harbor](https://harborframework.com)
  (`uv tool install harbor`).
- Surfpool comes through Cargo, from the branch of
  [surfpool#902](https://github.com/solana-foundation/surfpool/pull/902)
  (cheatcode locks, SDK snapshots, clock restore), until it ships in a
  release.
- Nothing for the snapshot: its accounts are committed gzipped
  (`snapshots/<name>/surfnet.json.gz`, under 3 MB), and `mtx` decompresses
  them into a temp-directory cache the first time a surfnet starts. To fork
  a fresh one, put a mainnet RPC URL in `.env` as `MTX_FORK_RPC_URL` and run
  `just snapshot-prepare snapshots/<name>`, which writes the `.gz`.

## Everyday loop

```sh
just bots                                          # build every bot's bin/bot
just grade bots/reference bench/tasks/flash-crash-spcx/environment/practice/*.json
just test lint                                     # unit tests and clippy
```

`mtx grade` runs exactly what the verifier runs, and bot logs land in
`target/logs/`. Don't run heavy builds while grading: a starved machine
slows the grade.

## Write a bot

`mtx bot` is the developer's side of grading, without Harbor or Docker:

```sh
mtx bot new ~/my-bot                       # from bots/starter, outside this checkout
mtx bot practice --bot ~/my-bot            # just build, then mtx grade on the practice set
mtx bot practice --task <name> --bot ~/my-bot   # another task in bench/tasks
mtx bot check ~/my-bot                     # a bot.wasm that is a component, not stale
mtx bot pack ~/my-bot -o my-bot.tar.gz     # the source and a manifest, no builds
```

- `new` starts from `bots/starter`, a `Strategy` on `mtx-kit`'s run
  loop (`mtx_kit::run`): the same starter the task images hand to
  agents. It depends on
  `mtx-kit` by path to the checkout `mtx` was built
  from, or with `--git` by git at the commit it was built from, and copies
  the workspace's `Cargo.lock`. A bot inside this checkout is refused: the
  workspace's `bots/*` would claim it.
- `practice` runs the same `grade::grade` as the verifier, on the task's
  snapshot and `environment/practice` scenarios, with logs in
  `<bot>/target/mtx-practice-logs`. `--json` prints the full report.
- A bot built on macOS is a macOS binary: it practises here, but the bench
  grades Linux builds. The guide, with the rules a bot runs under, is [specs/guides/bots/writing-a-bot.md](../specs/guides/bots/writing-a-bot.md).

## Run, review and publish a bench

A bench runs a released dataset version: `just bench` pulls the tasks from
Harbor's registry, not from this checkout. Release the version first
([Release](#release)); `harbor dataset download
solana-foundation/machine-trading@<version>` says whether it is there.

```sh
just bench "claude-opus-5-5 openai/<model>" 5 3 0.11.0   # models, attempts, at a time, version
just review jobs/bench/<batch>                          # browse it
just publish jobs/bench/<batch> 0.11.0                  # keep it, in bench/results/0.11.0/
git add bench/results/0.11.0 && git commit && git push  # the push publishes the website
```

1. **Run.** `just bench` writes `jobs/bench/<batch>/`: the plan
   (`batch.json`: models, harnesses and versions, attempts, the commit),
   the batch's secret (`.seed`, owner-only), and one Harbor job per model.
   Every trial of a batch is graded on the same hidden set, drawn from the
   secret inside the verifier. At the end it checks the secret is in no job
   file, then prints the report (`bench/report.py`): skill by model and task,
   with 90% intervals.
   - **Machine:** an idle one. Load slows the surfnets' clocks, and the
     grader fails sessions it can't run on time. A trial takes about 8 CPUs
     while it grades; market-making's verifier alone runs about an hour.
   - **Keys:** `ANTHROPIC_API_KEY` and `OPENAI_API_KEY`, for the providers
     it runs, from `.env` or `~/.machine-trading.env`.
   - **Several machines:** give each the same secret, so every trial sees
     the same hidden set: `MTX_EVAL_SEED="$(cat <batch>/.seed)" just bench
     …`. Then copy each machine's Harbor job directories
     (`jobs/bench/<batch>/<job>/`) into the first batch, note it in
     `batch.json`, and run `python3 -I bench/report.py <batch>`. Start runs
     on one machine a minute apart: two at once can race on Harbor's task
     cache.
   - **A void trial:** rerun it on the same set with the batch's secret.
2. **Review.** Look at what stands out before you publish: read a winning
   run's bot, and check its report for refused calls and late slots. A score
   far above the others is a leak until it isn't.
3. **Publish.** `just publish <batch> <version>` writes
   `bench/results/<version>/` and refuses if any file holds the batch's
   secret or an API key. `--force` replaces a release. See
   [bench/results](../bench/results/README.md) for what a release keeps.
4. **Commit and push.** The Pages workflow (`.github/workflows/specs.yml`)
   rebuilds the website whenever `bench/results` changes, so pushing the
   release publishes it. A deploy that times out is rerun with
   `gh run rerun <id> --failed`.

`mtx bench review <version>` browses a published release, and
`mtx bench compare 0.11.0...0.10.0 [--open]` compares skill by task and
model between two.

- **Review page:** a breadcrumb (`mtx › release › task › run › scenario`),
  the tasks as a column of tiles, and an overview: a leaderboard of skill
  by model and task, and skill against cost. A task shows its equity
  curves and one grid of runs × scenarios. A run is one model's attempt (a
  Harbor trial); a scenario is one hidden market session every run shares.
  A row opens the run in a side panel: its numbers, then every scenario as
  a small PnL chart beside the picked one's timeline, the agent (time and
  token breakdowns, where its time went, context and cost over the run),
  the bot and the logs. A column replays that scenario for every run.
  Timelines need the detail files the verifier writes with
  `mtx grade --details`.
- **The website:** every published release is on GitHub Pages at
  `/bench/`, as the same page with its data written out as files
  (`mtx bench export <dir>`; the browser inflates the gzipped ones).
- **Published releases:** about 7 MB a trial, most of it the bot's
  sessions step by step, which the review page draws. 0.11.0, 40 trials,
  is 293 MB in the repository and about 450 MB on the website with 0.10.0.
  GitHub Pages serves at most 1 GB, so the session files will have to move
  out of the repository within a release or two.

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
just keypair-probe                     # a bot can't read the grader's keypairs
harbor check bench/tasks/<task>        # LLM review against Harbor's rubric
```

After changing `bots/reference`, run `just solution` and recalibrate the
practice scenarios (`just calibrate bots/reference FILES`). Hidden sets need
no calibration, because the verifier grades the reference on them every
time. Record the change in [the changelog](../bench/tasks/CHANGELOG.md).

The scenario file format, v1 and v2, is in [scenario-format.md](scenario-format.md).

## Add a task

It's guided. In Claude Code, in this repository, ask for a new task: the
`mtx` MCP server (`.mcp.json`; `just install mtx` first) interviews you
with forms, scaffolds the task and its reference bot, then reports what's
left after every step (`task_status`). From a terminal: `mtx task new`
(every answer is a flag), `mtx task status <name>`, `mtx task guide <step>`.

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

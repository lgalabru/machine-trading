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
  `just snapshot-build <name>`, which rebuilds the `.gz` from
  `snapshots/<name>/recipe.toml` ([snapshot recipes](snapshot-recipes.md)).

## Everyday loop

```sh
just bots                                          # build every bot's bin/bot
just grade bench/tasks/flash-crash-spcx/reference bench/tasks/flash-crash-spcx/environment/practice/*.json
just test lint                                     # unit tests and clippy
```

`mtx grade` runs exactly what the verifier runs, and bot logs land in
`target/logs/`. Don't run heavy builds while grading: a starved machine
slows the grade.

A grade replays exactly: the same bot on the same hidden set (the same
`MTX_EVAL_SEED`) gets the same report, byte for byte but for wall times.
The sessions' keypairs are derived from that secret, so their transactions
and signatures repeat too. A grade of scenario files has no secret and
draws fresh keys, so its own replays agree but two separate grades of a
component that reads signatures can differ in fuel. Load slows a grade, not
its result: sessions run on no clock, trusted bots (the reference,
`--trusted`) get up to two minutes a turn, and the grader's own requests
wait up to ten minutes each, a read that got no answer being made again.
The limit: a request that fails outright rather than slowly (a surfnet that
died, a refused connection) still fails the grade, and a taker order lost
that way counts as failed.

`--replay N` checks it: it runs the grade N times (with `--replay-load
<threads>` busy threads alongside) and fails at the first session report,
the reference's included, that came out differently.

```sh
MTX_EVAL_SEED=... mtx grade --draw flash-crash --count 6 --components-only \
  --reference /opt/mtx/anchors/flash-crash --bot bench/tasks/flash-crash-spcx/reference \
  --snapshot snapshots/phoenix-spcx-2026-10-29 --replay 3 --replay-load 16
```

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
just publish jobs/bench/<batch> 0.11.0                  # upload it; writes bench/results/0.11.0/release.json
git add bench/results/0.11.0 && git commit && git push  # the push publishes the website
```

1. **Run.** `just bench` writes `jobs/bench/<batch>/`: the plan
   (`batch.json`: models, harnesses and versions, attempts, the commit),
   the batch's secret (`.seed`, owner-only), and one Harbor job per model.
   Every trial of a batch is graded on the same hidden set, drawn from the
   secret inside the verifier. At the end it checks the secret is in no job
   file, then prints the report (`bench/report.py`): skill by model and task,
   with 90% intervals.
   - **Machine:** any; an idle one is faster. Sessions run on no clock, so
     load makes a grade slower, not different (see `--replay` above for the
     limit). A trial takes about 8 CPUs
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
   run's bot, and check its report for refused calls. A score
   far above the others is a leak until it isn't.
3. **Publish.** `just publish <batch> <version>` compacts the batch into a
   release and refuses if any file holds the batch's secret or an API key.
   It uploads the release, as one archive, to the
   [results dataset](https://huggingface.co/datasets/solana-foundation/machine-trading-results)
   and writes `bench/results/<version>/release.json`: the dataset revision,
   the archive's sha256 and the release's scores. It needs the `hf` CLI
   (`uv tool install huggingface_hub`) and `HF_TOKEN`, a token that may
   write to the `solana-foundation` org. `--force` replaces a release;
   `--local` keeps the whole release in `bench/results/<version>/` and
   uploads nothing. See [bench/results](../bench/results/README.md) for
   what a release keeps.
4. **Commit and push.** The Pages workflow (`.github/workflows/specs.yml`)
   rebuilds the website whenever `bench/results` changes, fetching each
   release from the dataset, so pushing the `release.json` publishes it. A deploy that times out is rerun with
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

One command runs them all and writes one report: `mtx gates <task> --fast`
for the cheap checks, `mtx gates <task>` for the full set (see
[gates](gates.md)). The pieces, one by one:

```sh
just ladder <task>                     # the skill ladder on fresh hidden sets
just image && just harbor-checks       # oracle skill 1; nop, cheater, sneaky 0
just leak-audit                        # the agent's image holds no answer
just keypair-probe                     # a bot can't read the grader's keypairs
harbor check bench/tasks/<task>        # LLM review against Harbor's rubric
```

After changing a task's `reference/`, run `mtx task sync` (it copies the
reference into the task's `solution/`) and recalibrate the practice
scenarios (`just calibrate bench/tasks/<task>/reference FILES`). Hidden sets need
no calibration, because the verifier grades the reference on them every
time. Record the change in [the changelog](../bench/tasks/CHANGELOG.md).

The scenario file format, v1 and v2, is in [scenario-format.md](scenario-format.md).

## Add a task

It's guided. In Claude Code, in this repository, ask for a new task: the
`mtx` MCP server (`.mcp.json`; `just install mtx` first) interviews you
with forms, scaffolds the task and its reference crate, then reports what's
left after every step (`task_status`). From a terminal: `mtx task new`
(every answer is a flag), `mtx task status <name>`, `mtx task guide <step>`.

A task is one folder, `bench/tasks/<name>/`:

| | |
| --- | --- |
| `task.toml` | Harbor's task, and `[metadata.mtx]`: the scenario `generator` (a name in `mtx_grader::generators`), the text of the README tables, the leak-audit needles, an `anchor` name if two tasks share a generator |
| `reference/` | The reference bot's crate (a workspace member): the verifier scores against it |
| `solution/` | The oracle's copy of `reference/`, written by `mtx task sync` |
| `checks/` | `ladder.json`, `rungs/` (rungs that run a bot), `bots/` (ladder bots' crates) and `exploits/` (the frozen exploit bots) |
| `environment/`, `tests/`, `instruction.md`, `README.md`, `DESIGN.md` | As the [task template](../bench/tasks/README.md#task-readmes) |

A task that draws an existing generator touches no shared file: copy a
task's folder, edit it, and run `mtx task sync`. It writes every list that
names the tasks (`bench/tasks/tasks.just` that the Justfile imports,
`bench/image/anchors.txt` that the image build reads, the dataset's
`[[tasks]]`, the README tables, each `solution/`), and `mtx task check` (and
`cargo test`) fails when one is stale. A new generator is a family in
`crates/grader` and one entry in `GENERATORS` (`generators.rs`). `harbor sync`
rewrites the dataset's digests at release; run `mtx task sync` after it, and
commit.

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
tests, lint, calibrated scenarios) and runs `mtx gates --fast` on the tasks a
pull request or push changes. Once a week it builds the images and runs the
oracle through Harbor.

The Rust jobs run in a prebuilt image, `ghcr.io/<owner>/machine-trading-ci`
(`bench/ci/Dockerfile`). `ci-image.yml` builds it at the start of a CI run
whenever `bench/ci` or `rust-toolchain` changes, tagged with their hash.
Until anyone can pull it, the jobs run `bench/ci/setup.sh` on the plain Rust
image instead, which is slower but just as correct. One-time step after the
first push to main: in the repository owner's GitHub packages, open
`machine-trading-ci`, then Package settings, and change its visibility to
public. Pull requests from forks use it from then on.

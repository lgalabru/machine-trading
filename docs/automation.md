# Release and bench automation

How a version of the benchmark goes from a tag to published results: who
does what, in what order, what runs at the same time, and where each secret
lives. The commands are in [development.md](development.md#automation); why
GitHub reaches the bench machine over SSH is
[ADR 0002](adr/0002-bench-machine-over-ssh.md).

## The whole path

Three stages, each started by a person: a release, a bench, a publish. A
bench runs a released version only, so the tasks an agent sees are the ones
Harbor serves to everyone.

```mermaid
flowchart LR
  subgraph release["1. Release (release.yml)"]
    tag["git push vX.Y.Z"] --> images["Images<br/>amd64 and arm64, at once"]
    images --> approve1{{"Approve<br/>release"}}
    approve1 --> harbor["Harbor dataset<br/>machine-trading@X.Y.Z"]
    harbor --> pins["Branch release/X.Y.Z<br/>(pins, to merge)"]
  end
  subgraph bench["2. Bench (bench.yml start)"]
    approve2{{"Approve<br/>bench"}} --> run["Bench machine: just bench<br/>models × tasks × attempts"]
  end
  subgraph publish["3. Publish (bench.yml publish)"]
    approve3{{"Approve<br/>bench"}} --> hf["Hugging Face<br/>machine-trading-results"]
    hf --> results["Branch results/X.Y.Z-batch<br/>(release.json, to merge)"]
    results --> site["Pages site<br/>rebuilt on merge"]
  end
  harbor -. "pulled by every trial" .-> run
  run -. "batch on the bench machine" .-> approve3
```

`status` (`bench.yml status`) can be asked at any time, without approval.

## 1. Release

A tag on main starts it. The image jobs need no secret, so they start at
once; the publish job is the only one that holds secrets, and it waits for
a reviewer.

```mermaid
sequenceDiagram
  autonumber
  actor Dev as Maintainer
  participant GH as GitHub Actions
  participant A as Runner amd64
  participant B as Runner arm64
  participant Stage as ghcr.io/lgalabru/<br/>machine-trading-release
  participant Rel as ghcr.io/solana-foundation/<br/>machine-trading
  participant H as Harbor

  Dev->>GH: git push vX.Y.Z (on main)
  GH->>GH: check: tag on main, = Justfile tag = dataset.toml version,<br/>each oracle is its task's reference
  par amd64, natively
    GH->>A: build runtime, then dev
    A->>Stage: push X.Y.Z-runtime-amd64, X.Y.Z-dev-amd64
  and arm64, natively
    GH->>B: build runtime, then dev
    B->>Stage: push X.Y.Z-runtime-arm64, X.Y.Z-dev-arm64
  end
  GH-->>Dev: environment "release" waits for review
  Dev->>GH: approve
  GH->>Rel: imagetools create X.Y.Z-runtime, X.Y.Z<br/>(refused if the tag exists)
  Note over GH,Rel: just release-publish
  GH->>Rel: pin-image: FROM digests, then the 8 task images<br/>(4 tasks × environment, verifier), both arches
  GH->>GH: lint, harbor sync, no task at sha256:000…, mtx task check
  GH->>H: publish 4 tasks + dataset, tag each X.Y.Z
  GH->>H: download machine-trading@X.Y.Z: holds every task?
  GH->>GH: push release/X.Y.Z (the pins)
  GH-->>Dev: summary: link to open the PR
```

What runs at the same time:

| Step | In parallel | Why it can |
| --- | --- | --- |
| Image builds | amd64 and arm64, on two native runners | separate machines, separate caches; merged by digest afterwards |
| Within a build | `cargo` with all the runner's cores | one build per machine |
| Task images | the 8 (4 tasks × environment, verifier), one after another, each for both arches | small: a `COPY` on the pinned image |
| Harbor publish | the 4 tasks' uploads at once (`--concurrency 50`) | independent packages |

From a laptop, `just release` does the same in one go; the images then
build for both arches on the laptop, amd64 under emulation (about 40
minutes on Apple silicon). A buildx node on the bench machine builds amd64
natively instead, in minutes: `docker buildx create --name mtx-amd64
--platform linux/amd64 ssh://<bench machine>`, then one `buildx build --platform
linux/amd64` there and one `linux/arm64` locally, merged with `docker buildx
imagetools create`. The registry login stays on the laptop: buildx lends it
to the remote builder for the build only.

## 2. Bench

```mermaid
sequenceDiagram
  autonumber
  actor Dev as Maintainer
  participant GH as GitHub Actions<br/>(bench.yml)
  participant D as Bench machine:<br/>dispatcher
  participant J as just bench
  participant HB as harbor run<br/>(one per model)
  participant T as Trials<br/>(n at a time)

  Dev->>GH: action=start, version, models, attempts, concurrent
  GH->>GH: check every input
  GH-->>Dev: environment "bench" waits for review
  Dev->>GH: approve
  GH->>D: ssh (key pinned to the dispatcher):<br/>start <main sha> X.Y.Z k n limit models…
  D->>D: check again, sha on origin/main, no bench running,<br/>clean checkout → detach at sha
  D->>J: start, detached (setsid nohup)
  D-->>GH: batch=YYYYMMDD-HHMMSS-x1
  GH-->>Dev: summary: batch, status and publish commands
  J->>J: batch dir, .seed (one secret per batch), batch.json (the plan)
  loop each model, one after the other
    J->>HB: -d machine-trading@X.Y.Z -k attempts -n concurrent
    HB->>T: models × tasks × attempts trials, n at a time
  end
  J->>J: secret in no job file? report (skill by model and task)
  Dev->>GH: action=status (any time, no review)
  GH->>D: ssh (read-only key): status [batch]
  D-->>GH: running, trials finished, report when done
```

### One trial

Each trial is two containers, one after the other, on their own Docker
networks. The agent never sees the verifier's container or the hidden set;
the verifier never reaches a network.

```mermaid
sequenceDiagram
  autonumber
  participant HB as harbor run
  participant E as Environment container<br/>(4 CPUs, 6 GB, public network)
  participant Ag as Coding agent<br/>(Codex or Claude Code, pinned)
  participant V as Verifier container<br/>(4 CPUs, 6 GB, no network)
  participant G as mtx grade

  HB->>E: start X.Y.Z-<task> (pinned digest)
  HB->>Ag: the task's instructions
  Ag->>E: write /app/bot, build bot.wasm, practice scenarios
  Note over Ag,E: up to 1 hour (agent timeout × limit)
  HB->>V: start X.Y.Z-<task>-verifier, with /app/bot and MTX_EVAL_SEED
  V->>G: draw 72 hidden sessions from the seed
  par the reference, 4 sessions at a time
    G->>G: reference bot on each session
  end
  par the bot, 2 sessions at a time (memory: 1 GiB each)
    G->>G: bot.wasm on each session, own surfnet each
  end
  G-->>HB: reward.json, report.json, sessions/
  HB->>HB: result.json
```

### What runs at the same time

| Level | In parallel | Limit | Set by |
| --- | --- | --- | --- |
| Models | one after the other in `just bench` | — | the recipe's loop |
| Trials of a model | `concurrent` at a time | the machine: 4 CPUs and 6 GB per container | `-n`, `bench.yml concurrent` |
| A trial's two phases | no: agent, then verifier | — | Harbor |
| Reference sessions | 4 at a time | trusted, small | `--anchor-jobs 4` |
| Bot sessions | 2 at a time (up to 4) | 1 GiB per strategy in 6 GB | `--jobs 4`, capped by memory |
| Sessions vs each other | isolated | a component has no files and no network | the grader |

Grades don't depend on load: sessions run on no clock, so a trial graded
alongside 39 others gets the report it would get alone (`mtx grade
--replay` checks it). Concurrency only changes how long a batch takes.

How long a batch takes, roughly: the agent's hour, then grading (about 15
minutes for flash-crash, longer for market-making), times the number of
waves (`ceil(models × tasks × attempts / concurrent)` per model, models in
turn). Two models × four tasks × five attempts at `concurrent` 20 is one
wave per model.

Each trial uses two Docker networks while it runs. Docker's default address
pools hold about 31, so the bench machine's Docker daemon is given a larger
private range (`default-address-pools`, with room for hundreds of /24s).
Without it, a batch of 40 trials fails at the 16th with "all predefined
address pools have been fully subnetted".

## 3. Publish

```mermaid
sequenceDiagram
  autonumber
  actor Dev as Maintainer
  participant GH as GitHub Actions<br/>(bench.yml)
  participant D as Bench machine:<br/>dispatcher
  participant M as mtx bench publish
  participant HF as Hugging Face<br/>machine-trading-results
  participant Repo as GitHub repo
  participant P as Pages

  Dev->>GH: action=publish, batch, version
  GH-->>Dev: environment "bench" waits for review
  Dev->>GH: approve
  GH->>D: ssh: publish <batch> X.Y.Z [force]
  D->>D: no bench running, load HF_TOKEN
  D->>M: into a copy of bench/results (the checkout stays clean)
  M->>M: compact the batch, refuse if any file holds the seed or an API key
  M->>HF: X.Y.Z/release.tar, trials.jsonl, sessions.jsonl
  M->>HF: check the revision holds exactly those
  M->>HF: README.md (the card: leaderboard, releases)
  D-->>GH: release.json (revision, sha256, scores)
  GH->>GH: release.json is a stored X.Y.Z release?
  GH->>Repo: push results/X.Y.Z-<batch>
  GH-->>Dev: summary: link to open the PR
  Dev->>Repo: merge
  Repo->>P: specs.yml: mtx bench export fetches every archive from HF
```

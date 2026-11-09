# Loads MTX_FORK_RPC_URL for snapshot-prepare.
set dotenv-load := true

snapshot := "phoenix-spcx-2026-10-29"
registry := "ghcr.io/solana-foundation"
image := "machine-trading"
# Bump on every image change that tasks should pick up.
tag := "0.12.0"
tasks := "bench/tasks"
# Each task and the bot its oracle runs, as `<task>=<bot>`.
references := "flash-crash-spcx=reference market-making-spcx=maker funding-carry-spcx=carry"
# The harness versions `just bench` runs, pinned so results compare: each
# provider's own coding agent, Claude Code for Anthropic models and Codex for
# OpenAI's.
claude_code := "2.1.312"
codex := "0.166.0"

default:
    @just --list

fmt:
    cargo +nightly fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets -- -D warnings
    cargo clippy -p mtx-kit --all-features --all-targets -- -D warnings

test:
    cargo test --workspace
    cargo test -p mtx-kit --all-features

# Run mtx from source, like `cargo run --release --`. Arguments keep their
# quoting, and a leading `mtx` is optional: `just run mtx scenario play "..."`.
[positional-arguments]
run *args:
    #!/usr/bin/env bash
    set -euo pipefail
    [ "${1:-}" = mtx ] && shift
    exec cargo run --release -- "$@"

# Install a binary into ~/.cargo/bin: `just install` or `just install mtx`.
install bin="mtx":
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{bin}}" in
      mtx) cargo install --locked --path crates/cli ;;
      *) echo "unknown binary {{bin}} (have: mtx)"; exit 1 ;;
    esac

# Fork mainnet and write a pinned Phoenix snapshot.
snapshot-prepare out:
    cargo run --release -- snapshot prepare --out {{out}}

# Replay a snapshot offline and trade through a scripted crash.
snapshot-verify dir=("snapshots/" + snapshot):
    cargo run --release -- snapshot verify --dir {{dir}}

# Build the bots' bin/bot.
bots:
    bots/starter/build.sh
    bots/reference/build.sh
    bots/maker/build.sh
    bots/dip-buyer/build.sh
    bots/carry/build.sh
    bots/carry-gbm/build.sh

# Draw scenarios from a task's family: just scenarios DIR hidden 12 SEED market-making
scenarios out prefix count seed family="flash-crash":
    cargo run --release -- scenario generate --family {{family}} --out {{out}} --prefix {{prefix}} --count {{count}} --seed {{seed}}

# Record a reference bot's PnL in each scenario file: just calibrate bots/maker FILES...
calibrate bot +files: bots
    cargo run --release -- calibrate --snapshot snapshots/{{snapshot}} --bot {{bot}} --logs target/logs/calibrate {{files}}

# Grade a bot directory on scenario files, as the verifier does.
grade bot +files:
    cargo run --release -- grade --snapshot snapshots/{{snapshot}} --bot {{bot}} --logs target/logs/grade {{files}}

# The task images, for this machine only: dev (agents) and runtime
# (verifiers). Tagged as the registry names, so task Dockerfiles that aren't
# pinned yet build against them.
# `surfpool=DIR` builds against a local Surfpool checkout's committed HEAD
# (its git archive, so no build artifacts) instead of the git sources.
image surfpool="":
    #!/usr/bin/env bash
    set -euo pipefail
    context=()
    if [ -n "{{surfpool}}" ]; then
      src="$PWD/target/surfpool-src"
      rm -rf "$src" && mkdir -p "$src"
      git -C "{{surfpool}}" archive HEAD | tar -x -C "$src"
      context=(--build-context "surfpool=$src")
    fi
    docker buildx build -f bench/image/Dockerfile --target runtime ${context[@]+"${context[@]}"} \
        -t {{registry}}/{{image}}:{{tag}}-runtime --load .
    docker buildx build -f bench/image/Dockerfile --target dev ${context[@]+"${context[@]}"} \
        -t {{registry}}/{{image}}:{{tag}} --load .

# Build both images for amd64 and arm64 and push them. Pin the tasks to the
# pushed digests afterwards (`just pin-image`).
# amd64 builds under emulation on Apple silicon: expect the first run to be slow.
push-image jobs="3":
    docker buildx build -f bench/image/Dockerfile --target runtime \
        --build-arg CARGO_BUILD_JOBS={{jobs}} --platform linux/amd64,linux/arm64 \
        -t {{registry}}/{{image}}:{{tag}}-runtime --push .
    docker buildx build -f bench/image/Dockerfile --target dev \
        --build-arg CARGO_BUILD_JOBS={{jobs}} --platform linux/amd64,linux/arm64 \
        -t {{registry}}/{{image}}:{{tag}} --push .

# Point every task at the pushed images, by digest: environments at dev,
# verifiers at runtime.
pin-image:
    #!/usr/bin/env bash
    set -euo pipefail
    pin() {
      local ref="$1"; shift
      # The registry occasionally fails a lookup; retry before giving up.
      local digest="" attempt
      for attempt in 1 2 3 4 5; do
        digest="$(docker buildx imagetools inspect "$ref" 2>/dev/null | awk '/^Digest:/ { print $2; exit }')" && [ -n "$digest" ] && break
        sleep $((attempt * 5))
      done
      [ -n "$digest" ] || { echo "no digest for $ref"; exit 1; }
      echo "$ref@$digest"
      sed -i '' -E "s|^FROM {{registry}}/{{image}}[-a-z]*:.*|FROM $ref@$digest|" "$@"
    }
    pin {{registry}}/{{image}}:{{tag}} {{tasks}}/*/environment/Dockerfile
    pin {{registry}}/{{image}}:{{tag}}-runtime {{tasks}}/*/tests/Dockerfile

# Release: push the image, pin every task to it, check, and publish the
# dataset. Bump `tag` first. Run `just harbor-checks` on the new image before
# releasing it.
release: check-solution
    #!/usr/bin/env bash
    set -euo pipefail
    git diff --quiet && git diff --cached --quiet || { echo "commit or stash your changes first"; exit 1; }
    # The images build from the working tree: if it moves during the build
    # (0.5.0 shipped a half-renamed dev image that way), stop before pinning.
    head="$(git rev-parse HEAD)"
    just push-image
    [ "$(git rev-parse HEAD)" = "$head" ] && git diff --quiet && git diff --cached --quiet \
      && [ -z "$(git ls-files --others --exclude-standard)" ] \
      || { echo "the tree changed while the images built; release from a clean worktree"; exit 1; }
    just pin-image
    for task in {{tasks}}/*/; do harbor lint "$task" --select ALL; done
    harbor sync {{tasks}}
    # Publishing a dataset asks for confirmation, even with -y.
    printf 'y\n' | harbor publish {{tasks}} --public
    # Harbor addresses versions by revision; tag each with its declared version.
    version_of() { awk -F'"' '/^version *=/ { print $2; exit }' "$1"; }
    name_of() { awk -F'"' '/^name *=/ { print $2; exit }' "$1"; }
    for task in {{tasks}}/*/; do
      harbor version tag "$(name_of "$task/task.toml")@latest" "$(version_of "$task/task.toml")" --force
    done
    harbor version tag "$(name_of {{tasks}}/dataset.toml)@latest" "$(version_of {{tasks}}/dataset.toml)" --force
    echo "released {{registry}}/{{image}}:{{tag}} and :{{tag}}-runtime; commit the pins and digests"

# Benchmark agents on the published dataset, on an idle machine: other load
# slows the surfnets' clocks, and the grader fails sessions it can't run on
# time. Models are `provider/model`, `anthropic/` when there's no provider:
# Anthropic models run in Claude Code, OpenAI models in Codex. Reads
# ANTHROPIC_API_KEY and OPENAI_API_KEY (for the providers it runs) from .env
# or ~/.machine-trading.env. Results go to jobs/bench/; read them with
# `mtx bench review`. Arguments are positional:
# just bench "claude-opus-5-5 openai/<model>" 5 3 0.11.0
bench models="claude-sonnet-5-5 claude-opus-5-5 claude-fable-5-1" k="5" n="3" version="" limit="1":
    #!/usr/bin/env bash
    set -euo pipefail
    set -a
    [ -f .env ] && . ./.env
    [ -f ~/.machine-trading.env ] && . ~/.machine-trading.env
    set +a
    for model in {{models}}; do
      case "$model" in
        openai/*) [ -n "${OPENAI_API_KEY:-}" ] || { echo "OPENAI_API_KEY is not set, for $model"; exit 1; } ;;
        */*) [ "${model%%/*}" = anthropic ] || { echo "$model: only anthropic/ and openai/ models have a pinned harness"; exit 1; }
          [ -n "${ANTHROPIC_API_KEY:-}" ] || { echo "ANTHROPIC_API_KEY is not set, for $model"; exit 1; } ;;
        *) [ -n "${ANTHROPIC_API_KEY:-}" ] || { echo "ANTHROPIC_API_KEY is not set, for $model"; exit 1; } ;;
      esac
    done
    version="{{version}}"
    dataset="solana-foundation/machine-trading${version:+@$version}"
    batch="jobs/bench/$(date +%Y%m%d-%H%M%S)-x{{limit}}"
    mkdir -p "$batch"
    # One secret per batch: every trial is graded on the same hidden set,
    # drawn inside its verifier. Kept (owner-only) so a void trial can be
    # re-run on the same set: MTX_EVAL_SEED="$(cat <batch>/.seed)" just bench ...
    if [ -z "${MTX_EVAL_SEED:-}" ]; then
      MTX_EVAL_SEED="$(python3 -c 'import secrets; print(secrets.token_hex(32))')"
    fi
    export MTX_EVAL_SEED
    # For `time-left` in tasks that opt in: their limit scales with the agent's.
    export MTX_AGENT_TIMEOUT_MULTIPLIER={{limit}}
    (umask 077 && printf '%s' "$MTX_EVAL_SEED" > "$batch/.seed")
    # The plan, written before the first trial starts: what runs (models,
    # harnesses and their versions, attempts, time), what's measured and on
    # which commit. `mtx bench review` shows trials not started yet as queued.
    python3 -I -c 'import json, sys; out, m, k, n, v, x, cc, cx, commit = sys.argv[1:]
    models = m.split()
    harness = lambda m: {"agent": "codex", "version": cx} if m.startswith("openai/") else {"agent": "claude-code", "version": cc}
    json.dump({"models": models, "attempts": int(k), "concurrent": int(n), "version": v, "timeout_multiplier": float(x),
               "harness": {m: harness(m) for m in models}, "web": False, "metric": "skill: total PnL over the hidden set / the reference bot'"'"'s, mean over attempts, 90% bootstrap interval",
               "commit": commit}, open(out, "w"), indent=2)' \
      "$batch/batch.json" "{{models}}" "{{k}}" "{{n}}" "{{version}}" "{{limit}}" "{{claude_code}}" "{{codex}}" "$(git rev-parse HEAD 2>/dev/null || echo unknown)"
    for model in {{models}}; do
      echo "== $model on $dataset: {{k}} trials, {{n}} at a time, {{limit}}x the agent's time"
      # Each harness is pinned, so numbers compare across releases, with no
      # web and no memory across trials.
      case "$model" in
        openai/*)
          harbor run -y -d "$dataset" -a codex -m "$model" -k {{k}} -n {{n}} -o "$batch" \
            --agent-timeout-multiplier {{limit}} \
            --ak version={{codex}} --ak web_search=disabled --ak disable_web_search=true
          ;;
        *)
          harbor run -y -d "$dataset" -a claude-code -m "anthropic/${model#anthropic/}" -k {{k}} -n {{n}} -o "$batch" \
            --agent-timeout-multiplier {{limit}} \
            --ak version={{claude_code}} \
            --ak disallowed_tools=WebSearch,WebFetch,RemoteTrigger,PushNotification,CronCreate,CronDelete,SendMessage,ListAgents \
            --ae ENABLE_TOOL_SEARCH=false --ae CLAUDE_CODE_DISABLE_AUTO_MEMORY=1
          ;;
      esac
    done
    if grep -rlF --exclude=.seed "$MTX_EVAL_SEED" "$batch"; then
      echo "the batch's secret appears in the job files above"; exit 1
    fi
    python3 -I bench/report.py "$batch"
    echo "browse it: just review $batch"
    echo "publish it: just publish $batch {{version}}"

# Browse a bench's results: a batch, a Harbor job, one trial, or a
# published release by its version (`just review 0.9.0`).
review path:
    cargo run --release -q -- bench review {{path}}

# Publish a finished batch to bench/results/<version>, to commit.
publish batch version:
    cargo run --release -q -- bench publish {{batch}} {{version}}

# Compare two releases task by task, as in `just compare 0.9.0...0.8.0`.
compare range:
    cargo run --release -q -- bench compare {{range}} --open

# Copy the reference bot into the task's solution: its sources, and the
# model it reads, if any (a `model/` directory, or `model.*` files beside
# its Cargo.toml).
solution:
    #!/usr/bin/env bash
    set -euo pipefail
    for pair in {{references}}; do
      bot="bots/${pair#*=}"; solution="{{tasks}}/${pair%%=*}/solution"
      mkdir -p "$solution/src"
      cp "$bot"/src/*.rs "$solution/src/"
      if [ -d "$bot/model" ]; then
        mkdir -p "$solution/model"
        cp "$bot"/model/*.json "$solution/model/"
      fi
      for model in "$bot"/model.*; do
        if [ -e "$model" ]; then cp "$model" "$solution/"; fi
      done
    done

# Fail if a task's solution drifted from its reference bot.
check-solution:
    #!/usr/bin/env bash
    set -euo pipefail
    for pair in {{references}}; do
      bot="bots/${pair#*=}"; solution="{{tasks}}/${pair%%=*}/solution"
      diff -ru "$bot/src" "$solution/src"
      if [ -d "$bot/model" ]; then diff -ru "$bot/model" "$solution/model"; fi
      for model in "$bot"/model.*; do
        if [ -e "$model" ]; then cmp "$model" "$solution/$(basename "$model")"; fi
      done
    done

# The checks every task change should pass: oracle ~1, and nop, cheater and
# sneaky 0. Cheater must be disqualified; sneaky must grade without errors.
harbor-checks task="flash-crash-spcx" surfpool="": (image surfpool) check-solution
    #!/usr/bin/env bash
    set -euo pipefail
    # The checks run on the images `image` just built: a task still pinned to
    # a released digest would test the old grader instead.
    grep -q '^FROM {{registry}}/{{image}}:{{tag}}$' {{tasks}}/{{task}}/environment/Dockerfile \
      && grep -q '^FROM {{registry}}/{{image}}:{{tag}}-runtime$' {{tasks}}/{{task}}/tests/Dockerfile \
      || { echo "{{task}} isn't on {{image}}:{{tag}}; unpin its Dockerfiles first"; exit 1; }
    # The verifier draws its hidden set from the batch's secret.
    export MTX_EVAL_SEED="${MTX_EVAL_SEED:-$(python3 -c 'import secrets; print(secrets.token_hex(32))')}"
    harbor run -y -p {{tasks}}/{{task}} -a oracle -o jobs
    harbor run -y -p {{tasks}}/{{task}} -a nop -o jobs
    PYTHONPATH=bench harbor run -y -p {{tasks}}/{{task}} -a checks.cheater_agent:CheaterAgent -o jobs
    PYTHONPATH=bench harbor run -y -p {{tasks}}/{{task}} -a checks.cheater_agent:SneakyAgent -o jobs

# Fail if the agent's image holds anything that answers its task: the
# anchors (reference bots), their source or the tasks' solutions, the
# checks' bots, or a scored scenario. Runs on the dev image `image` built.
leak-audit:
    #!/usr/bin/env bash
    set -euo pipefail
    # Lines only the answers hold: the reference bots' and the ladder's.
    needles=(
      "ride each move while it runs"
      "then fade it once it stops"
      "one tick inside the competitor"
      "backs off after one-sided fills"
      "fading the premiums the crowd's positioning opened"
      "mtx-ladder-rungs"
    )
    image="{{registry}}/{{image}}:{{tag}}"
    # Without the image every probe below fails, which reads as no leak.
    docker image inspect "$image" > /dev/null 2>&1 \
      || { echo "no image $image: run \`just image\` first" >&2; exit 2; }
    found=0
    for path in /opt/mtx/anchors /tests /solution /opt/mtx/sdk/bots/reference /opt/mtx/sdk/bots/maker /opt/mtx/sdk/bots/carry; do
      if docker run --rm --entrypoint test "$image" -e "$path"; then
        echo "LEAK: $image holds $path"; found=1
      fi
    done
    for needle in "${needles[@]}"; do
      hits="$(docker run --rm --entrypoint grep "$image" -rlsF --exclude-dir=proc --exclude-dir=sys \
        --exclude-dir=target --exclude-dir=.cargo --exclude-dir=registry -e "$needle" / || true)"
      if [ -n "$hits" ]; then
        echo "LEAK: \"$needle\" in:"; echo "$hits" | sed 's/^/  /'; found=1
      fi
    done
    [ "$found" = 0 ] && echo "no leaks in $image"
    exit "$found"

# Fail if a bot can read the grader's own keypairs (takers', competitor's):
# one market-making session in the runtime image `image` built, with a probe
# bot that looks for them.
keypair-probe:
    bench/checks/keypairs/probe.sh {{registry}}/{{image}}:{{tag}}-runtime

# The clock check for a task that opts in to `time-left` (bench/checks/clock):
# its agent reads half the limit at 0.5x and the clock runs down in step.
clock-probe task:
    bench/checks/clock/probe.sh {{tasks}}/{{task}}

# The skill ladder (bench/checks/ladder): fixed bots, from doing nothing to
# the regime oracle and the frozen exploit bots, graded on `sets` fresh
# hidden sets, each drawn from a secret of its own. Fails unless the order
# rule holds on every set and the exploit bots stay within their limits.
# Run it before benchmarking any scenario change. `parallel` rungs are
# graded at once, `jobs` sessions each: four in all at most, one bot user each.
ladder task="flash-crash" sets="4" jobs="4" parallel="1":
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release -q
    just bots > /dev/null
    (cd bench/checks/ladder/rungs && CARGO_TARGET_DIR="$PWD/../../../../target/ladder-rungs" \
      cargo build --release -q)
    mkdir -p bench/checks/ladder/bin
    for bin in blind momentum regime-oracle flow-oracle quoter; do
      cp "target/ladder-rungs/release/$bin" bench/checks/ladder/bin/
    done
    for bot in bench/checks/exploits/{{task}}-*/; do
      CARGO_TARGET_DIR="$PWD/target/exploits" "$bot/build.sh" > /dev/null
    done
    python3 -I bench/checks/ladder/ladder.py {{task}} --mtx target/release/mtx \
      --snapshot snapshots/{{snapshot}} --sets {{sets}} --jobs {{jobs}} --parallel {{parallel}}

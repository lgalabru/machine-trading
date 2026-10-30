# Loads MTH_FORK_RPC_URL for snapshot-prepare.
set dotenv-load := true

snapshot := "phoenix-spcx-2026-10-29"
registry := "ghcr.io/solana-foundation"
image := "machine-trading"
# Bump on every image change that tasks should pick up.
tag := "0.8.0"
tasks := "bench/tasks"
# Each task and the bot its oracle runs, as `<task>=<bot>`.
references := "phoenix-flash-crash=reference phoenix-market-making=maker"

default:
    @just --list

fmt:
    cargo +nightly fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

# Run mth from source, like `cargo run --release --`. Arguments keep their
# quoting, and a leading `mth` is optional: `just run mth scenario play "..."`.
[positional-arguments]
run *args:
    #!/usr/bin/env bash
    set -euo pipefail
    [ "${1:-}" = mth ] && shift
    exec cargo run --release -- "$@"

# Install a binary into ~/.cargo/bin: `just install` or `just install mth`.
install bin="mth":
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{bin}}" in
      mth) cargo install --locked --path crates/cli ;;
      *) echo "unknown binary {{bin}} (have: mth)"; exit 1 ;;
    esac

# Fork mainnet and write a pinned Phoenix snapshot.
snapshot-prepare out:
    cargo run --release -- snapshot prepare --out {{out}}

# Copy the snapshot's surfnet.json, too big for git, out of the published
# runtime image.
snapshot-pull:
    #!/usr/bin/env bash
    set -euo pipefail
    id="$(docker create {{registry}}/{{image}}:{{tag}}-runtime)"
    trap 'docker rm "$id" > /dev/null' EXIT
    docker cp "$id:/opt/mth/snapshots/{{snapshot}}/surfnet.json" snapshots/{{snapshot}}/surfnet.json
    echo "snapshots/{{snapshot}}/surfnet.json"

# Replay a snapshot offline and trade through a scripted crash.
snapshot-verify dir=("snapshots/" + snapshot):
    cargo run --release -- snapshot verify --dir {{dir}}

# Build the bots' bin/bot.
bots:
    bots/starter/build.sh
    bots/reference/build.sh
    bots/maker/build.sh
    bots/dip-buyer/build.sh

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
image:
    docker buildx build -f bench/image/Dockerfile --target runtime \
        -t {{registry}}/{{image}}:{{tag}}-runtime --load .
    docker buildx build -f bench/image/Dockerfile --target dev \
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
# time. Reads ANTHROPIC_API_KEY from .env or ~/.machine-trading.env. Results go
# to jobs/bench/; read them with `harbor view jobs/bench`. Arguments are
# positional: just bench "claude-sonnet-5-5 claude-opus-5-5" 3 3 0.3.0
bench models="claude-sonnet-5-5 claude-opus-5-5 claude-fable-5-1" k="3" n="3" version="":
    #!/usr/bin/env bash
    set -euo pipefail
    set -a
    [ -f .env ] && . ./.env
    [ -f ~/.machine-trading.env ] && . ~/.machine-trading.env
    set +a
    [ -n "${ANTHROPIC_API_KEY:-}" ] || { echo "ANTHROPIC_API_KEY is not set"; exit 1; }
    version="{{version}}"
    dataset="solana-foundation/machine-trading${version:+@$version}"
    for model in {{models}}; do
      echo "== $model on $dataset: {{k}} trials, {{n}} at a time"
      harbor run -d "$dataset" -a claude-code -m "anthropic/$model" -k {{k}} -n {{n}} -o jobs/bench
    done

# Copy the reference bot into the task's solution.
solution:
    #!/usr/bin/env bash
    set -euo pipefail
    for pair in {{references}}; do
      mkdir -p "{{tasks}}/${pair%%=*}/solution/src"
      cp "bots/${pair#*=}/src/main.rs" "{{tasks}}/${pair%%=*}/solution/src/main.rs"
    done

# Fail if a task's solution drifted from its reference bot.
check-solution:
    #!/usr/bin/env bash
    set -euo pipefail
    for pair in {{references}}; do
      diff -u "bots/${pair#*=}/src/main.rs" "{{tasks}}/${pair%%=*}/solution/src/main.rs"
    done

# The checks every task change should pass: oracle ~1, and nop, cheater and
# sneaky 0. Cheater must be disqualified; sneaky must grade without errors.
harbor-checks task="phoenix-flash-crash": image check-solution
    harbor run -p {{tasks}}/{{task}} -a oracle -o jobs
    harbor run -p {{tasks}}/{{task}} -a nop -o jobs
    PYTHONPATH=bench harbor run -p {{tasks}}/{{task}} -a checks.cheater_agent:CheaterAgent -o jobs
    PYTHONPATH=bench harbor run -p {{tasks}}/{{task}} -a checks.cheater_agent:SneakyAgent -o jobs

# Grade the exploit bots, written by agents in past benchmark runs, on a
# task's hidden set: each must lose in some sessions and stay near the
# reference. Run before benchmarking a scenario change. See
# bench/checks/exploits/README.md.
exploit-checks task="phoenix-flash-crash" jobs="3":
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release -q
    out=target/exploit-checks/{{task}}
    rm -rf "$out" && mkdir -p "$out"
    for bot in bench/checks/exploits/{{trim_start_match(task, "phoenix-")}}-*/; do
      name="$(basename "$bot")"
      CARGO_TARGET_DIR="$PWD/target/exploits" "$bot/build.sh" > /dev/null
      echo "== $name"
      target/release/mth grade --jobs {{jobs}} --snapshot snapshots/{{snapshot}} --bot "$bot" \
        --logs "$out/$name" --reward "$out/$name.json" {{tasks}}/{{task}}/tests/scenarios/*.json > /dev/null
    done
    case "{{task}}" in
      phoenix-market-making) limits="--max-profitable 0.9" ;;
      *) limits="--max-profitable 0.75 --max-ratio 3" ;;
    esac
    python3 -I bench/checks/exploits/judge.py $limits "$out"/*.json

# Loads OTT_FORK_RPC_URL for snapshot-prepare.
set dotenv-load := true

snapshot := "phoenix-sol-2026-10-27"
registry := "ghcr.io/solana-foundation"
image := "open-trading"
# Bump on every image change that tasks should pick up.
tag := "0.3.0"
tasks := "bench/tasks"
task := tasks + "/phoenix-flash-crash"

default:
    @just --list

fmt:
    cargo +nightly fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

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
    docker cp "$id:/opt/ott/snapshots/{{snapshot}}/surfnet.json" snapshots/{{snapshot}}/surfnet.json
    echo "snapshots/{{snapshot}}/surfnet.json"

# Replay a snapshot offline and trade through a scripted crash.
snapshot-verify dir=("snapshots/" + snapshot):
    cargo run --release -- snapshot verify --dir {{dir}}

# Build the bots' bin/bot.
bots:
    bots/starter/build.sh
    bots/reference/build.sh
    bots/dip-buyer/build.sh

# Draw scenarios from the SOL crash family into a directory.
scenarios out prefix count seed:
    cargo run --release -- scenario generate --out {{out}} --prefix {{prefix}} --count {{count}} --seed {{seed}}

# Record the reference bot's PnL in each scenario file.
calibrate +files: bots
    cargo run --release -- calibrate --snapshot snapshots/{{snapshot}} --bot bots/reference --logs target/logs/calibrate {{files}}

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
      local digest; digest="$(docker buildx imagetools inspect "$ref" | awk '/^Digest:/ { print $2; exit }')"
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
    just push-image
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
# time. Reads ANTHROPIC_API_KEY from .env or ~/.open-trading.env. Results go
# to jobs/bench/; read them with `harbor view jobs/bench`.
bench models="claude-sonnet-5-5 claude-opus-5-5 claude-fable-5-1" k="3" n="3" version="":
    #!/usr/bin/env bash
    set -euo pipefail
    set -a
    [ -f .env ] && . ./.env
    [ -f ~/.open-trading.env ] && . ~/.open-trading.env
    set +a
    [ -n "${ANTHROPIC_API_KEY:-}" ] || { echo "ANTHROPIC_API_KEY is not set"; exit 1; }
    version="{{version}}"
    dataset="solana-foundation/open-trading${version:+@$version}"
    for model in {{models}}; do
      echo "== $model on $dataset: {{k}} trials, {{n}} at a time"
      harbor run -d "$dataset" -a claude-code -m "anthropic/$model" -k {{k}} -n {{n}} -o jobs/bench
    done

# Copy the reference bot into the task's solution.
solution:
    mkdir -p {{task}}/solution/src
    cp bots/reference/src/main.rs {{task}}/solution/src/main.rs

# Fail if the task's solution drifted from bots/reference.
check-solution:
    diff -u bots/reference/src/main.rs {{task}}/solution/src/main.rs

# The checks every task change should pass: oracle ~1, and nop, cheater and
# sneaky 0. Cheater must be disqualified; sneaky must grade without errors.
harbor-checks: image check-solution
    harbor run -p {{task}} -a oracle -o jobs
    harbor run -p {{task}} -a nop -o jobs
    PYTHONPATH=bench harbor run -p {{task}} -a checks.cheater_agent:CheaterAgent -o jobs
    PYTHONPATH=bench harbor run -p {{task}} -a checks.cheater_agent:SneakyAgent -o jobs

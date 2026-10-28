# Loads OTT_FORK_RPC_URL for snapshot-prepare.
set dotenv-load := true

snapshot := "phoenix-sol-2026-10-27"
registry := "ghcr.io/solana-foundation"
image := "open-trading"
# Bump on every image change that tasks should pick up.
tag := "0.1.0"
surfpool := "../surfpool-ofm"
task := "tasks/phoenix-flash-crash"

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

# Replay a snapshot offline and trade through a scripted crash.
snapshot-verify dir=("snapshots/" + snapshot):
    cargo run --release -- snapshot verify --dir {{dir}}

# Build the bots' bin/bot.
bots:
    bots/starter/build.sh
    bots/reference/build.sh

# Draw scenarios from the SOL crash family into a directory.
scenarios out prefix count seed:
    cargo run --release -- scenario generate --out {{out}} --prefix {{prefix}} --count {{count}} --seed {{seed}}

# Record the reference bot's PnL in each scenario file.
calibrate +files: bots
    cargo run --release -- calibrate --snapshot snapshots/{{snapshot}} --bot bots/reference --logs target/logs/calibrate {{files}}

# Grade a bot directory on scenario files, as the verifier does.
grade bot +files:
    cargo run --release -- grade --snapshot snapshots/{{snapshot}} --bot {{bot}} --logs target/logs/grade {{files}}

# The shared task image, for this machine only. Tag it as the registry name so
# task Dockerfiles that aren't pinned yet build against it.
image:
    docker buildx build --build-context surfpool={{surfpool}} -f docker/Dockerfile \
        -t {{registry}}/{{image}}:{{tag}} --load .

# Build the image for amd64 and arm64 and push it. Pin the tasks to the
# pushed digest afterwards (`just pin-image`).
# amd64 builds under emulation on Apple silicon: expect the first run to be slow.
push-image jobs="3":
    docker buildx build --build-context surfpool={{surfpool}} -f docker/Dockerfile \
        --build-arg CARGO_BUILD_JOBS={{jobs}} --platform linux/amd64,linux/arm64 \
        -t {{registry}}/{{image}}:{{tag}} --push .

# Point every task's Dockerfiles at the pushed image, by digest.
pin-image:
    #!/usr/bin/env bash
    set -euo pipefail
    ref="{{registry}}/{{image}}:{{tag}}"
    digest="$(docker buildx imagetools inspect "$ref" | awk '/^Digest:/ { print $2; exit }')"
    echo "$ref@$digest"
    sed -i '' -E "s|^FROM {{registry}}/{{image}}[-a-z]*:.*|FROM $ref@$digest|" tasks/*/environment/Dockerfile tasks/*/tests/Dockerfile

# Copy the reference bot into the task's solution.
solution:
    mkdir -p {{task}}/solution/src
    cp bots/reference/src/main.rs {{task}}/solution/src/main.rs

# Fail if the task's solution drifted from bots/reference.
check-solution:
    diff -u bots/reference/src/main.rs {{task}}/solution/src/main.rs

# The checks every task change should pass: oracle ~1, noop 0, cheater 0.
harbor-checks: image check-solution
    harbor run -p {{task}} -a oracle -o jobs
    harbor run -p {{task}} -a nop -o jobs
    PYTHONPATH=. harbor run -p {{task}} -a checks.cheater_agent:CheaterAgent -o jobs

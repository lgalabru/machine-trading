# Loads MTX_FORK_RPC_URL for snapshot-build.
set dotenv-load := true

# `references`, `reference_packages`, `task_bots` and `leak_needles`, written
# from the task folders by `mtx task sync`.
import 'bench/tasks/tasks.just'

snapshot := "phoenix-spcx-2026-10-29"
spot_snapshot := "sol-usdc-spot-2026-11-11"
registry := "ghcr.io/solana-foundation"
image := "machine-trading"
# Bump on every image change that tasks should pick up.
tag := "0.13.0"
tasks := "bench/tasks"
# The harness versions `just bench` runs, pinned so results compare. Every
# model runs in Hermes (NousResearch/hermes-agent): one harness for every
# provider, so no model is helped or hurt by its harness. `native:<model>`
# runs a model in its provider's own coding agent instead (Claude Code,
# Codex), as releases up to 0.11.0 did.
hermes := "v0.23.1"
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

# Build a snapshot from its recipe: fork mainnet, warm up, export.
# Refuses to replace the committed snapshot: pass `--out DIR`, or `--force`.
# The ladder script's own tests.
ladder-test:
    python3 -I bench/checks/ladder/test_ladder.py

# The probe bots' crate (outside the workspace, so `fmt`, `lint` and `test`
# never see it) and its calibration: format, lint, tests. CI runs it.
probes-check:
    cargo +nightly fmt --manifest-path bench/checks/probes/Cargo.toml -- --check
    cargo clippy --manifest-path bench/checks/probes/Cargo.toml --all-targets -- -D warnings
    cargo test --manifest-path bench/checks/probes/Cargo.toml
    python3 -I bench/checks/probes/test_calibrate.py
    python3 -I bench/checks/ladder/test_ladder.py

snapshot-build name *flags:
    cargo run --release -- snapshot build snapshots/{{name}}/recipe.toml {{flags}}

# Replay a snapshot offline and trade through a scripted crash.
snapshot-verify dir=("snapshots/" + snapshot):
    cargo run --release -- snapshot verify --dir {{dir}}

# Build the bots: the process bots' bin/bot, the components' bot.wasm. The
# starters, and each task's reference and ladder bots.
bots:
    #!/usr/bin/env bash
    set -euo pipefail
    for bot in bots/starter bots/printer bots/first-strategy {{task_bots}}; do
      just --justfile "$bot/Justfile" build
    done

# The geyser plugin against a grade (docs/rfd/0004-strategies-on-mainnet.md):
# loaded into a flash-crash and a market-making session's surfnet, it reads
# the market from the accounts it is fed as the grader reads it, and trades
# both references as the grader does; on a surfnet in real time, it runs a
# strategy on its own and takes `mtx live`'s commands.
geyser-parity:
    #!/usr/bin/env bash
    set -euo pipefail
    just --justfile bench/tasks/flash-crash-spcx/reference/Justfile build
    just --justfile bench/tasks/market-making-spcx/reference/Justfile build
    cargo build -p mtx-geyser
    case "$(uname -s)" in Darwin) lib=libmtx_geyser.dylib ;; *) lib=libmtx_geyser.so ;; esac
    MTX_TEST_GEYSER_PLUGIN="$PWD/${CARGO_TARGET_DIR:-target}/debug/$lib" \
      cargo test -p mtx-grader --test geyser_parity --test live_control -- --ignored

# A spot session end to end: the smoke component sells on SOL/USDC and passes
# the mandate's audit; the buyer breaches it; the order smokes; OpenBook
# alone; and the same smoke as a process bot, which spot sessions still run.
# Each grade must come out as expected: filled and clean, or disqualified.
spot-smoke:
    #!/usr/bin/env bash
    set -euo pipefail
    just --justfile bots/spot-smoke/Justfile build
    cargo build --release
    grade() {
      local name=$1 bot=$2 check=$3 want=$4; shift 4
      "${CARGO_TARGET_DIR:-target}/release/mtx" grade --snapshot snapshots/{{spot_snapshot}} --bot "bots/$bot" "$@" \
        --logs "target/logs/$name" --report "target/logs/$name.json" "bench/checks/spot/$check.json" > /dev/null
      local report="target/logs/$name.json"
      local banned fills
      banned=$(jq '[.. | objects | select(has("disqualified")) | .disqualified | select(. != null)] | length' "$report")
      fills=$(jq '[.. | objects | select(has("fills")) | .fills | arrays | length] | add // 0' "$report")
      # A resting order seen at some step; the takers' orders all sent.
      local open flow
      open=$(jq '[.. | objects | select(has("open_orders")) | .open_orders | arrays | length] | add // 0' "$report")
      flow=$(jq -c '[.. | objects | select(has("makers_widened_ticks"))] | [(map(.orders) | add // 0), (map(.sent) | add // 0), (map(.failed) | add // 0)]' "$report")
      case "$want" in
        clean) [ "$banned" -eq 0 ] && [ "$fills" -gt 0 ] ;;
        disqualified) [ "$banned" -gt 0 ] ;;
        rested) [ "$banned" -eq 0 ] && [ "$open" -gt 0 ] ;;
        takers) [ "$banned" -eq 0 ] && jq -e '.[0] > 0 and .[1] == .[0] and .[2] == 0' <<< "$flow" > /dev/null ;;
      esac || { echo "$name: expected $want, got $fills fills, $open open orders, takers $flow and $banned disqualifications"; exit 1; }
      echo "$name: $want ($fills fills)"
    }
    grade spot-smoke spot-smoke smoke clean --components-only
    grade spot-smoke-buyer spot-smoke/buyer smoke disqualified --components-only
    grade spot-smoke-sell-order spot-smoke smoke-sell-order clean --components-only
    grade spot-smoke-buy-order spot-smoke/accumulate smoke-buy-order clean --components-only
    grade spot-smoke-reduce-only spot-smoke/buyer smoke-reduce-only disqualified --components-only
    grade spot-smoke-net spot-smoke smoke-net clean --components-only
    grade spot-smoke-openbook spot-smoke smoke-openbook clean --components-only
    grade spot-smoke-rest spot-smoke/rest smoke-rest rested --components-only
    grade spot-smoke-tape spot-smoke smoke-tape takers --components-only
    grade spot-smoke-process spot-smoke/process smoke clean

# Kamino lending sessions (bench/checks/kamino), graded with the probe: the
# bot and a competing liquidator both liquidate borrowers a crash pushes
# under; a stale oracle blocks borrows but not repays; a day's clock jump
# accrues a day's interest; a process bot that sends all its SOL away is
# still measured.
kamino-smoke:
    #!/usr/bin/env bash
    set -euo pipefail
    just --justfile bots/kamino-probe/Justfile build
    just --justfile bots/kamino-drain/Justfile build
    cargo build --release
    grade() {
      "${CARGO_TARGET_DIR:-target}/release/mtx" grade --components-only --snapshot snapshots/kamino-2026-11-13 \
        --bot bots/kamino-probe --logs "target/logs/kamino-$1" --report "target/logs/kamino-$1.json" \
        "bench/checks/kamino/$1.json" > /dev/null
    }
    grade liquidation
    jq -e '.scenarios[0] | .lending.liquidations >= 1
      and ([.lending.actions[] | select(.action == "liquidate" and .error == null)] | length) >= 1
      and (has("crank_refusals") | not)' target/logs/kamino-liquidation.json > /dev/null
    grade stale-oracle
    log=$(cat target/logs/kamino-stale-oracle/*)
    grep -q 'Borrow { amount: Some(1.0) }.*Landed' <<< "$log"
    grep -q 'Borrow { amount: Some(1.0) }.*Failed.*price status' <<< "$log"
    grade interest
    jq -e '.scenarios[0].lending.end[1].borrowed > 600.03' target/logs/kamino-interest.json > /dev/null
    "${CARGO_TARGET_DIR:-target}/release/mtx" grade --snapshot snapshots/kamino-2026-11-13 \
      --bot bots/kamino-drain --logs target/logs/kamino-drain --report target/logs/kamino-drain.json \
      bench/checks/kamino/drain.json > /dev/null
    grep -rq 'drain: sent all SOL away' target/logs/kamino-drain
    jq -e '.scenarios[0].lending.end[1].borrowed > 299' target/logs/kamino-drain.json > /dev/null
    echo "kamino-smoke: ok"

# The actors on every equity perp of the 96-market snapshot (NVDA, TSLA,
# SPY, QQQ, with SPCX as the control): one TABLE line per check and market.
actors-markets:
    cargo test -p mtx-grader --test actors_non_spcx -- --ignored --nocapture

# One actor on two venues: the grader's takers send a funding-carry practice
# session's tape on the SPCX perp, and the spot smoke's tape on Phoenix v1's
# SOL-USDC book, every order from its account's own seat. Then every actor
# of the library on the SPCX perp (docs/actors.md).
actors-smoke:
    #!/usr/bin/env bash
    set -euo pipefail
    just --justfile bots/starter/Justfile build
    just --justfile bots/spot-smoke/Justfile build
    cargo build --release
    takers() {
      local name=$1 snapshot=$2 bot=$3 scenario=$4 report="target/logs/$1.json"
      "${CARGO_TARGET_DIR:-target}/release/mtx" grade --components-only --snapshot "snapshots/$snapshot" --bot "$bot" \
        --logs "target/logs/$name" --report "$report" "$scenario" > /dev/null
      local flow
      flow=$(jq -c '[.. | objects | select(has("makers_widened_ticks"))][0] | {orders, sent, failed, volume}' "$report")
      jq -e '.orders > 0 and .sent == .orders and .failed == 0' <<< "$flow" > /dev/null \
        || { echo "$name: the takers didn't send every order: $flow"; exit 1; }
      echo "$name: $flow"
    }
    takers actors-perp {{snapshot}} bots/starter bench/tasks/funding-carry-spcx/environment/practice/practice-1.json
    takers actors-spot {{spot_snapshot}} bots/spot-smoke bench/checks/spot/smoke-tape.json
    # The library: every actor beside a bot that sells and answers requests
    # for quotes, in six sessions of fresh wallets. In each, every actor
    # acts, none acting on its own fails, and RFQ fills settle; over the
    # six, reading the chain pays: told makes at least twice what a careful
    # blind strategy does.
    just --justfile bots/actors-smoke/Justfile build
    just --justfile bots/rfq-desk/Justfile build
    just --justfile bots/rfq-dust/Justfile build
    library() {
      local name=$1 bot=$2 scenario=${3:-everyone}
      "${CARGO_TARGET_DIR:-target}/release/mtx" grade --components-only --snapshot "snapshots/{{snapshot}}" --bot "$bot" \
        --logs "target/logs/$name" --report "target/logs/$name.json" --details "target/logs/$name-details" \
        "bench/checks/actors/$scenario.json" > /dev/null
    }
    sessions=()
    for i in 1 2 3 4 5 6; do
      name=actors-library-$i report=target/logs/actors-library-$i.json
      library "$name" bots/actors-smoke
      jq -e '.scenarios[0].actors | ([.actors[] | select(.reacts_to_bot | not) | .actions.failed] | add) == 0
        and all(.actors[]; .actions.took + .actions.placed > 0 or .counts.no_lending_venue == 1)
        and all(.actors[]; .on_chain == .actions.took + .actions.placed + .actions.cancelled)
        and any(.actors[]; .kind == "rfq-client" and .counts.fills > 0)' "$report" > /dev/null \
        || { echo "$name: an actor didn't act, or failed: $(jq -c '.scenarios[0].actors.actions' "$report")"; exit 1; }
      sessions+=("$report" "target/logs/$name-details/bot/actors-everyone.json.gz")
    done
    python3 bench/checks/actors/read.py --require-edge "${sessions[@]}" > target/logs/actors-library-read.json
    echo "actors-library: $(jq -c .edge target/logs/actors-library-read.json)"
    # The RFQ desk's accounting: an idle bot's PnL is its desk asset's move
    # at the index, and a bot that only answers requests changes by what
    # its fills earn against the index, no more.
    library actors-desk-idle bots/starter
    python3 bench/checks/actors/desk.py --idle target/logs/actors-desk-idle.json > /dev/null
    library actors-desk bots/rfq-desk
    python3 bench/checks/actors/desk.py target/logs/actors-desk.json > /dev/null
    echo "actors-desk: idle $(jq -c '.scenarios[0].pnl_usd' target/logs/actors-desk-idle.json) USD, answering $(jq -c '.scenarios[0].pnl_usd' target/logs/actors-desk.json) USD, both the desk's own"
    # The pool, the iceberg and the desk scored over the idle desk: noise
    # and a two-wallet stop hunter draw from four wallets, so some wallet is
    # in both roles; the iceberg shows slices and never takes; an idle desk
    # scores 0, and an answering desk what its fills earn.
    library actors-pooled bots/actors-smoke pooled
    jq -e '.scenarios[0].actors | ([.actors[] | select(.reacts_to_bot | not) | .actions.failed] | add) == 0
        and all(.actors[]; .on_chain == .actions.took + .actions.placed + .actions.cancelled)
        and ([.actors[] | select(.kind == "noise" or .kind == "stop-hunter") | .wallets[]] | (length > (unique | length)))
        and any(.actors[]; .kind == "iceberg" and .counts.slices > 0 and .actions.took == 0)
        and any(.actors[]; .kind == "stop-hunter" and .counts.pushes > 0)' target/logs/actors-pooled.json > /dev/null \
      || { echo "actors-pooled: the pool or the iceberg didn't act as set: $(jq -c '[.scenarios[0].actors.actors[] | {kind, wallets: (.wallets | length), counts}]' target/logs/actors-pooled.json)"; exit 1; }
    library actors-pooled-idle bots/starter pooled
    python3 bench/checks/actors/desk.py --idle --over-idle target/logs/actors-pooled-idle.json > /dev/null
    jq -e '(.scenarios[0].pnl_usd | fabs) < 0.01' target/logs/actors-pooled-idle.json > /dev/null \
      || { echo "actors-pooled-idle: an idle desk didn't score 0: $(jq -c '.scenarios[0].pnl_usd' target/logs/actors-pooled-idle.json)"; exit 1; }
    library actors-pooled-desk bots/rfq-desk pooled
    python3 bench/checks/actors/desk.py --over-idle target/logs/actors-pooled-desk.json > /dev/null
    echo "actors-pooled: idle $(jq -c '.scenarios[0].pnl_usd' target/logs/actors-pooled-idle.json) USD, answering $(jq -c '.scenarios[0].pnl_usd' target/logs/actors-pooled-desk.json) USD over the idle desk"
    # Answers too small to settle are no answers: a bot that sends nothing
    # else grades (its session isn't voided), and no client acts on them.
    library actors-rfq-dust bots/rfq-dust rfq-dust
    jq -e '.scenarios[0].actors.actors[] | select(.kind == "rfq-client")
        | .counts.requests > 0 and (.counts.answered // 0) == 0 and (.counts.fills // 0) == 0
          and .actions == {placed: 0, cancelled: 0, took: 0, failed: 0}' target/logs/actors-rfq-dust.json > /dev/null \
      || { echo "actors-rfq-dust: a client acted on dust: $(jq -c '.scenarios[0].actors.actors' target/logs/actors-rfq-dust.json)"; exit 1; }
    echo "actors-rfq-dust: $(jq -c '[.scenarios[0].actors.actors[] | select(.kind == "rfq-client") | .counts] | .[0]' target/logs/actors-rfq-dust.json), graded"

# Write the lists kept from the task folders (bench/tasks/*/task.toml), or
# fail if they are stale: the Justfile's, the image's, the dataset's, the READMEs'.
tasks-sync:
    cargo run --release -- task sync

tasks-check:
    cargo run --release -q -- task check

# The strategy interface end to end: the probe puts one line of each kind
# on its first sheet in a flash-crash session, and its log
# (target/logs/wit-probe) shows what it was handed and what became of each.
wit-probe:
    just --justfile bots/wit-probe/Justfile build
    cargo run --release -- grade --components-only --snapshot snapshots/{{snapshot}} --bot bots/wit-probe --logs target/logs/wit-probe bench/tasks/flash-crash-spcx/environment/practice/practice-1.json

# Raw sends end to end: the probe tries one of each kind, allowed and not,
# in a session that enables them, and its log (target/logs/raw-probe) shows
# what the grader answered and what became of the ones it took.
raw-probe:
    just --justfile bots/raw-probe/Justfile build
    cargo run --release -- grade --components-only --snapshot snapshots/{{snapshot}} --bot bots/raw-probe --logs target/logs/raw-probe bots/raw-probe/scenario.json

# The transaction layer end to end: the probe sends with each kind of
# transaction option through a fee spike, and its log (target/logs/tx-probe)
# shows what became of each; the report's `tx_layer` has every block.
tx-probe:
    just --justfile bots/tx-probe/Justfile build
    cargo run --release -- grade --components-only --snapshot snapshots/{{snapshot}} --bot bots/tx-probe --logs target/logs/tx-probe bots/tx-probe/scenario.json

# Draw scenarios from a task's family: just scenarios DIR hidden 12 SEED market-making
scenarios out prefix count seed family="flash-crash":
    cargo run --release -- scenario generate --family {{family}} --out {{out}} --prefix {{prefix}} --count {{count}} --seed {{seed}}

# Record a reference bot's PnL in each scenario file: just calibrate bench/tasks/market-making-spcx/reference FILES...
calibrate bot +files: bots
    cargo run --release -- calibrate --snapshot snapshots/{{snapshot}} --bot {{bot}} --logs target/logs/calibrate {{files}}

# Grade a bot directory on scenario files, as the verifier does.
grade bot +files:
    cargo run --release -- grade --snapshot snapshots/{{snapshot}} --bot {{bot}} --logs target/logs/grade {{files}}

# The task images, for this machine only: dev and agent (the agents'; agent is
# dev with a guarded `mtx`) and runtime (verifiers). Tagged as the registry names, so task Dockerfiles that aren't
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
    docker buildx build -f bench/image/Dockerfile --target agent ${context[@]+"${context[@]}"} \
        -t {{registry}}/{{image}}:{{tag}}-agent --load .
    just task-images

# Each task's own images, one at a time, from its environment/ and tests/:
# `<tag>-<task>` for the agent and `<tag>-<task>-verifier`, which task.toml
# names. Harbor then builds nothing when a trial starts. `push` pushes them,
# for both platforms, instead of loading them here.
#
# Each context is a fresh copy, under a name of its own. BuildKit keeps a
# context's files between builds by the directory's name, and a file with
# the same size and mtime counts as unchanged: every task's context is
# `tests` (or `environment`), and a checkout gives their files one mtime,
# so two tasks whose files have the same sizes got each other's (2026-11-12:
# flash-crash's verifier held weekend-gap's tests). A loaded image is then
# checked against its own files.
task-images push="":
    #!/usr/bin/env bash
    set -euo pipefail
    stage="$(mktemp -d)"
    trap 'rm -rf "$stage"' EXIT
    # The files of a context, as `sha256sum` lists them, sorted.
    files() { (cd "$1" && find . -type f ! -name Dockerfile | sort | xargs sha256sum); }
    for dir in {{tasks}}/*/; do
      task="$(basename "$dir")"
      for part in environment:"":/opt/mtx/practice:practice tests:-verifier:/tests:.; do
        IFS=: read -r src suffix inside sub <<< "$part"
        ref="{{registry}}/{{image}}:{{tag}}-$task$suffix"
        context="$stage/$task-$src"
        cp -R "$dir$src" "$context"
        if [ -n "{{push}}" ]; then
          docker buildx build --platform linux/amd64,linux/arm64 -t "$ref" --push "$context"
        else
          docker buildx build -t "$ref" --load "$context"
          want="$(files "$context/$sub")"
          got="$(docker run --rm --entrypoint sh "$ref" -c "cd $inside && find . -type f ! -name Dockerfile | sort | xargs sha256sum")"
          [ "$want" = "$got" ] || { echo "$ref doesn't hold $task's $src: Docker built it from another context"; exit 1; }
          echo "$ref holds $task's $src"
        fi
      done
    done

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
    docker buildx build -f bench/image/Dockerfile --target agent \
        --build-arg CARGO_BUILD_JOBS={{jobs}} --platform linux/amd64,linux/arm64 \
        -t {{registry}}/{{image}}:{{tag}}-agent --push .

# Point every task at the pushed images, by digest: environments at dev (at
# agent, for a guarded task: one whose environment starts from `-agent`),
# verifiers at runtime.
pin-image:
    #!/usr/bin/env bash
    set -euo pipefail
    # In place, with GNU sed (Linux, CI) or BSD sed (macOS).
    sedi() { if sed --version > /dev/null 2>&1; then sed -i "$@"; else sed -i '' "$@"; fi; }
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
      sedi -E "s|^FROM {{registry}}/{{image}}[-a-z]*:.*|FROM $ref@$digest|" "$@"
    }
    dev_files=() agent_files=()
    for f in {{tasks}}/*/environment/Dockerfile; do
      if grep -q -E '^FROM {{registry}}/{{image}}:[^@ ]*-agent(@| |$)' "$f"; then agent_files+=("$f"); else dev_files+=("$f"); fi
    done
    [ "${#dev_files[@]}" = 0 ] || pin {{registry}}/{{image}}:{{tag}} "${dev_files[@]}"
    [ "${#agent_files[@]}" = 0 ] || pin {{registry}}/{{image}}:{{tag}}-agent "${agent_files[@]}"
    pin {{registry}}/{{image}}:{{tag}}-runtime {{tasks}}/*/tests/Dockerfile
    # The tasks' own images, built on the pins just made, then pinned in
    # each task.toml.
    just task-images push
    for dir in {{tasks}}/*/; do
      task="$(basename "$dir")"
      for suffix in "" -verifier; do
        ref="{{registry}}/{{image}}:{{tag}}-$task$suffix"
        digest=""
        for attempt in 1 2 3 4 5; do
          digest="$(docker buildx imagetools inspect "$ref" 2>/dev/null | awk '/^Digest:/ { print $2; exit }')" && [ -n "$digest" ] && break
          sleep $((attempt * 5))
        done
        [ -n "$digest" ] || { echo "no digest for $ref"; exit 1; }
        echo "$ref@$digest"
        sedi -E "s|^docker_image = \"{{registry}}/{{image}}:[^\"]*-$task$suffix(@sha256:[0-9a-f]+)?\"|docker_image = \"$ref@$digest\"|" "$dir/task.toml"
      done
    done

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
    just release-publish

# The second half of a release, once both images are pushed under `tag`:
# pin every task to them, check, and publish the dataset to Harbor. The
# release workflow (.github/workflows/release.yml) builds the images itself,
# then runs this.
release-publish:
    #!/usr/bin/env bash
    set -euo pipefail
    just pin-image
    for task in {{tasks}}/*/; do harbor lint "$task" --select ALL; done
    harbor sync {{tasks}}
    # Every task in the dataset has a real digest: 0.12.0 went out with two
    # of its four tasks still at sha256:000…, and so without them.
    if grep -q 'digest = "sha256:0\{64\}"' {{tasks}}/dataset.toml; then
      echo "dataset.toml lists a task with no digest; harbor sync didn't reach it:"
      grep -B1 'digest = "sha256:0\{64\}"' {{tasks}}/dataset.toml
      exit 1
    fi
    # The dataset keeps the layout `mtx task sync` writes.
    cargo run --release -q -- task check
    # Publishing a dataset asks for confirmation, even with -y.
    printf 'y\n' | harbor publish {{tasks}} --public
    # Harbor addresses versions by revision; tag each with its declared version.
    version_of() { awk -F'"' '/^version *=/ { print $2; exit }' "$1"; }
    name_of() { awk -F'"' '/^name *=/ { print $2; exit }' "$1"; }
    for task in {{tasks}}/*/; do
      harbor version tag "$(name_of "$task/task.toml")@latest" "$(version_of "$task/task.toml")" --force
    done
    harbor version tag "$(name_of {{tasks}}/dataset.toml)@latest" "$(version_of {{tasks}}/dataset.toml)" --force
    # What a bench will pull: every task of bench/tasks, at this version.
    check="$(mktemp -d)"
    harbor dataset download "$(name_of {{tasks}}/dataset.toml)@$(version_of {{tasks}}/dataset.toml)" -o "$check" > /dev/null
    want="$(ls -d {{tasks}}/*/ | wc -l | tr -d ' ')"
    got="$(find "$check" -mindepth 2 -maxdepth 3 -name task.toml | wc -l | tr -d ' ')"
    rm -r "$check"
    [ "$got" = "$want" ] || { echo "the published dataset holds $got tasks, bench/tasks $want"; exit 1; }
    echo "released {{registry}}/{{image}}:{{tag}} and :{{tag}}-runtime; commit the pins and digests"

# Benchmark agents on the published dataset. Models are names from
# bench/harness/models.toml, or `provider/model` (`anthropic/` when there's
# no provider), and run in Hermes; `native:<model>` runs one in its
# provider's own harness instead (Claude Code, Codex). Reads each model's
# key (ANTHROPIC_API_KEY, OPENAI_API_KEY, or the one models.toml names) from
# .env or ~/.machine-trading.env, and checks each Hermes model with a
# 1-token request before the first trial (`just preflight`). Hermes's
# trials reach their providers through a key proxy on this machine and
# never hold a key; a key found in the batch's files quarantines it.
# `effort`, when given, applies to every harness; left empty, each keeps its
# own default. Results go to jobs/bench/; read them with `mtx bench review`.
# Arguments are positional (models, attempts, at a time, version, limit,
# together, effort):
# just bench "claude-opus-5-5 openai/<model> kimi-k3" 5 3 0.12.0 1 0 high
# `together=1` runs the models at once (each with n trials at a time)
# instead of one after the other: grades don't depend on load.
#
# A provider that refuses an agent for quota or credit stops the whole
# batch at once: its other agents would be cut off too, and a cut-off trial
# can't be ranked. The batch is marked (.stopped) and gets no report.
bench models="claude-sonnet-5-5 claude-opus-5-5 claude-fable-5-1 claude-fable-5-2" k="5" n="3" version="" limit="1" together="0" effort="":
    #!/usr/bin/env bash
    set -euo pipefail
    set -a
    [ -f .env ] && . ./.env
    [ -f ~/.machine-trading.env ] && . ~/.machine-trading.env
    set +a
    # A guarded task's agent can reach only its model's address and the crates
    # registry; a native harness would need its provider's API open, with its
    # real key in the agent's environment and the provider's server-side fetch
    # and search tools behind it. Guarded tasks run through Hermes only
    # (MTX_UNGUARDED=1 lets a screen that is never published run natives anyway).
    python3 -I bench/harness/models.py native-guard {{tasks}} {{models}}
    unguarded=""
    if [ -n "${MTX_UNGUARDED:-}" ] && python3 -I -c 'import sys; sys.exit(0 if "native:" in sys.argv[1] else 1)' "{{models}}"; then
      unguarded=1
      echo "WARNING: MTX_UNGUARDED is set: native harnesses run with their provider's API open and a real key" >&2
      echo "WARNING: in the agent's environment. Guarded tasks are NOT guarded in this batch; it can't be published." >&2
    fi
    for model in {{models}}; do
      case "$model" in
        native:openai/*) [ -n "${OPENAI_API_KEY:-}" ] || { echo "OPENAI_API_KEY is not set, for $model"; exit 1; } ;;
        native:*/*) [ "${model#native:anthropic/}" != "$model" ] || { echo "$model: only anthropic/ and openai/ models have a native harness"; exit 1; }
          [ -n "${ANTHROPIC_API_KEY:-}" ] || { echo "ANTHROPIC_API_KEY is not set, for $model"; exit 1; } ;;
        native:*) [ -n "${ANTHROPIC_API_KEY:-}" ] || { echo "ANTHROPIC_API_KEY is not set, for $model"; exit 1; } ;;
        *) key="$(python3 -I bench/harness/models.py key "$model")"
          [ -n "${!key:-}" ] || { echo "$key is not set, for $model"; exit 1; } ;;
      esac
    done
    # Hermes's models: a wrong name or a dead key fails here, not in every
    # trial.
    hermes_models=()
    for model in {{models}}; do
      case "$model" in native:*) ;; *) hermes_models+=("${model#hermes:}") ;; esac
    done
    # What the x402 routes pay, the preflight's calls included, counts against
    # one MTX_X402_BUDGET_USD: the preflight and the key proxy share a ledger,
    # kept in the batch as x402-ledger.json.
    MTX_X402_LEDGER="$(mktemp)"
    rm -f "$MTX_X402_LEDGER"
    export MTX_X402_LEDGER
    if [ "${#hermes_models[@]}" -gt 0 ]; then
      python3 -I bench/harness/models.py preflight "${hermes_models[@]}"
    fi
    # No harness run gets a flag the batch didn't ask for: each keeps its own
    # default effort unless `effort` is given.
    effort=()
    [ -z "{{effort}}" ] || effort=(--effort "{{effort}}")
    version="{{version}}"
    dataset="solana-foundation/machine-trading${version:+@$version}"
    batch="jobs/bench/$(date +%Y%m%d-%H%M%S)-x{{limit}}"
    mkdir -p "$batch"
    [ ! -e "$MTX_X402_LEDGER" ] || mv "$MTX_X402_LEDGER" "$batch/x402-ledger.json"
    MTX_X402_LEDGER="$PWD/$batch/x402-ledger.json"
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
    python3 -I -c 'import json, sys; out, m, k, n, v, x, cc, cx, hm, effort, commit = sys.argv[1:]
    models = m.split()
    harness = lambda m: ({"agent": "codex", "version": cx} if m.startswith("native:openai/") else {"agent": "claude-code", "version": cc}) if m.startswith("native:") else {"agent": "hermes", "version": hm}
    json.dump({"models": models, "attempts": int(k), "concurrent": int(n), "version": v, "timeout_multiplier": float(x), "effort": effort or None,
               "harness": {m: harness(m) for m in models}, "web": False, "metric": "skill: total PnL over the hidden set / the reference bot'"'"'s, mean over attempts, 90% bootstrap interval",
               "commit": commit}, open(out, "w"), indent=2)' \
      "$batch/batch.json" "{{models}}" "{{k}}" "{{n}}" "{{version}}" "{{limit}}" "{{claude_code}}" "{{codex}}" "{{hermes}}" "{{effort}}" "$(git rev-parse HEAD 2>/dev/null || echo unknown)"
    # A screen with native harnesses on guarded tasks is stamped before its
    # first trial: an interrupted batch is still unguarded.
    if [ -n "$unguarded" ]; then
      python3 -I bench/harness/models.py stamp-unguarded "$batch"
    fi
    # What the providers say when the account, not the model, is out: a
    # quota, a credit balance or a usage cap (an x402 route's: the wallet's
    # USDC, or the batch's MTX_X402_BUDGET_USD, as the key proxy words them). Rate limits are retried by the
    # harnesses and don't stop a batch.
    refused='Quota exceeded|insufficient_quota|exceeded your current quota|credit balance is too low|billing_hard_limit_reached|usage limit reached|credits exhausted|no credits remaining|x402 wallet is out of USDC|x402 budget spent'
    # A guarded task limits its agent's network to an allowlist: the bench adds
    # the key proxy's address to it (native harnesses don't run guarded tasks:
    # see native-guard above). A task on the public network ignores the flag.
    proxy_host() { local h="${proxy_url#*://}"; echo "${h%%:*}"; }
    run_model() {
      local model="$1"
      echo "== $model on $dataset: {{k}} trials, {{n}} at a time, {{limit}}x the agent's time"
      # Each harness is pinned, so numbers compare across releases, with no
      # web and no memory across trials.
      case "$model" in
        native:openai/*)
          model="${model#native:}"
          harbor run -y -d "$dataset" -a codex -m "$model" -k {{k}} -n {{n}} -o "$batch" \
            --agent-timeout-multiplier {{limit}} "${effort[@]}" \
            --allow-agent-host api.openai.com --allow-agent-host chatgpt.com \
            --ak version={{codex}} --ak web_search=disabled --ak disable_web_search=true
          ;;
        native:*)
          model="${model#native:}"
          harbor run -y -d "$dataset" -a claude-code -m "anthropic/${model#anthropic/}" -k {{k}} -n {{n}} -o "$batch" \
            --agent-timeout-multiplier {{limit}} "${effort[@]}" \
            --allow-agent-host api.anthropic.com \
            --ak version={{claude_code}} \
            --ak disallowed_tools=WebSearch,WebFetch,RemoteTrigger,PushNotification,CronCreate,CronDelete,SendMessage,ListAgents \
            --ae ENABLE_TOOL_SEARCH=false --ae CLAUDE_CODE_DISABLE_AUTO_MEMORY=1
          ;;
        *)
          # Every provider's model in one harness, routed by models.toml
          # through the batch's key proxy: Harbor and the trials get the
          # proxy's token, never a key. bench/harness/mtx_hermes.py:
          # Harbor's adapter, with the bench's tools and no memory or skills.
          local route
          mapfile -t route < <(python3 -I bench/harness/models.py args "${model#hermes:}" "$proxy_url")
          MTX_PROXY_TOKEN="$(python3 -I bench/harness/models.py token "${model#hermes:}")" \
          env "${unset_keys[@]}" PYTHONPATH="$PWD/bench/harness${PYTHONPATH:+:$PYTHONPATH}" \
          harbor run -y -d "$dataset" -a mtx_hermes:Hermes "${route[@]}" -k {{k}} -n {{n}} -o "$batch" \
            --agent-timeout-multiplier {{limit}} "${effort[@]}" \
            --allow-agent-host "$(proxy_host)" \
            --ak version={{hermes}}
          ;;
      esac
    }
    # Watch the agents' logs while the batch runs; at the first refusal,
    # interrupt every harbor run writing to this batch.
    (
      while sleep 20; do
        hit="$(grep -rlE "$refused" "$batch"/*/*/agent 2>/dev/null | head -1 || true)"
        if [ -n "$hit" ]; then
          { echo "refused: $(grep -hoE "$refused" "$hit" | head -1)"; echo "in: $hit"; date -u +%FT%TZ; } > "$batch/.stopped"
          # INT first, for a clean stop; TERM for what's left: a run started
          # in the background (together=1) ignores INT.
          pkill -INT -f -- "harbor run .* -o $batch" || true
          sleep 30
          pkill -TERM -f -- "harbor run .* -o $batch" || true
          exit 0
        fi
      done
    ) &
    watcher=$!
    # No key may reach a trial's files, however the batch ends: a hit
    # quarantines the batch; a clean scan writes .scan-passed, which
    # `just publish` requires.
    scanned=""
    scan_batch() {
      [ -z "$scanned" ] || return 0
      scanned=1
      [ -z "$unguarded" ] || python3 -I bench/harness/models.py fetch-scan "$batch" || true
      kill "$watcher" ${proxy:+"$proxy"} 2> /dev/null || true
      if python3 -I bench/harness/models.py scan "$batch" > "$batch/.scan" 2>&1; then
        mv "$batch/.scan" "$batch/.scan-passed"
      else
        mv "$batch/.scan" "$batch/.quarantined"
        echo "quarantined: a provider key is in the batch's files, or a file couldn't be checked ($batch/.quarantined);"
        echo "rotate any key named there, and don't publish or share the batch"
        return 1
      fi
    }
    trap 'scan_batch || true' EXIT
    # Hermes's trials reach their providers through a proxy on this machine
    # that holds the keys (bench/harness/key_proxy.py): a key inside a trial
    # could be read by the agent from /proc and printed into its trajectory.
    # Each route (model) has its own token; the proxy listens on the docker
    # bridge, so only this machine's containers reach it.
    proxy=""
    unset_keys=(-u MTX_PROXY_SECRET)
    for key in $(python3 -I bench/harness/models.py keys); do unset_keys+=(-u "$key"); done
    if [ "${#hermes_models[@]}" -gt 0 ]; then
      MTX_PROXY_SECRET="$(python3 -I -c 'import secrets; print(secrets.token_hex(32))')"
      export MTX_PROXY_SECRET
      bridge="$(ip -4 -o addr show docker0 | awk '{print $4}' | cut -d/ -f1)"
      [ -n "$bridge" ] || { echo "no docker0 address for the key proxy"; exit 1; }
      python3 -I bench/harness/key_proxy.py --host "$bridge" --port 0 \
        "${hermes_models[@]}" > "$batch/.proxy" 2> "$batch/proxy.log" &
      proxy=$!
      for _ in $(seq 50); do [ -s "$batch/.proxy" ] && break; sleep 0.2; done
      [ -s "$batch/.proxy" ] || { echo "the key proxy didn't start:"; cat "$batch/proxy.log"; exit 1; }
      proxy_url="$(cat "$batch/.proxy")"
    fi
    status=0
    if [ "{{together}}" = 1 ]; then
      pids=()
      for model in {{models}}; do
        run_model "$model" & pids+=("$!")
        # Two runs starting at once can race on Harbor's task cache.
        sleep 60
      done
      for pid in "${pids[@]}"; do wait "$pid" || status=1; done
    else
      for model in {{models}}; do
        [ -e "$batch/.stopped" ] && break
        run_model "$model" || status=1
      done
    fi
    scan_batch || exit 1
    if [ -e "$batch/.stopped" ]; then
      echo "stopped: a provider refused an agent for quota or credit; no trial of this batch can be ranked"
      cat "$batch/.stopped"
      echo "raise the account's limit, then run the batch again"
      exit 1
    fi
    [ "$status" = 0 ] || { echo "a harbor run failed; see above"; exit 1; }
    if grep -rlF --exclude=.seed "$MTX_EVAL_SEED" "$batch"; then
      echo "the batch's secret appears in the job files above"; exit 1
    fi
    python3 -I bench/report.py "$batch"
    echo "browse it: just review $batch"
    if [ -n "$unguarded" ]; then
      echo "WARNING: this batch was not guarded (guarded: false in batch.json): it can't be published"
    else
      echo "publish it: just publish $batch {{version}}"
    fi

# Check each model's name and key with a 1-token request: names from
# bench/harness/models.toml, or `provider/model`. Reads the keys as `bench` does.
preflight models:
    #!/usr/bin/env bash
    set -euo pipefail
    set -a
    [ -f .env ] && . ./.env
    [ -f ~/.machine-trading.env ] && . ~/.machine-trading.env
    set +a
    python3 -I bench/harness/models.py preflight {{models}}

# The Hermes harness check (bench/checks/hermes), with no model: two trials
# per route through the key proxy to a stand-in provider on this machine;
# the second must see nothing the first planted, and the key must reach no
# trial file, nor anything the agent can read under /proc.
hermes-check task="flash-crash-spcx" routes="openai anthropic openrouter chat":
    #!/usr/bin/env bash
    set -euo pipefail
    status=0
    # The key proxy under attack first: tokens, paths, models, headers,
    # binding, error bodies, the scan, broken streams.
    python3 -I bench/checks/hermes/proxy_attacks.py \
      --bind "$(ip -4 -o addr show docker0 | awk '{print $4}' | cut -d/ -f1)" \
      --other "$(hostname -I | awk '{print $1}')" \
      --container-image {{registry}}/{{image}}:{{tag}}-runtime || status=1
    for route in {{routes}}; do
      bench/checks/hermes/probe.sh {{tasks}}/{{task}} {{hermes}} "$route" || status=1
    done
    exit "$status"

# Browse a bench's results: a batch, a Harbor job, one trial, or a
# published release by its version (`just review 0.9.0`).
review path:
    cargo run --release -q -- bench review {{path}}

# Publish a finished batch: upload it to the results dataset (needs hf and
# HF_TOKEN) and write bench/results/<version>/release.json, to commit.
publish batch version:
    [ ! -e "{{batch}}/.quarantined" ] || { echo "{{batch}} is quarantined: a provider key is in its files"; exit 1; }
    [ -e "{{batch}}/.scan-passed" ] || { echo "{{batch}} has no clean key scan (.scan-passed); run: python3 -I bench/harness/models.py scan {{batch}}"; exit 1; }
    cargo run --release -q -- bench publish {{batch}} {{version}}

# Compare two releases task by task, as in `just compare 0.9.0...0.8.0`.
compare range:
    cargo run --release -q -- bench compare {{range}} --open

# Copy the reference bot's strategy into the task's solution: its sources
# but the process's `main.rs`, and the model it reads, if any (a `model/`
# directory, or `model.*` files beside its Cargo.toml).
solution:
    #!/usr/bin/env bash
    set -euo pipefail
    for pair in {{references}}; do
      bot="${pair#*=}"; solution="{{tasks}}/${pair%%=*}/solution"
      mkdir -p "$solution/src"
      for src in "$bot"/src/*.rs; do
        if [ "$(basename "$src")" != main.rs ]; then cp "$src" "$solution/src/"; fi
      done
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
      bot="${pair#*=}"; solution="{{tasks}}/${pair%%=*}/solution"
      diff -ru -x main.rs "$bot/src" "$solution/src"
      if [ -d "$bot/model" ]; then diff -ru "$bot/model" "$solution/model"; fi
      for model in "$bot"/model.*; do
        if [ -e "$model" ]; then cmp "$model" "$solution/$(basename "$model")"; fi
      done
    done

# The checks every task change should pass: oracle ~1, and nop and every
# cheater 0, each grade completing. The component cheater's log must show
# every attempt refused; the process cheaters aren't graded (not_graded 1).
harbor-checks task="flash-crash-spcx" surfpool="" jobs="jobs": (image surfpool) check-solution
    #!/usr/bin/env bash
    set -euo pipefail
    # The checks run on the images `image` just built: a task still pinned to
    # a released digest would test the old grader instead.
    grep -q -E '^FROM {{registry}}/{{image}}:{{tag}}(-agent)?$' {{tasks}}/{{task}}/environment/Dockerfile \
      && grep -q '^FROM {{registry}}/{{image}}:{{tag}}-runtime$' {{tasks}}/{{task}}/tests/Dockerfile \
      || { echo "{{task}} isn't on {{image}}:{{tag}}; unpin its Dockerfiles first"; exit 1; }
    # The verifier draws its hidden set from the batch's secret.
    export MTX_EVAL_SEED="${MTX_EVAL_SEED:-$(python3 -c 'import secrets; print(secrets.token_hex(32))')}"
    harbor run -y -p {{tasks}}/{{task}} -a oracle -o {{jobs}}/oracle
    harbor run -y -p {{tasks}}/{{task}} -a nop -o {{jobs}}/nop
    PYTHONPATH=bench harbor run -y -p {{tasks}}/{{task}} -a checks.cheater_agent:ComponentCheaterAgent -o {{jobs}}/component-cheater
    PYTHONPATH=bench harbor run -y -p {{tasks}}/{{task}} -a checks.cheater_agent:CheaterAgent -o {{jobs}}/cheater
    PYTHONPATH=bench harbor run -y -p {{tasks}}/{{task}} -a checks.cheater_agent:SneakyAgent -o {{jobs}}/sneaky

# The commands a guarded agent's `mtx` offers, and no others: it grades a bot
# on scenario files (what `mtx-practice` runs) and checks a bot or a scenario
# file. Nothing that draws from a task's family or authors a task, and none of
# the host's commands (studio, bench, snapshots, calibration), which start
# from a checkout the image doesn't hold. A new command is a leak until it is
# listed here; the build's own test (`cargo test -p mtx-cli
# --no-default-features`) holds the same lists.
agent_commands := "bot grade help scenario"
agent_bot_commands := "check help"
agent_scenario_commands := "check help"

# Fail if an agent's image holds anything that answers its task: the anchors
# (reference bots), their source or the tasks' solutions, the checks' bots, or
# a scored scenario. A guarded image (a task whose environment starts from the
# base's `-agent` image) also fails if its `mtx` can draw scenarios, offers a
# command outside the allowlist above, or holds a scenario family's code.
# With a task, audits the image its task.toml's `docker_image` names, which
# must be `<tag>-<task>` (the one Harbor runs); with none, the base
# dev image, the base agent image, and the image of every guarded task. Runs
# on the images `image` built.
leak-audit task="":
    #!/usr/bin/env bash
    set -euo pipefail
    IFS='|' read -ra needles <<< "{{leak_needles}}|mtx-ladder-rungs"
    base="{{registry}}/{{image}}:{{tag}}"
    guarded() { grep -q -E '^FROM [^ ]*:[^@ ]*-agent(@| |$)' "{{tasks}}/$1/environment/Dockerfile"; }
    # The image Harbor runs for a task is the one task.toml's `docker_image`
    # names, not the Dockerfile's: it must be the task's own (`<tag>-<task>`).
    # Prints the reference to audit: as written when this machine holds it,
    # else its tag (a pinned reference not pulled here).
    task_image() {
      local want="$base-$1" ref
      ref="$(python3 -I -c 'import sys, tomllib; print(tomllib.load(open(sys.argv[1], "rb")).get("environment", {}).get("docker_image", ""))' "{{tasks}}/$1/task.toml")"
      if [ "${ref%@*}" != "$want" ]; then
        echo "LEAK: task.toml's docker_image for $1 is \"${ref:-<none>}\", not $want: Harbor runs that image, not the Dockerfile's" >&2
        return 1
      fi
      if docker image inspect "$ref" > /dev/null 2>&1; then echo "$ref"; else echo "$want"; fi
    }
    # The command names `mtx <args> --help` lists, one a line.
    listed() {
      docker run --rm --entrypoint mtx "$image" "$@" --help 2>&1 \
        | awk '/^Commands:/ { f = 1; next } f && /^[^ ]/ { f = 0 } f && /^  [a-z]/ { print $1 }' | sort -u
    }
    audit() {
      image="$1"; local guard="$2" found=0
      # Without the image every probe below fails, which reads as no leak.
      docker image inspect "$image" > /dev/null 2>&1 \
        || { echo "no image $image: run \`just image\` first" >&2; exit 2; }
      local path
      for path in /opt/mtx/anchors /tests /solution; do
        if docker run --rm --entrypoint test "$image" -e "$path"; then
          echo "LEAK: $image holds $path"; found=1
        fi
      done
      # The SDK holds the starter and nothing of any task's.
      local bot
      for bot in $(docker run --rm --entrypoint ls "$image" /opt/mtx/sdk/bots); do
        if [ "$bot" != starter ]; then echo "LEAK: $image holds /opt/mtx/sdk/bots/$bot"; found=1; fi
      done
      if [ "$guard" = guarded ]; then
        # The agent's mtx offers the allowlisted commands only: no scenario
        # generation, no task authoring, gates, MCP or market calibration,
        # and none of the host's commands.
        local extra
        extra="$(comm -13 <(printf '%s\n' {{agent_commands}} | sort -u) <(listed))"
        if [ -n "$extra" ]; then echo "LEAK: $image's mtx offers $(echo $extra)"; found=1; fi
        extra="$(comm -13 <(printf '%s\n' {{agent_bot_commands}} | sort -u) <(listed bot))"
        if [ -n "$extra" ]; then echo "LEAK: $image's mtx bot offers $(echo $extra)"; found=1; fi
        extra="$(comm -13 <(printf '%s\n' {{agent_scenario_commands}} | sort -u) <(listed scenario))"
        if [ -n "$extra" ]; then echo "LEAK: $image's mtx scenario offers $(echo $extra)"; found=1; fi
        if docker run --rm --entrypoint mtx "$image" grade --help 2>&1 | grep -q -e '--draw'; then
          echo "LEAK: $image's mtx offers grade --draw"; found=1
        fi
        # Generator source and family parameters: none ships with the agent.
        local gen_hits
        gen_hits="$(docker run --rm --entrypoint find "$image" / -xdev \( -path /proc -o -path /sys -o -path '*/registry' -o -path '*/.cargo' -o -path /opt/mtx/target \) -prune -o \
          \( -name generators.rs -o -name making.rs -o -name carry.rs -o -name weekend.rs -o -name 'calibration' -o -name 'mtx-grader*' -o -path '*/crates/grader*' \) -print 2>/dev/null || true)"
        if [ -n "$gen_hits" ]; then
          echo "LEAK: generator source in $image:"; echo "$gen_hits" | sed 's/^/  /'; found=1
        fi
        # The families' code, by its mangled type names, is not in the binary.
        if docker run --rm --entrypoint grep "$image" -qaE '[0-9](Shock|Making|Carry|Weekend)Family' /usr/local/bin/mtx; then
          echo "LEAK: $image's mtx holds a scenario family's code"; found=1
        fi
      fi
      local needle hits
      for needle in "${needles[@]}"; do
        hits="$(docker run --rm --entrypoint grep "$image" -rlsF --exclude-dir=proc --exclude-dir=sys \
          --exclude-dir=target --exclude-dir=.cargo --exclude-dir=registry -e "$needle" / || true)"
        if [ -n "$hits" ]; then
          echo "LEAK: \"$needle\" in:"; echo "$hits" | sed 's/^/  /'; found=1
        fi
      done
      [ "$found" = 0 ] && echo "no leaks in $image ($guard)"
      return "$found"
    }
    status=0
    if [ -n "{{task}}" ]; then
      if guarded "{{task}}"; then mode=guarded; else mode=unguarded; fi
      if image="$(task_image "{{task}}")"; then audit "$image" "$mode" || status=1; else status=1; fi
    else
      audit "$base" unguarded || status=1
      audit "$base-agent" guarded || status=1
      for dir in {{tasks}}/*/; do
        task="$(basename "$dir")"
        if guarded "$task"; then
          if image="$(task_image "$task")"; then audit "$image" guarded || status=1; else status=1; fi
        fi
      done
    fi
    exit "$status"

# Fail if a bot can reach what the grader keeps from it (its environment and
# MTX_EVAL_SEED, /tests, its temp directories, any keypair file): one
# market-making session in the runtime image `image` built, with a probe bot
# that checks each surface and reports it.
keypair-probe:
    bench/checks/keypairs/probe.sh {{registry}}/{{image}}:{{tag}}-runtime

# The network check for a guarded task (bench/checks/network): its agent's
# container can't reach github.com, ghcr.io or the rest of the internet, and
# still reaches the crates registry, its model's address and its chain.
network-probe task:
    bench/checks/network/probe.sh {{tasks}}/{{task}}

# The agent's `mtx` (`--no-default-features`: no generators, none of the
# host's commands) lints and passes its tests, and so does the build between
# it and the default (`host` without `generate`), which nothing ships and
# which would otherwise rot. CI runs the same lines.
agent-build-check:
    cargo clippy -p mtx-cli --no-default-features --all-targets -- -D warnings
    cargo test -p mtx-cli --no-default-features
    cargo clippy -p mtx-cli --no-default-features --features host --all-targets -- -D warnings
    cargo test -p mtx-cli --no-default-features --features host

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
# `count` overrides the sessions per set, `min_odds` the bootstrap floor (for
# short sets), `out` the report directory (it keeps
# `<out>/<task>/summary.json`, which `mtx gates` reads) and `public_seed`
# draws fixed, public sets from a label instead of fresh hidden ones.
ladder task="flash-crash-spcx" sets="4" jobs="4" parallel="1" count="" out="target/ladder" min_odds="" public_seed="":
    #!/usr/bin/env bash
    set -euo pipefail
    [ -f "{{tasks}}/{{task}}/checks/ladder.json" ] || { echo "{{task}} has no checks/ladder.json"; exit 1; }
    cargo build --release -q
    just bots > /dev/null
    (cd bench/checks/ladder/rungs && CARGO_TARGET_DIR="$PWD/../../../../target/ladder-rungs" \
      cargo build --release -q)
    count_args=()
    [ -n "{{count}}" ] && count_args+=(--count "{{count}}")
    [ -n "{{min_odds}}" ] && count_args+=(--min-rule-odds "{{min_odds}}")
    [ -n "{{public_seed}}" ] && count_args+=(--public-seed "{{public_seed}}")
    mkdir -p bench/checks/ladder/bin
    for bin in blind momentum regime-oracle flow-oracle quoter; do
      cp "target/ladder-rungs/release/$bin" bench/checks/ladder/bin/
    done
    for bot in {{tasks}}/{{task}}/checks/exploits/*/; do
      [ -x "$bot/build.sh" ] && CARGO_TARGET_DIR="$PWD/target/exploits" "$bot/build.sh" > /dev/null
    done
    # The shared exploit and probe bots, and the task's own probe wrapper,
    # that the ladder lists (bench/checks/exploits, bench/checks/probes,
    # bench/tasks/<task>/checks/rungs/probe).
    for bot in $(jq -r '.rungs[].dir' "{{tasks}}/{{task}}/checks/ladder.json" | sort -u); do
      case "$bot" in bench/checks/exploits/*|bench/checks/probes|bench/tasks/*/checks/rungs/probe)
        [ -x "$bot/build.sh" ] && CARGO_TARGET_DIR="$PWD/target/exploits" "$bot/build.sh" > /dev/null ;;
      esac
    done
    python3 -I bench/checks/ladder/ladder.py {{task}} --mtx "${CARGO_TARGET_DIR:-target}/release/mtx" \
      --snapshot snapshots/{{snapshot}} --sets {{sets}} --jobs {{jobs}} --parallel {{parallel}} \
      --out "{{out}}" ${count_args[@]+"${count_args[@]}"}

# The live runtime against a real Agave validator (docs/rfd/0004, step 5):
# `solana-test-validator` loads the plugin and feeds it slots, and a
# transfer sent over the TPU client lands with no RPC send. Needs
# `solana-test-validator` on the path, of the plugin interface's version.
agave:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build -p mtx-geyser
    case "$(uname -s)" in Darwin) lib=libmtx_geyser.dylib ;; *) lib=libmtx_geyser.so ;; esac
    MTX_TEST_GEYSER_PLUGIN="$PWD/${CARGO_TARGET_DIR:-target}/debug/$lib" \
      cargo test -p mtx-grader --test agave -- --ignored
    cargo test -p mtx-runtime --features live --test tpu -- --ignored

# syntax=docker/dockerfile:1.7
#
# The Machine Trading images, shared by every task. Build with `just image`.
#
#   runtime  ghcr.io/solana-foundation/machine-trading:<tag>-runtime
#            Verifiers start from it: the `mtx` grader and `just`, every chain snapshot
#            under /opt/mtx/snapshots, and the bot users. Debian slim, no
#            toolchain: a verifier runs bots, it never builds them.
#   dev      ghcr.io/solana-foundation/machine-trading:<tag>
#            Agents start from it: everything in runtime, a Rust toolchain,
#            `just`, the wasm32-wasip2 target, the bot SDK at /opt/mtx/sdk with its
#            dependencies compiled for it, and the starter strategy built at
#            /app/bot/bot.wasm.
#
# Tasks add what is theirs: the snapshot they trade on (MTX_SNAPSHOT) and
# their practice scenarios (/opt/mtx/practice) in environment/. Hidden sets
# are drawn inside the verifier, from the batch's secret.

ARG RUST_IMAGE=rust:1.97.1-bookworm
ARG RUST_SLIM_IMAGE=rust:1.97.1-slim-bookworm
ARG RUNTIME_IMAGE=debian:bookworm-slim

# Surfpool from a local checkout, when one is patched in for changes not
# pushed yet (`just image surfpool=DIR` passes it as this stage's context);
# empty otherwise, and the build uses the git sources Cargo.toml names.
FROM scratch AS surfpool

FROM ${RUST_IMAGE} AS build
ARG TARGETPLATFORM
# The Docker VM's memory, not its CPUs, bounds a Solana build.
ARG CARGO_BUILD_JOBS=6
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
# `just`: bots build with `just build`, and the grader starts a process bot
# (the anchors) with `just run`. Its own layer, so it builds once.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    cargo install --locked --root /opt/just just@1.49.0
WORKDIR /src/mtx
COPY Cargo.toml Cargo.lock rust-toolchain ./
COPY crates crates
COPY bots bots
# One target cache per platform: a multi-platform build runs both at once.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/mtx/target,id=mtx-target-${TARGETPLATFORM} \
    --mount=type=bind,from=surfpool,target=/src/surfpool \
    if [ -d /src/surfpool/crates ]; then \
      mkdir -p .cargo && printf '%s\n' \
        '[patch."https://github.com/solana-foundation/surfpool"]' \
        'surfpool-sdk = { path = "/src/surfpool/crates/sdk" }' \
        'surfpool-core = { path = "/src/surfpool/crates/core" }' \
        'surfpool-types = { path = "/src/surfpool/crates/types" }' > .cargo/config.toml; \
    fi \
    && cargo build --release --locked -p mtx-cli -p mtx-bot-reference -p mtx-bot-maker -p mtx-bot-carry -p mtx-bot-weekend \
    && cp target/release/mtx /usr/local/bin/mtx \
    && for anchor in flash-crash:reference market-making:maker funding-carry:carry weekend-gap:weekend; do \
      family="${anchor%%:*}"; bot="${anchor##*:}"; \
      mkdir -p "/opt/mtx/anchors/$family/bin"; \
      cp "bots/$bot/Justfile" "/opt/mtx/anchors/$family/Justfile"; \
      cp "target/release/mtx-$bot" "/opt/mtx/anchors/$family/bin/bot"; \
    done

# Verifiers: the grader, the snapshots, and one unprivileged user per
# session the grader can run at once (mtx grade --jobs). Graders run as root
# and start bots as these users, so bots can't read hidden scenarios or
# signal the grader. `mtx` links only libc, so slim Debian is enough;
# procps provides the pkill that cleans up after each bot.
FROM ${RUNTIME_IMAGE} AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends procps \
    && rm -rf /var/lib/apt/lists/* \
    && for user in mtx-bot mtx-bot-2 mtx-bot-3 mtx-bot-4; do \
      useradd --system --create-home --shell /usr/sbin/nologin "$user"; \
    done \
    && mkdir -p /app
COPY --from=build /usr/local/bin/mtx /usr/local/bin/mtx
COPY --from=build /opt/just/bin/just /usr/local/bin/just
# The reference bots, as the anchors hidden sets are scored against: the
# verifier runs them on the same freshly drawn sessions as the agent's bot.
# Verifier only: the agent's image never holds them.
COPY --from=build /opt/mtx/anchors/ /opt/mtx/anchors/
COPY snapshots/ /opt/mtx/snapshots/
WORKDIR /app

# Agents: the runtime plus a toolchain and the SDK. The slim Rust image's gcc
# is all the SDK's native dependencies need (ring, zstd build bundled C).
# Agents run as `agent` (task.toml sets agent.user) and own everything they
# build into: the bot, the shared target dir, the crate cache. mtx-practice
# runs bots as the agent itself, so dev needs no bot users.
FROM ${RUST_SLIM_IMAGE} AS dev
ARG CARGO_BUILD_JOBS=6
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
RUN apt-get update \
    && apt-get install -y --no-install-recommends procps jq \
    && rustup target add wasm32-wasip2 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --shell /bin/bash agent \
    && mkdir -p /app /opt/mtx/target /opt/mtx/practice \
    && chown agent:agent /app /opt/mtx/target
COPY --from=build /usr/local/bin/mtx /usr/local/bin/mtx
COPY --from=build /opt/just/bin/just /usr/local/bin/just
COPY bench/image/mtx-practice /usr/local/bin/mtx-practice
COPY bench/image/time-left /usr/local/bin/time-left
COPY snapshots/ /opt/mtx/snapshots/

# The SDK, with the repository's lockfile so bots build against the versions
# the grader was tested with.
COPY bench/image/sdk-workspace.toml /opt/mtx/sdk/Cargo.toml
COPY Cargo.lock /opt/mtx/sdk/Cargo.lock
COPY crates/core /opt/mtx/sdk/crates/core
COPY crates/kit /opt/mtx/sdk/crates/kit
COPY bots/starter /opt/mtx/sdk/bots/starter
ENV CARGO_HOME=/home/agent/.cargo
# One target dir for every build, so the agent's bot reuses the SDK's
# compiled dependencies instead of rebuilding Solana from scratch.
ENV CARGO_TARGET_DIR=/opt/mtx/target

# The agent's bot starts as a copy of the starter, built into bot.wasm. The crate cache
# stays whole: Cargo wants the downloaded tarballs, not only the extracted
# sources, so trimming it makes every agent build re-fetch from crates.io.
USER agent
RUN cp -r /opt/mtx/sdk/bots/starter /app/bot \
    && sed -i 's|path = "../../crates/kit"|path = "/opt/mtx/sdk/crates/kit"|; s|^name = "mtx-bot-starter"|name = "bot"|' /app/bot/Cargo.toml \
    && cp /opt/mtx/sdk/Cargo.lock /app/bot/Cargo.lock \
    && just --justfile /app/bot/Justfile build \
    && rm -rf /app/bot/target

USER root
WORKDIR /app

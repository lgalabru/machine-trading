# syntax=docker/dockerfile:1.7
#
# The Machine Trading images, shared by every task. Build with `just image`.
#
#   runtime  ghcr.io/solana-foundation/machine-trading:<tag>-runtime
#            Verifiers start from it: the `mt` grader, every chain snapshot
#            under /opt/mt/snapshots, and the bot users. Debian slim, no
#            toolchain: a verifier runs bots, it never builds them.
#   dev      ghcr.io/solana-foundation/machine-trading:<tag>
#            Agents start from it: everything in runtime, a Rust toolchain,
#            the bot SDK at /opt/mt/sdk with its dependencies compiled, and a
#            built starter bot at /app/bot.
#
# Tasks add what is theirs: the snapshot they trade on (MT_SNAPSHOT) and
# their practice scenarios (/opt/mt/practice) in environment/, and hidden
# scenarios in tests/.

ARG RUST_IMAGE=rust:1.97.1-bookworm
ARG RUST_SLIM_IMAGE=rust:1.97.1-slim-bookworm
ARG RUNTIME_IMAGE=debian:bookworm-slim

FROM ${RUST_IMAGE} AS build
ARG TARGETPLATFORM
# The Docker VM's memory, not its CPUs, bounds a Solana build.
ARG CARGO_BUILD_JOBS=6
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
WORKDIR /src/mt
COPY Cargo.toml Cargo.lock rust-toolchain ./
COPY crates crates
COPY bots bots
# One target cache per platform: a multi-platform build runs both at once.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/mt/target,id=mt-target-${TARGETPLATFORM} \
    cargo build --release --locked -p mt-cli \
    && cp target/release/mt /usr/local/bin/mt

# Verifiers: the grader, the snapshots, and one unprivileged user per
# session the grader can run at once (mt grade --jobs). Graders run as root
# and start bots as these users, so bots can't read hidden scenarios or
# signal the grader. `mt` links only libc, so slim Debian is enough;
# procps provides the pkill that cleans up after each bot.
FROM ${RUNTIME_IMAGE} AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends procps \
    && rm -rf /var/lib/apt/lists/* \
    && for user in mt-bot mt-bot-2 mt-bot-3 mt-bot-4; do \
      useradd --system --create-home --shell /usr/sbin/nologin "$user"; \
    done \
    && mkdir -p /app
COPY --from=build /usr/local/bin/mt /usr/local/bin/mt
COPY snapshots/ /opt/mt/snapshots/
WORKDIR /app

# Agents: the runtime plus a toolchain and the SDK. The slim Rust image's gcc
# is all the SDK's native dependencies need (ring, zstd build bundled C).
# Agents run as `agent` (task.toml sets agent.user) and own everything they
# build into: the bot, the shared target dir, the crate cache. mt-practice
# runs bots as the agent itself, so dev needs no bot users.
FROM ${RUST_SLIM_IMAGE} AS dev
ARG CARGO_BUILD_JOBS=6
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
RUN apt-get update \
    && apt-get install -y --no-install-recommends procps jq \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --shell /bin/bash agent \
    && mkdir -p /app /opt/mt/target /opt/mt/practice \
    && chown agent:agent /app /opt/mt/target
COPY --from=build /usr/local/bin/mt /usr/local/bin/mt
COPY bench/image/mt-practice /usr/local/bin/mt-practice
COPY snapshots/ /opt/mt/snapshots/

# The SDK, with the repository's lockfile so bots build against the versions
# the grader was tested with.
COPY bench/image/sdk-workspace.toml /opt/mt/sdk/Cargo.toml
COPY Cargo.lock /opt/mt/sdk/Cargo.lock
COPY crates/luce /opt/mt/sdk/crates/luce
COPY crates/botkit /opt/mt/sdk/crates/botkit
COPY bots/starter /opt/mt/sdk/bots/starter
ENV CARGO_HOME=/home/agent/.cargo
# One target dir for every build, so the agent's bot reuses the SDK's
# compiled dependencies instead of rebuilding Solana from scratch.
ENV CARGO_TARGET_DIR=/opt/mt/target

# The agent's bot starts as a copy of the starter, built. The crate cache
# stays whole: Cargo wants the downloaded tarballs, not only the extracted
# sources, so trimming it makes every agent build re-fetch from crates.io.
USER agent
RUN cp -r /opt/mt/sdk/bots/starter /app/bot \
    && sed -i 's|path = "../../crates/botkit"|path = "/opt/mt/sdk/crates/botkit"|; s|^name = "mt-bot-starter"|name = "bot"|; s|^name = "mt-starter"|name = "bot"|' /app/bot/Cargo.toml \
    && sed -i 's|--bin mt-starter|--bin bot|' /app/bot/build.sh \
    && cp /opt/mt/sdk/Cargo.lock /app/bot/Cargo.lock \
    && /app/bot/build.sh \
    && rm -rf /app/bot/target

USER root
WORKDIR /app

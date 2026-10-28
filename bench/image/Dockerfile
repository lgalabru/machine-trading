# syntax=docker/dockerfile:1.7
#
# The Open Trading Terminal image: one base for every task, for both the
# agent's environment and the verifier. Build with `just image`, which passes
# the Surfpool checkout as the `surfpool` build context.
#
# It holds the `ott` grader, every chain snapshot under /opt/ott/snapshots,
# a Rust toolchain, the bot SDK at /opt/ott/sdk with its dependencies
# compiled, and a built starter bot at /app/bot. Tasks add what is theirs:
# the snapshot they trade on (OTT_SNAPSHOT) and their practice scenarios
# (/opt/ott/practice) in environment/, and hidden scenarios in tests/.

ARG RUST_IMAGE=rust:1.97.1-bookworm

FROM ${RUST_IMAGE} AS build
ARG TARGETPLATFORM
# The Docker VM's memory, not its CPUs, bounds a Solana build.
ARG CARGO_BUILD_JOBS=6
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
COPY --from=surfpool . /src/surfpool-ofm
WORKDIR /src/ott
COPY Cargo.toml Cargo.lock rust-toolchain ./
COPY crates crates
COPY bots bots
# One target cache per platform: a multi-platform build runs both at once.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/ott/target,id=ott-target-${TARGETPLATFORM} \
    cargo build --release --locked -p ott-cli \
    && cp target/release/ott /usr/local/bin/ott

FROM ${RUST_IMAGE}
ARG CARGO_BUILD_JOBS=6
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
RUN apt-get update \
    && apt-get install -y --no-install-recommends procps jq \
    && rm -rf /var/lib/apt/lists/*
# Graders run as root and start bots as ott-bot, so bots can't read hidden
# scenarios or write rewards. Agents run as agent (task.toml sets
# agent.user), and own everything they build into: the bot, the shared
# target dir, the crate cache.
RUN useradd --system --create-home --shell /usr/sbin/nologin ott-bot \
    && useradd --create-home --shell /bin/bash agent \
    && mkdir -p /app /opt/ott/target /opt/ott/practice \
    && chown agent:agent /app /opt/ott/target
COPY --from=build /usr/local/bin/ott /usr/local/bin/ott
COPY bench/image/ott-practice /usr/local/bin/ott-practice
COPY snapshots/ /opt/ott/snapshots/

# The SDK, with the repository's lockfile so bots build against the versions
# the grader was tested with.
COPY bench/image/sdk-workspace.toml /opt/ott/sdk/Cargo.toml
COPY Cargo.lock /opt/ott/sdk/Cargo.lock
COPY crates/botkit /opt/ott/sdk/crates/botkit
COPY bots/starter /opt/ott/sdk/bots/starter
ENV CARGO_HOME=/home/agent/.cargo
# One target dir for every build, so the agent's bot reuses the SDK's
# compiled dependencies instead of rebuilding Solana from scratch.
ENV CARGO_TARGET_DIR=/opt/ott/target

# The agent's bot starts as a copy of the starter, built.
USER agent
RUN cp -r /opt/ott/sdk/bots/starter /app/bot \
    && sed -i 's|path = "../../crates/botkit"|path = "/opt/ott/sdk/crates/botkit"|; s|^name = "ott-bot-starter"|name = "bot"|; s|^name = "ott-starter"|name = "bot"|' /app/bot/Cargo.toml \
    && sed -i 's|--bin ott-starter|--bin bot|' /app/bot/build.sh \
    && cp /opt/ott/sdk/Cargo.lock /app/bot/Cargo.lock \
    && /app/bot/build.sh \
    && rm -rf /app/bot/target

USER root
WORKDIR /app

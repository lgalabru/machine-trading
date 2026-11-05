# syntax=docker/dockerfile:1
FROM rust:1.97.1-bookworm@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97 AS base
RUN apt-get update && apt-get install -y --no-install-recommends clang libclang-dev libudev-dev pkg-config python3 && rm -rf /var/lib/apt/lists/*
ENV CARGO_PROFILE_RELEASE_DEBUG=0 CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=6
WORKDIR /src

FROM base AS build
COPY Cargo.toml Cargo.lock rust-toolchain ./
COPY crates crates
COPY bots bots
COPY bench/backrun/searcher.py bench/backrun/
# Both variants have the same Cargo package name. Docker COPY timestamps can
# make Cargo reuse the other variant, so clean only this package under a lock.
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/usr/local/cargo/git --mount=type=cache,target=/src/target,sharing=locked \
    cargo clean --release -p mth-backrun && \
    cargo build --locked --release -p mth-backrun --bin mth-backrun && \
    target/release/mth-backrun grade --help && cp target/release/mth-backrun /usr/local/bin/mth-backrun

FROM build AS public-source
COPY bench/backrun/public-sdk.py bench/backrun/playground-main.rs bench/backrun/
RUN python3 bench/backrun/public-sdk.py /src /public-sdk

FROM base AS public-build
COPY --from=public-source /public-sdk /src
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/usr/local/cargo/git --mount=type=cache,target=/src/target,sharing=locked \
    cargo clean --release -p mth-backrun && \
    cargo build --offline --release -p mth-backrun --bin mth-backrun && \
    target/release/mth-backrun playground --help && \
    ! target/release/mth-backrun grade --help && \
    ! target/release/mth-backrun reference --help && \
    cp target/release/mth-backrun /usr/local/bin/mth-backrun

FROM debian:bookworm-slim@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3 AS runtime-base
RUN apt-get update && apt-get install -y --no-install-recommends libssl3 libudev1 python3 ca-certificates bash && rm -rf /var/lib/apt/lists/*
COPY bench/backrun/searcher.py /usr/local/bin/backrun-searcher
RUN useradd --uid 1000 --create-home searcher
WORKDIR /app
CMD ["sleep", "infinity"]

FROM runtime-base AS candidate
# Deliberately contains no reference-searcher or grader executable.

FROM runtime-base AS runtime
COPY --from=build /usr/local/bin/mth-backrun /usr/local/bin/mth-backrun

FROM base AS tooling
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/src/target \
    CARGO_TARGET_DIR=/src/target cargo install --locked --version 4.4.0 cargo-build-sbf
COPY programs/backrun-executor /opt/backrun/programs/backrun-executor
RUN useradd --uid 1000 --create-home agent && mkdir -p /app/bot && chown -R agent:agent /app /usr/local/cargo
RUN mkdir -p /home/agent/.rustup/toolchains && \
    cp /usr/local/rustup/settings.toml /home/agent/.rustup/settings.toml && \
    for toolchain in /usr/local/rustup/toolchains/*; do ln -s "$toolchain" /home/agent/.rustup/toolchains/; done && \
    chown -R agent:agent /home/agent
ENV RUSTUP_HOME=/home/agent/.rustup CARGO_TARGET_DIR=/app/target
USER agent
RUN cargo-build-sbf --tools-version v1.57 --manifest-path /opt/backrun/programs/backrun-executor/Cargo.toml --sbf-out-dir /tmp/executor-check -- --locked
USER root
WORKDIR /app

FROM tooling AS development
COPY --from=public-build /src /src
COPY --from=public-build /usr/local/bin/mth-backrun /usr/local/bin/mth-backrun
CMD ["sleep", "infinity"]

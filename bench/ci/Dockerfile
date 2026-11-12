# syntax=docker/dockerfile:1.7
#
# The image CI's Rust jobs run in, so a run installs nothing: the pinned
# toolchain with clippy and the wasm32-wasip2 target, nightly rustfmt (the
# repo's rustfmt.toml uses nightly options), the system libraries the Solana
# crates link, `just`, Node (the site build and the viewer check), Python,
# and the bot users `mtx grade` runs bots as.
#
# Built and pushed by .github/workflows/ci-image.yml when this directory or
# rust-toolchain changes; ci.yml and specs.yml run in `:latest`.

ARG RUST_IMAGE=rust:1.97.1-bookworm
FROM ${RUST_IMAGE}

RUN apt-get update \
    && apt-get install -y --no-install-recommends libudev-dev clang libclang-dev jq python3 python3-venv curl ca-certificates git \
    && curl -fsSL https://deb.nodesource.com/setup_24.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/*

RUN rustup component add clippy rustfmt \
    && rustup target add wasm32-wasip2 \
    && rustup toolchain install nightly --profile minimal --component rustfmt

RUN cargo install --locked --root /usr/local just@1.49.0

# The users a bot runs as, one per concurrent session (session::BOT_USERS).
RUN for user in mtx-bot mtx-bot-2 mtx-bot-3 mtx-bot-4; do \
      useradd --system --create-home --shell /usr/sbin/nologin "$user"; \
    done

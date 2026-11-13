# syntax=docker/dockerfile:1.7
#
# The image CI's Rust jobs run in, so a run installs nothing: the pinned Rust
# image plus setup.sh (toolchain components, system libraries, `just`, Node,
# Python and the bot users).
#
# Built and pushed by .github/workflows/ci-image.yml, tagged with a hash of
# this directory and rust-toolchain; ci.yml runs in it.

ARG RUST_IMAGE=rust:1.97.1-bookworm
FROM ${RUST_IMAGE}

COPY setup.sh /tmp/setup.sh
RUN bash /tmp/setup.sh && rm /tmp/setup.sh

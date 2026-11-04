# The runtime image (bench/image/Dockerfile, target runtime): the grader and
# the snapshots, without the agent's toolchain. Harbor copies the agent's
# /app/bot in.
FROM ghcr.io/solana-foundation/machine-trading:0.9.0-runtime@sha256:9426c5beb56ac4d0e9874e112b80a7cc4dc866a36ee6ede130313a645d769893
ENV MTX_SNAPSHOT=/opt/mtx/snapshots/phoenix-spcx-2026-10-29
COPY . /tests/
# Bots run as mtx-bot* and must not read the scenarios they're graded on.
RUN chown -R root:root /tests && chmod -R go-rwx /tests
WORKDIR /tests

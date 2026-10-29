# The runtime image (bench/image/Dockerfile, target runtime): the grader and
# the snapshots, without the agent's toolchain. Harbor copies the agent's
# /app/bot in.
FROM ghcr.io/solana-foundation/machine-trading:0.6.0-runtime@sha256:64b6d14af8e8e26b9f1db298036d24fb7c32fa2a71af6952fc09837d9d4f3d90
ENV MTH_SNAPSHOT=/opt/mth/snapshots/phoenix-spcx-2026-10-29
COPY . /tests/
# Bots run as mth-bot* and must not read the scenarios they're graded on.
RUN chown -R root:root /tests && chmod -R go-rwx /tests
WORKDIR /tests

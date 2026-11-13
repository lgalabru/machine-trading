# The runtime image (bench/image/Dockerfile, target runtime): the grader and
# the snapshots, without the agent's toolchain. Harbor copies the agent's
# /app/bot in.
FROM ghcr.io/solana-foundation/machine-trading:0.12.0-runtime@sha256:10b4ad0fef9b0564455191622fc20cfbc6267d94cb1cdda82791db2bc0ca89f0
ENV MTX_SNAPSHOT=/opt/mtx/snapshots/phoenix-spcx-2026-10-29
# The task's name, before anything is copied in: the tasks' Dockerfiles are
# otherwise identical, and BuildKit building two at once has put one task's
# files into the other's image (2026-11-11). test.sh checks it.
ENV MTX_TASK=flash-crash-spcx
COPY . /tests/
# Bots run as mtx-bot* and must not read the scenarios they're graded on.
RUN chown -R root:root /tests && chmod -R go-rwx /tests
WORKDIR /tests

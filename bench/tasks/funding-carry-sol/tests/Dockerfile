# The runtime image (bench/image/Dockerfile, target runtime): the grader and
# the snapshots, without the agent's toolchain. Harbor copies the agent's
# /app/bot in.
FROM ghcr.io/solana-foundation/machine-trading:0.14.0-runtime@sha256:87b622d3d1292d9a018f78c07e54b95e699a8955b579b8c0c53f5f1fd580c9a5
ENV MTX_SNAPSHOT=/opt/mtx/snapshots/phoenix-perps-2026-11-13
# The task's name, before anything is copied in: the tasks' Dockerfiles are
# otherwise identical, and BuildKit building two at once has put one task's
# files into the other's image (2026-11-11). test.sh checks it.
ENV MTX_TASK=funding-carry-sol
COPY . /tests/
# Bots run as mtx-bot* and must not read the scenarios they're graded on.
RUN chown -R root:root /tests && chmod -R go-rwx /tests
WORKDIR /tests

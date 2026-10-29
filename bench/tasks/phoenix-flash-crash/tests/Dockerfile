# The runtime image (bench/image/Dockerfile, target runtime): the grader and
# the snapshots, without the agent's toolchain. Harbor copies the agent's
# /app/bot in.
FROM ghcr.io/solana-foundation/machine-trading:0.5.0-runtime
ENV MT_SNAPSHOT=/opt/mt/snapshots/phoenix-spcx-2026-10-29
COPY . /tests/
# Bots run as mt-bot* and must not read the scenarios they're graded on.
RUN chown -R root:root /tests && chmod -R go-rwx /tests
WORKDIR /tests

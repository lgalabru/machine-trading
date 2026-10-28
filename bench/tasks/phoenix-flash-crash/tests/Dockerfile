# The runtime image (bench/image/Dockerfile, target runtime): the grader and
# the snapshots, without the agent's toolchain. Harbor copies the agent's
# /app/bot in.
FROM ghcr.io/solana-foundation/open-trading:0.3.0-runtime
ENV OTT_SNAPSHOT=/opt/ott/snapshots/phoenix-sol-2026-10-27
COPY . /tests/
# Bots run as ott-bot* and must not read the scenarios they're graded on.
RUN chown -R root:root /tests && chmod -R go-rwx /tests
WORKDIR /tests

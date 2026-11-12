# The dev image (bench/image/Dockerfile, target dev): the grader, the chain
# snapshots, a Rust toolchain, the bot SDK at /opt/mtx/sdk with its
# dependencies compiled, and a built starter bot at /app/bot.
FROM ghcr.io/solana-foundation/machine-trading:0.12.0
ENV MTX_SNAPSHOT=/opt/mtx/snapshots/phoenix-spcx-2026-10-29
# The task's name, before anything is copied in: the tasks' Dockerfiles are
# otherwise identical, and BuildKit building two at once has put one task's
# files into the other's image (2026-11-11). test.sh checks it.
ENV MTX_TASK=funding-carry-spcx
COPY practice/ /opt/mtx/practice/
WORKDIR /app

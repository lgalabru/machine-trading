# The dev image (bench/image/Dockerfile, target dev): the grader, the chain
# snapshots, a Rust toolchain, the bot SDK at /opt/mtx/sdk with its
# dependencies compiled, and a built starter bot at /app/bot.
FROM ghcr.io/solana-foundation/machine-trading:0.9.0@sha256:19a863e1772dd1dfcea04e1f75b7d9863ccb58f6c3847b186a55e0fe2d5c2b31
ENV MTX_SNAPSHOT=/opt/mtx/snapshots/phoenix-spcx-2026-10-29
COPY practice/ /opt/mtx/practice/
WORKDIR /app

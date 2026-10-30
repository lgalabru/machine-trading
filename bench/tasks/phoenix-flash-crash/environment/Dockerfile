# The dev image (bench/image/Dockerfile, target dev): the grader, the chain
# snapshots, a Rust toolchain, the bot SDK at /opt/mth/sdk with its
# dependencies compiled, and a built starter bot at /app/bot.
FROM ghcr.io/solana-foundation/machine-trading:0.7.0@sha256:e6db1d58cba5bf4e2f9d6fde875bb859d7cd8c872563cc4dcba03e986bedd1fc
ENV MTH_SNAPSHOT=/opt/mth/snapshots/phoenix-spcx-2026-10-29
COPY practice/ /opt/mth/practice/
WORKDIR /app

# The dev image (bench/image/Dockerfile, target dev): the grader, the chain
# snapshots, a Rust toolchain, the bot SDK at /opt/mth/sdk with its
# dependencies compiled, and a built starter bot at /app/bot.
FROM ghcr.io/solana-foundation/machine-trading:0.6.0@sha256:ecb850edbce46d8b8185aa1c2105e3261c63bf76d5b42695d962bc6182860e31
ENV MTH_SNAPSHOT=/opt/mth/snapshots/phoenix-spcx-2026-10-29
COPY practice/ /opt/mth/practice/
WORKDIR /app

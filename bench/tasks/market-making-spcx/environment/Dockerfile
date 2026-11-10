# The dev image (bench/image/Dockerfile, target dev): the grader, the chain
# snapshots, a Rust toolchain, the bot SDK at /opt/mtx/sdk with its
# dependencies compiled, and a built starter bot at /app/bot.
FROM ghcr.io/solana-foundation/machine-trading:0.11.0@sha256:92dde3255ec38ad16a75487c2eabe903cf34757b5e4cbef43ce376bd224f057c
ENV MTX_SNAPSHOT=/opt/mtx/snapshots/phoenix-spcx-2026-10-29
COPY practice/ /opt/mtx/practice/
WORKDIR /app

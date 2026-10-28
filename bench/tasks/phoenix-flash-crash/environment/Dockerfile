# The dev image (bench/image/Dockerfile, target dev): the grader, the chain
# snapshots, a Rust toolchain, the bot SDK at /opt/ott/sdk with its
# dependencies compiled, and a built starter bot at /app/bot.
FROM ghcr.io/solana-foundation/open-trading:0.3.0
ENV OTT_SNAPSHOT=/opt/ott/snapshots/phoenix-sol-2026-10-27
COPY practice/ /opt/ott/practice/
WORKDIR /app

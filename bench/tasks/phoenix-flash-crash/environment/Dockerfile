# The dev image (bench/image/Dockerfile, target dev): the grader, the chain
# snapshots, a Rust toolchain, the bot SDK at /opt/mt/sdk with its
# dependencies compiled, and a built starter bot at /app/bot.
FROM ghcr.io/solana-foundation/machine-trading:0.5.0
ENV MT_SNAPSHOT=/opt/mt/snapshots/phoenix-spcx-2026-10-29
COPY practice/ /opt/mt/practice/
WORKDIR /app

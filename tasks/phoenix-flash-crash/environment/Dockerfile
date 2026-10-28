# The Open Trading Terminal image (docker/Dockerfile in
# solana-foundation/open-trading, pushed by `just push-image`): the
# grader, the chain snapshots, the bot SDK at /opt/ott/sdk, and a built
# starter bot at /app/bot.
FROM ghcr.io/solana-foundation/open-trading:0.1.0@sha256:b20c187a2496bd9b03b489641f10868e5561597ee3a3cbe546248d31afdb5d24
ENV OTT_SNAPSHOT=/opt/ott/snapshots/phoenix-sol-2026-10-27
COPY practice/ /opt/ott/practice/
WORKDIR /app

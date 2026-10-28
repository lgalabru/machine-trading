# The Open Trading Terminal image (bench/image/Dockerfile in
# solana-foundation/open-trading, pushed by `just push-image`): the
# grader, the chain snapshots, the bot SDK at /opt/ott/sdk, and a built
# starter bot at /app/bot.
FROM ghcr.io/solana-foundation/open-trading:0.2.0
ENV OTT_SNAPSHOT=/opt/ott/snapshots/phoenix-sol-2026-10-27
COPY practice/ /opt/ott/practice/
WORKDIR /app

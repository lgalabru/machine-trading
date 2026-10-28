# The same image the agent starts from (see environment/Dockerfile).
FROM ghcr.io/solana-foundation/open-trading:0.1.0@sha256:b20c187a2496bd9b03b489641f10868e5561597ee3a3cbe546248d31afdb5d24
ENV OTT_SNAPSHOT=/opt/ott/snapshots/phoenix-sol-2026-10-27
# Harbor copies the agent's /app/bot in; drop the starter so nothing of it
# survives under the agent's files.
RUN rm -rf /app/bot
COPY . /tests/
# Bots run as ott-bot and must not read the scenarios they're graded on.
RUN chown -R root:root /tests && chmod -R go-rwx /tests
WORKDIR /tests

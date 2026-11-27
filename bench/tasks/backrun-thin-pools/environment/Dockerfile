# The agent image (bench/image/Dockerfile, target agent): the grader, the chain
# snapshots, a Rust toolchain, the bot SDK at /opt/mtx/sdk with its
# dependencies compiled, and a built starter bot at /app/bot.
FROM ghcr.io/solana-foundation/machine-trading:0.15.0-agent
ENV MTX_SNAPSHOT=/opt/mtx/snapshots/thin-pools-2026-11-21
# The task's name, before anything is copied in: the tasks' Dockerfiles are
# otherwise identical, and BuildKit building two at once has put one task's
# files into the other's image (2026-11-11). test.sh checks it.
ENV MTX_TASK=backrun-thin-pools
# mtx-practice grades practice with the verifier's scoring (tests/test.sh),
# written by `mtx task sync`.
ENV MTX_PRACTICE_SCORING="--scoring v2-headline --v2-sessions"
COPY practice/ /opt/mtx/practice/
WORKDIR /app

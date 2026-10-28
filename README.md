<div align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/banner-github-dark.png">
    <source media="(prefers-color-scheme: light)" srcset="docs/assets/banner-github-light.png">
    <img alt="Open Trading Terminal" width="100%" src="docs/assets/banner-github-light.png">
  </picture>
</div>

<p align="center">
  <a href="https://hub.harborframework.com/datasets/solana-foundation/open-trading"><img alt="Harbor dataset" src="https://img.shields.io/badge/harbor-solana--foundation%2Fopen--trading-blue"></a>
</p>

**Harbor tasks that evaluate trading agents on Solana.** An agent writes a bot.
The bot is graded on hidden market scenarios replayed on an offline copy of
mainnet, which Surfpool restores from a pinned snapshot.

## Layout

| Path | What it is |
| --- | --- |
| `crates/core` | Grader: offline worlds, scenarios, the RPC guard, sessions, scoring. |
| `crates/cli` | The `ott` binary. |
| `crates/botkit` | `ott-botkit`, the SDK bots build on. It ships in the task image. |
| `bots/starter` | The bot agents start from. It trades nothing, so it scores 0. |
| `bots/reference` | The reference strategy. Its PnL defines a score of 1, and it is the task's oracle solution. |
| `checks/` | A cheating bot and a Harbor agent that installs it. |
| `snapshots/<name>` | Pinned chain state. `surfnet.json` (~170 MB) is gitignored and rebuilt with `just snapshot-prepare`. |
| `docker/` | The one image every task builds on, for both the agent and the verifier. |
| `tasks/` | The `solana-foundation/open-trading` Harbor dataset. |
| `tasks/phoenix-flash-crash` | A task. Practice scenarios live in `environment/practice`, hidden ones in `tests/scenarios`. |
| `strategy/` | The original TypeScript prototype. |

The `surfpool-sdk` dependency points at `../surfpool-ofm`, a local
integration branch: the Phoenix scenario templates from surfpool#862, plus
`SurfnetBuilder::snapshot_file`.

## How a session works

1. Restore the snapshot into an offline surfnet, and jump the clock to the
   fork slot.
2. Onboard a fresh Phoenix trader with 10,000 USDC of collateral.
3. Start the bot's `run.sh`. It runs as an unprivileged user behind an RPC
   guard that forwards standard Solana methods only.
4. Register the scenario. Its waypoints become stepped Phoenix market moves,
   at most 4% per step to stay within the oracle's 5% band.
5. At the last slot, stop the bot and value the account at the mark price.

Score per scenario: `clamp(pnl / reference_pnl, 0, 1)`. It is 0 for any
refused RPC call, or if the bot ends the session liquidatable.

## Workflow

```sh
just bots                     # build bin/bot for the starter and reference
just grade bots/reference tasks/phoenix-flash-crash/environment/practice/*.json
just scenarios DIR NAME 3 SEED                 # draw scenarios
just calibrate DIR/*.json                      # record reference PnL
just image                    # build the task image locally (slow the first time)
just harbor-checks            # oracle ~1, nop 0, cheater 0, through Harbor
```

After changing `bots/reference`, run `just solution` and recalibrate every
scenario.

## The task image

Every task starts from `ghcr.io/solana-foundation/open-trading`,
in both its `environment/` and its `tests/`. The image holds the `ott`
grader, every snapshot under `/opt/ott/snapshots`, a Rust toolchain, the bot
SDK with its dependencies compiled, and a built starter bot at `/app/bot`.
A task's Dockerfiles only select a snapshot (`OTT_SNAPSHOT`) and add their
scenarios: practice ones in `environment/`, hidden ones in `tests/`.

To release an image change, bump `tag` in the Justfile, then:

```sh
just push-image               # amd64 + arm64; ~40 min on Apple silicon
just pin-image                # pin every task to the pushed digest
harbor publish tasks --public
```

## Known gaps

- The bot shares the verifier container with the surfnet. A bot that scans
  localhost ports can find the surfnet's own RPC port and skip the guard.
  The fix is to run the bot in a sidecar container that can reach only the
  guard.
- The restored clock is near real time, not the fork's exact timestamp,
  until Surfpool can restore both the slot and the unix time.

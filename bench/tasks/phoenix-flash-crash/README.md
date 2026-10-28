# solana-foundation/phoenix-flash-crash

The agent writes a bot that trades Phoenix SOL perpetuals through flash
crashes. The market is an offline copy of Solana mainnet: Phoenix's SOL
market, snapshotted on 2026-10-27 and restored into a
[Surfpool](https://github.com/solana-foundation/surfpool) surfnet at the
snapshot's slot and time.

**What it measures.** A good bot profits because it buys forced selling below
fair value and takes profit on the rebound, while sizing and exiting so that
the crashes that never recover don't eat those gains. Most crashes rebound
(~65%), but some stall (~20%) and some keep sliding (~15%). A bot that buys
every dip and holds wins most sessions and still loses overall.

- **Agent environment:** a Rust toolchain, the `ott-botkit` SDK, a starter
  bot that builds and never trades, six practice scenarios, the scenario
  generator, and `ott-practice`, which grades the bot exactly as the verifier
  does.
- **Verifier:** receives only `/app/bot`. With no network, it replays twelve
  hidden scenarios, each on a fresh surfnet. Before the bot starts, every
  cheatcode is locked for good. The bot runs as an unprivileged user behind an
  RPC proxy that forwards standard Solana methods only.
- **Reward:** `clamp(Σ bot PnL / Σ reference PnL, 0, 1)` over the twelve
  scenarios. Losses count, so risk control matters. The reward is 0 if any
  session makes a refused RPC call or ends liquidatable. `reward.json` also
  reports the unclamped `pnl_ratio`, the totals, and each scenario's PnL.

## Checks

| Agent | Reward | What it shows |
| --- | --- | --- |
| oracle (the reference bot) | 1.0 | The task is solvable and the reference is calibrated |
| nop (starter bot) | 0.0 | Doing nothing earns nothing |
| cheater | 0.0 | Cheatcodes through the proxy disqualify; `/tests` and the reward file are out of reach; ignoring SIGTERM doesn't extend the session |
| sneaky | 0.0 | Cheatcodes, airdrops and `exit` on the surfnet's own port are refused |

## Realism limits

- **Static replay.** The scenario moves Phoenix's oracle and re-crosses the
  book along a scripted path. Makers don't react to the bot, and the bot's own
  orders don't move the scripted price.
- **No MEV or competing takers.** Nobody front-runs or sandwiches the bot.
- **No funding.** Sessions last under two minutes; perp funding isn't simulated.
- **One market, one snapshot.** Depth and fees are mainnet's on 2026-10-27.

## Held-out data

The hidden scenarios ship in `tests/scenarios` with the task, so treat scores
on this public version as a development benchmark, not a held-out
evaluation.

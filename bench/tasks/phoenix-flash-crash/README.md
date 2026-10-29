# solana-foundation/phoenix-flash-crash

The agent writes a bot that trades Phoenix SPCX perpetuals (SpaceX
shares) through flash crashes and squeezes. The market is an offline copy of
Solana mainnet: Phoenix's SPCX market, snapshotted on 2026-10-29 and restored into a
[Surfpool](https://github.com/solana-foundation/surfpool) surfnet at the
snapshot's slot and time.

**What it measures.** SPCX shocks 5–12%: half the time it crashes, half the
time it squeezes. Most shocks revert (~65%), but some stall (~20%) and some
keep going (~15%). A good bot reads which way the shock is going, takes the
other side of the forced flow, and sizes and exits so that the shocks that
don't revert can't take it past its loss limit. Betting blind at the open
has no edge: it wins half the shocks and loses the other half.

- **Agent environment:** a Rust toolchain, the `mt-botkit` SDK, a starter
  bot that builds and never trades, six practice scenarios, the scenario
  generator, and `mt-practice`, which grades the bot exactly as the verifier
  does.
- **Verifier:** a 384 MB Debian slim image with the grader and the snapshot,
  and no toolchain. It receives only `/app/bot`. With no network, it replays
  twelve hidden scenarios, one at a time, each on a fresh surfnet. Before the bot starts, every
  cheatcode is locked for good. The bot runs as an unprivileged user behind an
  RPC proxy that forwards standard Solana methods only.
- **Reward:** `clamp(Σ bot PnL / Σ hindsight bound, 0, 1)` over the twelve
  scenarios. The hindsight bound is the full position cap held the right way
  through every move at no cost: no bot reaches it, so the score doesn't
  saturate. The reference (fade the shock in two tranches, hold) scores
  about 0.2 and is reported as a baseline.
- **Risk:** positions are capped at 70 SPCX (about 1× collateral), and a session may not draw
  down more than $1,000 (10% of collateral) from its peak. Breaking either,
  a refused RPC call, or ending liquidatable scores 0. Worst session, tail
  loss, deepest drawdown, profitable share, and a Sharpe ratio are reported
  alongside.

## Checks

| Agent | Reward | What it shows |
| --- | --- | --- |
| oracle (the reference bot) | ~0.2 | The task is solvable and the reference is calibrated |
| nop (starter bot) | 0.0 | Doing nothing earns nothing |
| cheater | 0.0 | Cheatcodes through the proxy disqualify; `/tests` is unreadable; writing the reward file counts as tampering; ignoring SIGTERM doesn't extend the session |
| sneaky | 0.0 | Cheatcodes, airdrops and `exit` on the surfnet's own port are refused |

## Realism limits

- **Static replay.** The scenario moves Phoenix's oracle and re-crosses the
  book along a scripted path. Makers don't react to the bot, and the bot's own
  orders don't move the scripted price.
- **No MEV or competing takers.** Nobody front-runs or sandwiches the bot.
- **No funding.** Sessions last under two minutes; perp funding isn't simulated.
- **One market, one snapshot.** Depth and fees are mainnet's on 2026-10-29.

## Held-out data

The hidden scenarios ship in `tests/scenarios` with the task, so treat scores
on this public version as a development benchmark, not a held-out
evaluation.

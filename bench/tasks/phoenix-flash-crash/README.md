# solana-foundation/phoenix-flash-crash

The agent writes a bot that trades Phoenix SPCX perpetuals (SpaceX
shares) through flash crashes and squeezes. The market is an offline copy of
Solana mainnet: Phoenix's SPCX market, snapshotted on 2026-10-29 and restored into a
[Surfpool](https://github.com/solana-foundation/surfpool) surfnet at the
snapshot's slot and time.

**What it measures.** SPCX shocks in one to three legs of 0.6–8%, most landing at once, among fizzles that give their move back: half the
time it crashes, half the time it squeezes. Most shocks revert (~65%), but
some stall (~20%) and some keep going (~15%). A good bot reads which way the
shock is going, takes the other side of the forced flow, and sizes and exits
so that the shocks that don't revert can't take it past its loss limit.
Betting blind at the open has no edge: it wins half the shocks and loses the
other half. The path's shape gives nothing away beyond what a trader would
see: moves ramp in uneven steps, legs are drawn independently, pauses and
holds share one range, the session ends a random while after the last move,
and the mark wobbles in between.

- **Agent environment:** a Rust toolchain, the `mth-botkit` SDK, a starter
  bot that builds and never trades, six practice scenarios, the scenario
  generator, and `mth-practice`, which grades the bot exactly as the verifier
  does.
- **Verifier:** a Debian slim image with the grader, the snapshot and the
  reference bot, and no toolchain. It receives only `/app/bot`. With no
  network, it draws thirty-six hidden sessions from the batch's secret
  (`MTH_EVAL_SEED`, passed by Harbor; it refuses to grade without one),
  grades the reference on them, then the bot, one session at a time, each on
  a fresh surfnet. Before the bot starts, every cheatcode is locked for good,
  and the surfnet serves RPC only at a secret path: the bot's only way in is
  an RPC proxy that forwards standard Solana methods only.
- **Turns:** the world moves in steps of two slots and waits for the bot
  between them (`mth-turn-1`), so the same bot on the same session trades
  the same way every time, and a session takes as long as the bot thinks,
  not its 80–180 seconds of market time.
- **Reward:** `clamp(Σ bot PnL / Σ hindsight bound, 0, 1)` over the thirty-six
  scenarios. The hindsight bound is the full position cap held the right way
  through every move at no cost: no bot reaches it, so the score doesn't
  saturate. The reference (ride each move while it runs, fade it once it
  stops) scores about 0.04, and `skill`, the bot's PnL over the reference's, is reported
  alongside: the headline for comparing bots.
- **Risk:** positions are capped at 70 SPCX (about 1× collateral), and a session may not draw
  down more than $1,000 (10% of collateral) from its peak. Breaking either,
  a refused RPC call, or ending liquidatable scores 0. Worst session, tail
  loss, deepest drawdown, profitable share, and a Sharpe ratio are reported
  alongside.

## Checks

| Agent | Reward | What it shows |
| --- | --- | --- |
| oracle (the reference bot) | ~0.04 | The task is solvable and the reference is calibrated |
| nop (starter bot) | 0.0 | Doing nothing earns nothing |
| cheater | 0.0 | Cheatcodes through the proxy disqualify; `/tests` is unreadable; writing the reward file counts as tampering; ignoring SIGTERM doesn't extend the session |
| sneaky | 0.0 | The surfnet's own port answers nothing without its secret path |

`just ladder flash-crash` grades the ladder (`bench/checks/ladder`) on fresh
hidden sets: blind bets, a dip-buyer and momentum below the reference, a
regime oracle above it, the frozen exploit bots within their limits.

## Realism limits

- **Static replay.** The scenario moves Phoenix's oracle and re-crosses the
  book along a scripted path. Makers don't react to the bot, and the bot's own
  orders don't move the scripted price.
- **No latency race.** The world waits for the bot each step, so a bot that
  thinks faster gains nothing within a step. That's the price of grades that
  replay.
- **No MEV or competing takers.** Nobody front-runs or sandwiches the bot.
- **No funding.** Sessions last under two minutes; perp funding isn't simulated.
- **One market, one snapshot.** Depth and fees are mainnet's on 2026-10-29.

## Held-out data

No hidden scenario ships with the task: each batch's are drawn inside the
verifier from a secret the batch's runner holds, and reports carry only the
secret's SHA-256. The generator is public (agents get it), so the family is
known and the draws aren't. Every trial in a batch is graded on the same
set.

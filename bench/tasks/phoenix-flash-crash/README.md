# solana-foundation/phoenix-flash-crash

The agent's bot trades Phoenix's SPCX (SpaceX) perpetual through flash
crashes and squeezes.

| | |
| --- | --- |
| Market | Phoenix SPCX perp, mainnet snapshot of 2026-10-29 |
| Sessions | 72 hidden per grade, stratified by direction and aftermath; 6 practice. Each session is 80–180 s of market time. |
| Mandate | 70 SPCX position cap (about 1× collateral); $1,000 loss limit (10% of collateral) |
| Bound | Hindsight: the full cap held the right way through every move, at no cost |
| Reference | Reward ~0.04. It rides each move while it runs, and fades it once it stops. |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot reads which way a shock is going and takes the other side of the
forced flow. It sizes and exits so that the shocks that don't revert can't
take it past its loss limit.

- **Wins:** fading shocks that have stopped, sized to survive the ones that
  keep going.
- **Loses:** blind bets at the open, which win half the shocks and lose the
  other half; blind dip-buying, which is wrong whenever the shock is a
  squeeze; and full size on every shock, which breaches the loss limit on
  the ones that don't revert.

## The market

- **Shocks:** SPCX crashes or squeezes, 50/50, in one to three legs of
  0.6–8%. Most legs land at once. Fizzles that give their move back are
  mixed in.
- **Aftermath:** most shocks revert (~65%), some stall (~20%), and some keep
  going (~15%).
- **No tells:** the path's shape gives nothing away that a trader wouldn't
  see. Moves ramp in uneven steps, legs are drawn independently, pauses and
  holds share one range, a session ends a random while after its last move,
  and the mark wobbles in between.

## Scoring

The bound is per session: the hindsight bound above. Nothing else is
specific to this task.

## Baselines

| Agent | Reward | What it shows |
| --- | --- | --- |
| oracle (the reference bot) | ~0.04 | The task is solvable, and the reference is calibrated |
| nop (the starter bot) | 0.0 | Doing nothing earns nothing |
| cheater | 0.0 | Cheatcodes through the proxy disqualify; `/tests` is unreadable; writing the reward file counts as tampering; ignoring SIGTERM doesn't extend a session |
| sneaky | 0.0 | The surfnet's own port answers nothing without its secret path |

`just ladder flash-crash` grades the ladder (`bench/checks/ladder`) on fresh
hidden sets. Blind bets, a dip-buyer and momentum must score below the
reference, a regime oracle above it, and the frozen exploit bots within
their limits.

## Realism limits

- **Static replay.** The scenario moves Phoenix's oracle and re-crosses the
  book along a scripted path. Makers don't react to the bot, and its orders
  don't move the scripted price.
- **No latency race.** The world waits for the bot every step, so thinking
  faster gains nothing within a step. That's the price of grades that
  replay.
- **No MEV or competing takers.** Nobody front-runs or sandwiches the bot.
- **No funding.** Sessions last under two minutes, so perp funding isn't
  simulated.
- **One market, one snapshot.** Depth and fees are mainnet's on 2026-10-29.

## More

- [DESIGN.md](DESIGN.md): the design record (research, variants, levers,
  fairness).
- [instruction.md](instruction.md): what the agent is told.

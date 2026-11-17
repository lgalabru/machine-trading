# solana-foundation/hunt-or-breakout-tsla

A bot trades the Phoenix TSLA perp through runs at round price levels, fading stop hunts and following real breaks.

| | |
| --- | --- |
| Market | Phoenix SPCX perp, mainnet snapshot of 2026-10-29 |
| Sessions | 72 hidden per grade; <!-- TODO: how they split by regime --> practice |
| Mandate | 12 SPCX position cap; $500 loss limit |
| Bound | <!-- TODO: what the reward divides by --> |
| Reference | <!-- TODO: its reward, and what it does in one line --> |
| Anchors | Scoring v2: 0.5 the reference; 1 the informed bot, <!-- TODO: what it knows, in one line --> |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it reads who leads a run through a round level (one hunter wallet pushing, or informed wallets building) and whether the oracle follows, then fades the hunt or follows the break before it resolves.

- **Wins:** fading a hunter push while the book is through the level; following informed flow into a break before the level jumps
- **Loses:** fading every run loses the breaks; following every run loses the hunts; waiting for the oracle enters breaks late and misses hunts that end first

## The market

<!-- TODO: the scenario family: what moves, how often, what the agent can observe. Regimes: every session: 1-3 stop hunts and 1-3 real breaks through a 2-dollar grid, mirrored -->

## Scoring

<!-- TODO: only what's specific to this task: the bound, valuation, extra void or disqualifying conditions. -->

## Baselines

<!-- TODO: the check table (oracle, nop, cheater, sneaky), the ladder, the reference's numbers on fresh sets. -->

## Realism limits

<!-- TODO: what the simulation doesn't model. -->

## More

- [DESIGN.md](DESIGN.md): the design record.
- [instruction.md](instruction.md): what the agent is told.

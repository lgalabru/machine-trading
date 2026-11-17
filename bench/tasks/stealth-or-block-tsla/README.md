# solana-foundation/stealth-or-block-tsla

A bot trades the Phoenix TSLA perp in the opening half hour, finding which trade-size class carries information today and following it into the opening jumps.

| | |
| --- | --- |
| Market | Phoenix SPCX perp, mainnet snapshot of 2026-10-29 |
| Sessions | 72 hidden per grade; <!-- TODO: how they split by regime --> practice |
| Mandate | 10 SPCX position cap; $250 loss limit |
| Bound | <!-- TODO: what the reward divides by --> |
| Reference | <!-- TODO: its reward, and what it does in one line --> |
| Anchors | Scoring v2: 0.5 the reference; 1 the informed bot, <!-- TODO: what it knows, in one line --> |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it tests the tape by trade size, finds the class whose flow the price follows, and follows only that class.

- **Wins:** it reads markouts by size class in blocks and commits to the class the price rewards
- **Loses:** it follows all flow, one fixed class, or the price itself

## The market

<!-- TODO: the scenario family: what moves, how often, what the agent can observe. Regimes: the informed trade small clips or large blocks, 50/50; a rebalancer works both classes -->

## Scoring

<!-- TODO: only what's specific to this task: the bound, valuation, extra void or disqualifying conditions. -->

## Baselines

<!-- TODO: the check table (oracle, nop, cheater, sneaky), the ladder, the reference's numbers on fresh sets. -->

## Realism limits

<!-- TODO: what the simulation doesn't model. -->

## More

- [DESIGN.md](DESIGN.md): the design record.
- [instruction.md](instruction.md): what the agent is told.

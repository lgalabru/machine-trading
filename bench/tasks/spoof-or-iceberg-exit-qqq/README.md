# solana-foundation/spoof-or-iceberg-exit-qqq

Write a bot that sells an inherited long in Phoenix QQQ perps by the close, reading the large orders on the book: walls that are pulled as the price comes and icebergs that reload where they stand, on an offline copy of Solana mainnet.

| | |
| --- | --- |
| Market | Phoenix SPCX perp, mainnet snapshot of 2026-10-29 |
| Sessions | 72 hidden per grade; <!-- TODO: how they split by regime --> practice |
| Mandate | 14 SPCX position cap; $500 loss limit |
| Bound | <!-- TODO: what the reward divides by --> |
| Reference | <!-- TODO: its reward, and what it does in one line --> |
| Anchors | Scoring v2: 0.5 the reference; 1 the informed bot, <!-- TODO: what it knows, in one line --> |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it tells a wall that will be pulled from an order that will trade, by its owner's behaviour over time, and times its sale on what each says the price is about to do.

- **Wins:** selling before a fall that a bid wall was painted to hide, or that a genuine seller's iceberg leads; holding through a rise that an ask wall or a buyer's iceberg leads
- **Loses:** trusting displayed depth; fading every wall; selling on a fixed schedule; reacting to the price after a jump

## The market

<!-- TODO: the scenario family: what moves, how often, what the agent can observe. Regimes: the jump falls or rises with equal odds; the spoofer walls against it and the iceberg works with it -->

## Scoring

<!-- TODO: only what's specific to this task: the bound, valuation, extra void or disqualifying conditions. -->

## Baselines

<!-- TODO: the check table (oracle, nop, cheater, sneaky), the ladder, the reference's numbers on fresh sets. -->

## Realism limits

<!-- TODO: what the simulation doesn't model. -->

## More

- [DESIGN.md](DESIGN.md): the design record.
- [instruction.md](instruction.md): what the agent is told.

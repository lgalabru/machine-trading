# solana-foundation/event-follow-through-tsla

Write a bot that trades the Phoenix TSLA perp through a scheduled headline, on an offline copy of Solana mainnet.

| | |
| --- | --- |
| Market | Phoenix SPCX perp, mainnet snapshot of 2026-10-29 |
| Sessions | 72 hidden per grade; <!-- TODO: how they split by regime --> practice |
| Mandate | 12 SPCX position cap; $600 loss limit |
| Bound | <!-- TODO: what the reward divides by --> |
| Reference | <!-- TODO: its reward, and what it does in one line --> |
| Anchors | Scoring v2: 0.5 the reference; 1 the informed bot, <!-- TODO: what it knows, in one line --> |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it reads which of the funds trading after a TSLA headline keeps leading the price, and positions with it before the verdict carries the jump on or takes part of it back.

- **Wins:** stay flat through the print; after the jump, mark out each wallet's takes, find the fund whose trades keep leading the oracle, and take its side before the verdict
- **Loses:** riding or fading every jump nets its costs; holding through the print is a coin flip; chasing the verdict after it prints is too late; following the market's net flow is following two mirrored funds

## The market

<!-- TODO: the scenario family: what moves, how often, what the agent can observe. Regimes: the headline jumps 1-4% up or down with equal odds; the verdict, a while later, follows through or fades by 30-80% of the jump, with equal odds -->

## Scoring

<!-- TODO: only what's specific to this task: the bound, valuation, extra void or disqualifying conditions. -->

## Baselines

<!-- TODO: the check table (oracle, nop, cheater, sneaky), the ladder, the reference's numbers on fresh sets. -->

## Realism limits

<!-- TODO: what the simulation doesn't model. -->

## More

- [DESIGN.md](DESIGN.md): the design record.
- [instruction.md](instruction.md): what the agent is told.

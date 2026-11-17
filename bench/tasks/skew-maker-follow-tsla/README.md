# solana-foundation/skew-maker-follow-tsla

Write a bot that trades the Phoenix TSLA perp in the opening half hour among three rival makers, finding which maker's lean leads the price and following it, on an offline copy of Solana mainnet.

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

A good bot profits here because it reads which rival maker's lean leads the price from how each quotes, and follows that maker by leaning its own quotes or by taking.

- **Wins:** taking or leaning ahead of the moves one rival maker's lean leads
- **Loses:** following every maker's lean, the most-leaned maker, or one maker picked blind; quoting neutral into the opening jumps

## The market

<!-- TODO: the scenario family: what moves, how often, what the agent can observe. Regimes: one of three makers leans ahead of moves, right 85% of the time; the other two lean on random views -->

## Scoring

<!-- TODO: only what's specific to this task: the bound, valuation, extra void or disqualifying conditions. -->

## Baselines

<!-- TODO: the check table (oracle, nop, cheater, sneaky), the ladder, the reference's numbers on fresh sets. -->

## Realism limits

<!-- TODO: what the simulation doesn't model. -->

## More

- [DESIGN.md](DESIGN.md): the design record.
- [instruction.md](instruction.md): what the agent is told.

# solana-foundation/toxic-day-make-or-follow-tsla

Your bot trades the TSLA perp through its opening half hour on Phoenix: make markets while the tape is noise, lean with the flow when it is informed.

| | |
| --- | --- |
| Market | Phoenix SPCX perp, mainnet snapshot of 2026-10-29 |
| Sessions | 72 hidden per grade; <!-- TODO: how they split by regime --> practice |
| Mandate | 8 SPCX position cap; $400 loss limit |
| Bound | <!-- TODO: what the reward divides by --> |
| Reference | <!-- TODO: its reward, and what it does in one line --> |
| Anchors | Scoring v2: 0.5 the reference; 1 the informed bot, <!-- TODO: what it knows, in one line --> |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it reads from the whole tape whether this morning's takers know where the price goes, and makes or follows accordingly.

- **Wins:** makes on quiet mornings and leans with the takers on toxic ones
- **Loses:** makes into informed flow, or follows noise and pays the spread

## The market

<!-- TODO: the scenario family: what moves, how often, what the agent can observe. Regimes: quiet mornings and toxic mornings, even odds, the same volume -->

## Scoring

<!-- TODO: only what's specific to this task: the bound, valuation, extra void or disqualifying conditions. -->

## Baselines

<!-- TODO: the check table (oracle, nop, cheater, sneaky), the ladder, the reference's numbers on fresh sets. -->

## Realism limits

<!-- TODO: what the simulation doesn't model. -->

## More

- [DESIGN.md](DESIGN.md): the design record.
- [instruction.md](instruction.md): what the agent is told.

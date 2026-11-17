# solana-foundation/smart-money-decay-nvda

Write a bot that trades Phoenix NVDA perps off the tape: it follows the wallets that trade ahead of moves, drops the ones whose edge dies, and ignores the ones that chase, on an offline copy of Solana mainnet.

| | |
| --- | --- |
| Market | Phoenix SPCX perp, mainnet snapshot of 2026-10-29 |
| Sessions | 72 hidden per grade; <!-- TODO: how they split by regime --> practice |
| Mandate | 30 SPCX position cap; $500 loss limit |
| Bound | <!-- TODO: what the reward divides by --> |
| Reference | <!-- TODO: its reward, and what it does in one line --> |
| Anchors | Scoring v2: 0.5 the reference; 1 the informed bot, <!-- TODO: what it knows, in one line --> |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it keeps a record of each wallet on the tape, follows the ones the price rewards, notices when a record turns, and tells flow that leads a move from flow that follows it.

- **Wins:** per-wallet markouts with a decaying record, following credible wallets net of chasers, exiting at the information horizon
- **Loses:** following the market net flow or the price move pays costs on chasers and decoys; following a wallet on its lifetime record keeps following decoys after they turn

## The market

<!-- TODO: the scenario family: what moves, how often, what the agent can observe. Regimes: every session makes 3 to 6 sharp moves up or down with equal odds; 1-3 informed, 1-3 decoy, 2-4 chaser and 3-6 noise wallets are seated in every session -->

## Scoring

<!-- TODO: only what's specific to this task: the bound, valuation, extra void or disqualifying conditions. -->

## Baselines

<!-- TODO: the check table (oracle, nop, cheater, sneaky), the ladder, the reference's numbers on fresh sets. -->

## Realism limits

<!-- TODO: what the simulation doesn't model. -->

## More

- [DESIGN.md](DESIGN.md): the design record.
- [instruction.md](instruction.md): what the agent is told.

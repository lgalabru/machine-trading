# solana-foundation/copy-shy-informed-nvda

Follow the informed fund in the NVDA perp at the open without being noticed: it hides its trading once one account keeps copying it.

| | |
| --- | --- |
| Market | Phoenix SPCX perp, mainnet snapshot of 2026-10-29 |
| Sessions | 72 hidden per grade; <!-- TODO: how they split by regime --> practice |
| Mandate | 10 SPCX position cap; $200 loss limit |
| Bound | <!-- TODO: what the reward divides by --> |
| Reference | <!-- TODO: its reward, and what it does in one line --> |
| Anchors | Scoring v2: 0.5 the reference; 1 the informed bot, <!-- TODO: what it knows, in one line --> |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it it finds the informed fund from its trades and copies it small enough that the fund keeps trading.

- **Wins:** copying the fund it identified at a footprint under its detection threshold, episode after episode
- **Loses:** copying at full size (the fund hides after one episode), following decoys or chasers, or trading the tape blind

## The market

<!-- TODO: the scenario family: what moves, how often, what the agent can observe. Regimes: opening-hour jumps (2.75 an hour) with one shy fund, 1-2 shy decoys, chasers and noise -->

## Scoring

<!-- TODO: only what's specific to this task: the bound, valuation, extra void or disqualifying conditions. -->

## Baselines

<!-- TODO: the check table (oracle, nop, cheater, sneaky), the ladder, the reference's numbers on fresh sets. -->

## Realism limits

<!-- TODO: what the simulation doesn't model. -->

## More

- [DESIGN.md](DESIGN.md): the design record.
- [instruction.md](instruction.md): what the agent is told.

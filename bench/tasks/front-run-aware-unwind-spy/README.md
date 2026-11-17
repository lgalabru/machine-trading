# solana-foundation/front-run-aware-unwind-spy

Write a bot that sells an inherited long in Phoenix SPY perps by the close without showing a pattern-reading front-runner the persistent selling it hunts, on an offline copy of Solana mainnet.

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

A good bot profits here because it infers from its own fills when a predator trades ahead of it, and paces and shapes its selling to stay under the trigger the predator reads, leaving the broker only what is cheaper to leave.

- **Wins:** selling at the largest pace the seated front-runner does not read; resting asks; leaving the rest to the broker
- **Loses:** a deadline TWAP or a randomised one that a window-reading predator still sees; dumping into the backstop; slicing so small the broker takes most of the order

## The market

<!-- TODO: the scenario family: what moves, how often, what the agent can observe. Regimes: one front-runner seated in every session, its type drawn per session from a fixed menu, dormant in about a sixth -->

## Scoring

<!-- TODO: only what's specific to this task: the bound, valuation, extra void or disqualifying conditions. -->

## Baselines

<!-- TODO: the check table (oracle, nop, cheater, sneaky), the ladder, the reference's numbers on fresh sets. -->

## Realism limits

<!-- TODO: what the simulation doesn't model. -->

## More

- [DESIGN.md](DESIGN.md): the design record.
- [instruction.md](instruction.md): what the agent is told.

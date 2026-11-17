# solana-foundation/cross-asset-lead-qqq

Write a bot that trades Phoenix QQQ perps on who is trading NVDA: it follows the wallets that trade NVDA ahead of its moves, carries their signal into the index by how much of NVDA s move QQQ makes, and ignores the ones that chase, on an offline copy of Solana mainnet.

| | |
| --- | --- |
| Market | Phoenix SPCX perp, mainnet snapshot of 2026-10-29 |
| Sessions | 72 hidden per grade; <!-- TODO: how they split by regime --> practice |
| Mandate | 10 SPCX position cap; $500 loss limit |
| Bound | <!-- TODO: what the reward divides by --> |
| Reference | <!-- TODO: its reward, and what it does in one line --> |
| Anchors | Scoring v2: 0.5 the reference; 1 the informed bot, <!-- TODO: what it knows, in one line --> |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it reads which NVDA wallets trade ahead of moves, and carries their signal into QQQ, sized by how much of NVDA s moves QQQ makes and net of QQQ s costs.

- **Wins:** per-wallet markouts on NVDA with a decaying record, an estimate of QQQ s share of NVDA s moves from the two oracles, and trading QQQ only when that share of the move clears its costs
- **Loses:** following NVDA s or QQQ s net flow, or either price s move, pays costs on chasers and decoys; trading QQQ at the full size on every NVDA signal pays costs where QQQ carries little

## The market

<!-- TODO: the scenario family: what moves, how often, what the agent can observe. Regimes: every session makes 3 to 6 sharp NVDA moves up or down with equal odds, and QQQ carries 0.35 to 0.75 of each; 1-3 informed, 1-3 decoy, 2-4 chaser and 3-6 noise wallets trade NVDA; QQQ s tape is noise and chasers -->

## Scoring

<!-- TODO: only what's specific to this task: the bound, valuation, extra void or disqualifying conditions. -->

## Baselines

<!-- TODO: the check table (oracle, nop, cheater, sneaky), the ladder, the reference's numbers on fresh sets. -->

## Realism limits

<!-- TODO: what the simulation doesn't model. -->

## More

- [DESIGN.md](DESIGN.md): the design record.
- [instruction.md](instruction.md): what the agent is told.

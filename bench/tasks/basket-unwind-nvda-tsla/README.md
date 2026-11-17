# solana-foundation/basket-unwind-nvda-tsla

Write a bot that unwinds a desk's long NVDA and TSLA perp positions on Phoenix by the close, under a cap on traded notional across both, spending the capacity where informed selling makes waiting costly.

| | |
| --- | --- |
| Market | Phoenix SPCX perp, mainnet snapshot of 2026-10-29 |
| Sessions | 72 hidden per grade; <!-- TODO: how they split by regime --> practice |
| Mandate | 40 SPCX position cap; $1000 loss limit |
| Bound | <!-- TODO: what the reward divides by --> |
| Reference | <!-- TODO: its reward, and what it does in one line --> |
| Anchors | Scoring v2: 0.5 the reference; 1 the informed bot, <!-- TODO: what it knows, in one line --> |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it reads which name has informed sellers from who is trading, and spends the shared execution capacity there first.

- **Wins:** front-loading the name informed wallets are selling, holding the name they are buying, working calm names passively
- **Loses:** pro-rata and fixed schedules spend capacity on names that can wait; selling every burst sells fake-outs and rising names

## The market

<!-- TODO: the scenario family: what moves, how often, what the agent can observe. Regimes: per market: informed sellers 35%, informed buyers 35%, calm 30%, drawn independently -->

## Scoring

<!-- TODO: only what's specific to this task: the bound, valuation, extra void or disqualifying conditions. -->

## Baselines

<!-- TODO: the check table (oracle, nop, cheater, sneaky), the ladder, the reference's numbers on fresh sets. -->

## Realism limits

<!-- TODO: what the simulation doesn't model. -->

## More

- [DESIGN.md](DESIGN.md): the design record.
- [instruction.md](instruction.md): what the agent is told.

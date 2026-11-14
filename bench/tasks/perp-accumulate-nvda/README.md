# solana-foundation/perp-accumulate-nvda

works a capped NVDA perp buy program against informed flow and a thin book, and decides how much to leave to the broker

| | |
| --- | --- |
| Market | Phoenix SPCX perp, mainnet snapshot of 2026-10-29 |
| Sessions | 72 hidden per grade; <!-- TODO: how they split by regime --> practice |
| Mandate | 45 SPCX position cap; $360 loss limit |
| Bound | <!-- TODO: what the reward divides by --> |
| Reference | <!-- TODO: its reward, and what it does in one line --> |
| Anchors | Scoring v2: 0.5 the reference; 1 the informed bot, <!-- TODO: what it knows, in one line --> |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it reads whether one-sided taker flow is informed, paces its buying to how fast the book recovers from its own takes, and leaves to the broker what is cheaper to leave.

- **Wins:** front-loading ahead of informed buying, standing aside ahead of informed selling, slicing at the pace the maker refills
- **Loses:** fixed schedules lose in one of the mirrored regimes; trading every burst loses to fake-outs; sweeping the book pays the backstop

## The market

<!-- TODO: the scenario family: what moves, how often, what the agent can observe. Regimes: calm 30%, informed up 35%, informed down 35% -->

## Scoring

<!-- TODO: only what's specific to this task: the bound, valuation, extra void or disqualifying conditions. -->

## Baselines

<!-- TODO: the check table (oracle, nop, cheater, sneaky), the ladder, the reference's numbers on fresh sets. -->

## Realism limits

<!-- TODO: what the simulation doesn't model. -->

## More

- [DESIGN.md](DESIGN.md): the design record.
- [instruction.md](instruction.md): what the agent is told.

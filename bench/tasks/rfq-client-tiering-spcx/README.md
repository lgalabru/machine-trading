# solana-foundation/rfq-client-tiering-spcx

Write a bot that runs an RFQ desk in SPCX tokens for a book of clients, a few of whom know where SPCX is going, on an offline copy of Solana mainnet.

| | |
| --- | --- |
| Market | Phoenix SPCX perp, mainnet snapshot of 2026-10-29 |
| Sessions | 72 hidden per grade; <!-- TODO: how they split by regime --> practice |
| Mandate | 0.001 SPCX position cap; $400 loss limit |
| Bound | <!-- TODO: what the reward divides by --> |
| Reference | <!-- TODO: its reward, and what it does in one line --> |
| Anchors | Scoring v2: 0.5 the reference; 1 the informed bot, <!-- TODO: what it knows, in one line --> |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it prices each client to its reach and declines the clients whose requests turn out to be informed, learning who they are from a handful of requests per client.

- **Wins:** quoting benign clients just inside their reach, flagging informed clients early from markouts and from who takes a quote no uninformed client would, and reading the flagged clients' sides as the coming move
- **Loses:** quoting everyone pays every informed fill; quoting wider loses the benign flow entirely past the reach; tiering by size or leaning against recent request flow catches little of the informed flow

## The market

<!-- TODO: the scenario family: what moves, how often, what the agent can observe. Regimes: 10 to 16 clients, 15% to 45% of them informed (which ones redrawn every session), seeing 30 to 90 slots ahead; reach 0.2 to 0.5 bp per trade size, times the square root of the size -->

## Scoring

<!-- TODO: only what's specific to this task: the bound, valuation, extra void or disqualifying conditions. -->

## Baselines

<!-- TODO: the check table (oracle, nop, cheater, sneaky), the ladder, the reference's numbers on fresh sets. -->

## Realism limits

<!-- TODO: what the simulation doesn't model. -->

## More

- [DESIGN.md](DESIGN.md): the design record.
- [instruction.md](instruction.md): what the agent is told.

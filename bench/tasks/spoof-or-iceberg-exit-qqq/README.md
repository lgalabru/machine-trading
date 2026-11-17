# solana-foundation/spoof-or-iceberg-exit-qqq

Write a bot that sells an inherited long in Phoenix QQQ perps by the close, timing the sale on what the large orders on the book turn out to be: walls pulled as the price comes, or icebergs that reload where they stand, on an offline copy of Solana mainnet.

| | |
| --- | --- |
| Market | Phoenix QQQ perp, mainnet snapshot of 2026-11-13 |
| Sessions | 72 hidden per grade, each with one jump of 0.27% to 0.53%; 6 practice |
| Mandate | sell the inherited 6 to 14 QQQ, sells only; a broker sells what is left at the close less 5 to 15 bp; a loss limit out of the jump's reach |
| Bound | none: scoring v2 alone |
| Reference | follows the walls by side, names a side's owner when a wall is pulled as the touch nears it or reloads where it stood, and sells ahead of a fall: about 0.37 of the informed bot, weighted as v2 weighs it |
| Anchors | Scoring v2: 0.5 the reference; 1 the informed bot, told the spoofer's wallet and the iceberg's, and nothing of the jump or the walls |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it tells a wall that will be pulled from an order that will trade, by its owner's behaviour over time, and times its sale on what each says the price is about to do.

- **Wins:** selling before a fall that a spoofer's bid or a seller's iceberg leads; holding through a rise that a spoofer's ask or a buyer's iceberg leads; selling late, while the bid beats the broker, when nothing is known
- **Loses:** trusting displayed depth; fading every wall; selling on a fixed schedule; reacting to the price after the jump, which lands in a slot or two

## The market

Sessions of 1,400 to 2,000 slots on QQQ's fitted price model, with one jump of 2 to 4 times QQQ's median one-minute jump, up or down with equal odds, 30% to 80% of the way through. From 30 to 90 slots before it, a spoofer keeps a wall against the jump's side and pulls it as the touch comes within 1 to 3 ticks; an iceberg works a genuine parent order on the jump's side, a slice at a time, reloading at its price after every fill. Both walls are drawn from one range of distances (8 to 36 ticks, jittered per wall) and sizes (1 to 4 QQQ, jittered per wall). Noise wallets take at random and a refill puts back the depth they use. Every role is seated in every session on fresh wallets; every order is an ordinary Phoenix transaction, signed by its wallet.

## Scoring

The account is valued at the oracle, which no order can push; the position left at the end is sold by the desk's broker at the oracle less the session's haircut. A session's PnL is measured against letting the broker sell everything, so doing nothing scores 0. Scoring v2 is the only score.

## Baselines

Gate 0 on the engine, four 36-session hidden sets (total PnL over 144 sessions):

| Rung | PnL | Per session |
| --- | --- | --- |
| nop | $0 | 0 |
| fade the walls (a bid wall means a fall) | −$33 | −0.23 |
| hold and sell late | $35 | +0.24 |
| trust the depth (a bid wall means support) | $217 | +1.50 |
| sell evenly over the session | $313 | +2.18 |
| rest an ask at the touch, sell the rest late | $393 | +2.73 |
| reference | $822 | +5.71 |
| informed bot | $2,014 | +13.99 |

The reference's weighted PnL is 0.37 of the informed bot's (by 36-session set: 0.50, 0.06, 0.62, 0.32), with a per-session Sharpe of 0.43; the order (every rung that doesn't read the walls' owners < reference < informed) holds on 0.938 of bootstrapped 72-session sets.

## Realism limits

- The jump is scripted on the fitted path; the spoofer and the iceberg see it coming and don't react to the bot.
- One spoofer and one iceberg wall a session, one each side; real books have many walls and many owners.
- The book's makers are the snapshot's, re-centred on the path, with a refill for the depth takers use.

## More

- [DESIGN.md](DESIGN.md): the design record.
- [instruction.md](instruction.md): what the agent is told.

# Design record: spoof-or-iceberg-exit-qqq

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned.

## Thesis

A good bot profits here because it tells a wall that will be pulled from an order that will trade, by its owner's behaviour over time, and times its sale on what each says the price is about to do.

- **Wins:** selling before a fall that a bid wall was painted to hide, or that a genuine seller's iceberg leads; holding through a rise that an ask wall or a buyer's iceberg leads
- **Loses:** trusting displayed depth; fading every wall; selling on a fixed schedule; reacting to the price after a jump
- **Regimes:** the jump falls or rises with equal odds; the spoofer walls against it and the iceberg works with it
- **Mandate:** QQQ only, sells only, the position between 0 and the inherited long Q (6 to 14), a loss limit of 5% of the order's notional (out of reach of the jump), 72 hidden sessions per grade.

## Expert research

<!-- TODO: why this is a real expert task, the expert's method, where non-experts go wrong, with sources. -->

## The central decision

<!-- TODO: the one decision the task is about, and the options that look defensible. -->

## Variants

A jump of 2 to 4 times QQQ's median jump (about 26 to 53 bp), up or down
with equal odds, lands in a slot or two at an onset 30% to 80% into the
session. Ahead of it, two wallets each rest one large order behind the
touch, each its own 30 to 90 slots ahead:

| Jump | Spoofer's wall | Iceberg's slice | Best response |
| --- | --- | --- | --- |
| Down | bid, pulled as the touch nears | ask, reloads where it stands | sell the rest before the jump |
| Up | ask, pulled as the touch nears | bid, reloads where it stands | hold, sell late |

| Habit | Down | Up |
| --- | --- | --- |
| Sell at once, or on a schedule | right for the share sold before the jump | wrong for the same share |
| Trust the depth (a bid holds the price up) | wrong when the first wall is the spoof | wrong when it is the iceberg |
| Fade every wall | right when the first wall is the spoof | wrong when it is the iceberg |
| React to the price | too late: the jump lands in a slot or two | too late |

## Levers

| Lever | What the market does | What a good bot does | How an honest operator detects it |
| --- | --- | --- | --- |
| Who owns a wall | the book's resting orders name their trader; the spoofer and the iceberg are fresh wallets every session | keeps each wall by owner | Phoenix's orderbook account (`getAccountInfo`), read every step |
| Pulled on approach | the spoofer cancels once the touch is within 1 to 3 ticks of its wall | names that owner the spoofer; the other wall is then the iceberg | a wall gone, unfilled, with the touch a few ticks away |
| Reloaded on a fill | the iceberg puts the same size back at the same price after any fill | names that owner the iceberg | a wall that shrank, or came back at its price at full size |
| How often the book tests a wall | the walls stand 8 to 36 ticks behind the touch; the touch moves with the price and the makers' quotes | watches through the lead; acts when a test comes | the gap between each wall and the touch, step by step |
| The broker | sells what is left at the close's index less 5 to 15 bp | sells late only while the bid, less the fee, beats it | the session's vars |

## No tells

<!-- TODO: the audit: nothing in the path's shape gives away what a trader couldn't see. -->

## Detectability

<!-- TODO: how early a good bot can tell the regime, measured on generated sets. -->

## Fairness

<!-- TODO: what the agent is told, what it can learn from practice, and that the reference stays inside the mandate. -->

## Failure modes, priced

<!-- TODO: each way a bot can lose, and what it costs. -->

## Predictions

<!-- TODO: what you expect each model tier to score, before running them. -->

## Generations

<!-- TODO: what each version learned. -->

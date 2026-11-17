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

Each step: sell the rest now, crossing the bid, or hold and leave it to a
late sale or the broker. A wall says which, but only once its owner is
known: the same bid wall means a fall (a spoofer's) or a rise (an
iceberg's). Holding by default costs nothing on a rise and the whole fall
on a fall; selling by default is the mirror. The read is worth the jump.

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
| How often the book tests a wall | the walls stand 6 to 28 ticks behind the touch; the touch moves with the price and the makers' quotes | watches through the lead; acts when a test comes | the gap between each wall and the touch, step by step |
| The broker | sells what is left at the close's index less 5 to 15 bp | sells late only while the bid, less the fee, beats it | the session's vars |

## No tells

- Both walls draw their lead (30 to 90 slots), their distance behind the
  touch (6 to 28 ticks) and their size (1 to 4 QQQ, in whole lots) from one
  range each; only the side differs, and the side is the jump's.
- Up and down are mirrored with equal odds (stratified in a set), so every
  policy that ignores who owns a wall nets about the haircut less its costs.
- Neither wall's owner trades on the other side while its wall stands
  (`harvest_mult` 0): a spoofer selling into the bids it props up would
  name itself at its first look.
- Open for the full build: the book numbers its traders as they register,
  and the actors register in a fixed order, so a trader's number may
  repeat its role across sessions (the spoofer was trader 183 in most
  Gate 0 sessions). The registration order needs drawing per session
  before practice files ship.

## Detectability

<!-- TODO: how early a good bot can tell the regime, measured on generated sets. -->

## The anchors (Gate 0 prototypes)

- **The reference (0.5)** reads Phoenix's orderbook account every step and
  keeps the walls (orders of 0.5 QQQ or more within 150 ticks of the
  touch) by owner. A wall gone unfilled with the touch within 10 ticks was
  pulled: a spoofer. A wall that shrank, or came back at its price, was
  reloaded: an iceberg. One of each stands in a session, so one named
  names the other. It sells the rest at once on a known fall signal, and
  otherwise sells late while the bid beats the broker.
- **The informed bot (1)** is told the session's roster: the spoofer's
  wallet and the iceberg's (identity, `{"roster": ["spoofer", "iceberg"]}`).
  Not the jump's direction, onset or size, nor where or when either wall
  goes up. It learns each told wallet's trader number when that wallet
  first walls (the order placed in the slot of the wallet's transaction,
  from `getSignaturesForAddress`), and acts on the first wall it can name.

## Failure modes, priced

<!-- TODO: each way a bot can lose, and what it costs. -->

## Predictions

<!-- TODO: what you expect each model tier to score, before running them. -->

## Generations

<!-- TODO: what each version learned. -->

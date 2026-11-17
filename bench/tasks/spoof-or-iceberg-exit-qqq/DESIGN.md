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

Reading displayed liquidity is a core execution skill, and spoofing is the
reason it can't be taken at face value. Regulators have prosecuted it on
exactly this pattern: orders placed to be cancelled before they trade,
on the side opposite the spoofer's real interest (the CFTC's and DOJ's
cases against Navinder Sarao, 2015, and Michael Coscia, 2015, the first
criminal conviction under the Dodd-Frank anti-spoofing provision, which
turned on order-to-trade and cancel-on-approach patterns). Icebergs are the
honest mirror: exchanges offer reserve orders so a large parent can work
without showing its size, and execution research treats their refills at a
fixed price as information about a genuine, persistent buyer or seller
(Hautsch and Huang 2012, "On the dark side of the market: identifying and
analyzing hidden order placements"; Frey and Sandas 2017 on iceberg
detection). The expert's method:

- **Judge a resting order by its owner's behaviour, not its size.** Who
  placed it, whether it stays when the price comes, whether it refills
  where it traded. A snapshot of the book can't tell the two apart.
- **Act on the read, in the right direction.** A wall that will be pulled
  says the opposite of what it shows; an iceberg says what it shows.
- **Where nothing is known, don't pay for a guess.** Sell late, while the
  bid beats the broker.

The non-expert trusts the depth (support under a bid wall), fades every
wall (all walls are fake), or sells on a schedule.

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
| Reloaded on a fill | the iceberg puts a new slice back at the same price after any fill | names that owner the iceberg | a wall that shrank, or came back at its price |
| How often the book tests a wall | the walls stand 8 to 36 ticks behind the touch; the touch moves with the price and the makers' quotes | watches through the lead; acts when a test comes | the gap between each wall and the touch, step by step |
| The broker | sells what is left at the close's index less 5 to 15 bp | sells late only while the bid, less the fee, beats it | the session's vars |

## No tells

- Both walls draw their lead (30 to 90 slots), their distance behind the
  touch (8 to 36 ticks) and their size (around 1 to 4 QQQ) from one
  range each; only the side differs, and the side is the jump's.
- Up and down are mirrored with equal odds (stratified in a set), so every
  policy that ignores who owns a wall nets about the haircut less its costs.
- Neither wall's owner trades on the other side while its wall stands
  (`harvest_mult` 0): a spoofer selling into the bids it props up would
  name itself at its first look.
- The book's resting orders carry a trader number, but Phoenix reuses
  those numbers as traders come and go (one wall showed three numbers in
  a session), so a number names no one across sessions or within one. An
  owner is read from the placing transaction's signer, or, since one owner
  walls each side, by side. Still to audit for the full build: whether
  the order the actors' wallets register in shows anywhere a bot can read.

## Detectability

The reference names a wall's owner only when the book tests the wall
before the jump: the touch reaching a spoof wall (pulled) or an iceberg's
slice (filled, reloaded). The walls' distance behind the touch sets how
often that happens. Measured with the fixed prototypes (side and slot
attribution): walls 6 to 28 ticks out gave the reference 0.71 of the
informed bot before #114; 8 to 36 ticks give 0.37 on #114's actors.

## Gate 0

Engine runs on runner-4 (`hidden/gate0/gate0.py`, untracked) on the actor
library of #114 (sizes, offsets and holds drawn per action, shuffled
onboarding): four fresh hidden sets of 36 sessions, pooled (144), every
rung a component, bootstrapped 72-session sets. Walls 8 to 36 ticks out.

| Rung | Total | Mean | v2-weighted share of informed |
| --- | --- | --- | --- |
| fade-walls | -33 | -0.23 | -0.31 |
| late | 35 | 0.24 | -0.01 |
| trust-depth | 217 | 1.50 | -0.10 |
| twap | 313 | 2.18 | -0.07 |
| passive (rests an ask at the touch, sells the rest late) | 393 | 2.73 | 0.11 |
| reference | 822 | 5.71 | 0.37 |
| informed | 2014 | 13.99 | 1.00 |

- Signal gate: informed / best non-reading (passive) = 5.13 (need 2);
  holds on 1.000 of bootstrapped sets.
- Order `max(twap, late, passive, trust-depth, fade-walls) < reference <
  informed` holds on 0.938 of bootstrapped sets (need 0.85). `passive` was
  added after the early agent check, whose bot sold passively after the
  jump; it was graded on the same four sets after the rest.
- Strength band: reference 0.37 of informed, v2-weighted (band 0.2 to
  0.7, target 0.5 or under; by set 0.50, 0.06, 0.62, 0.32); per-session
  Sharpe 0.43.
- How far one set's band strays: bootstrapping the 144 sessions, a
  72-session hidden set lands the reference at 0.18 to 0.54 of informed
  (5th to 95th percentile), outside 0.2 to 0.7 in 7% of sets, nearly all
  on the low side (a set whose falls the book seldom tests before the
  jump). 100 sessions would cut that to 4% and 144 to 2%; the verifier
  keeps the card's 72 for its time budget (the informed bot runs as a
  process), so a grade's 0.5 can sit on a reference that read fewer walls
  than usual.
- The practice set (six sessions, seed 61, three falls) is a lucky one for
  the reference: it reads two of the three falls, and makes 0.82 of the
  informed bot's PnL. Practice scores there run lower than the hidden
  set's for the same bot: matching the reference scores 0.5 on both, but
  doing nothing to the reference's 0.82 leaves little room between.
- Passed. An earlier run with walls 6 to 28 ticks out, before #114, put
  the reference at 0.71: the walls' distance sets the band.

## The anchors (Gate 0 prototypes)

- **The reference (0.5)** reads Phoenix's orderbook account every step and
  follows the walls (orders of 0.25 QQQ or more within 150 ticks of the
  touch) by side and price. A wall gone unfilled with the touch within 10
  ticks was pulled: that side's owner is the spoofer. A wall that shrank,
  or came back at its price, reloaded: that side's owner is the iceberg.
  One owner walls each side, so one named names the other. It sells the
  rest at once on a known fall signal, and otherwise sells late while the
  bid beats the broker.
- **The informed bot (1)** is told the session's roster: the spoofer's
  wallet and the iceberg's (identity, `{"roster": ["spoofer", "iceberg"]}`).
  Not the jump's direction, onset or size, nor where or when either wall
  goes up. It names a wall's owner by the slot the wall was placed in (the
  told wallet whose transaction landed in that slot, from
  `getSignaturesForAddress`), and acts on the first wall it can name.

## Failure modes, priced

From Gate 0 (per session, mean USD): trusting the depth or fading the
walls nets about the haircut less costs on average (+1.5 and −0.2) but
swings ±20 a session on which wall came first; selling on a schedule
nets +2.2 with the same swing at half the size; holding and selling late
nets +0.2. Missing a fall the reference could have read costs the jump on
the whole position: about $25 to $50.

## Predictions

Frontier agents find the late sale and the order's arithmetic quickly and
land near 0 to 0.1. Reading the order book account's resting orders is
within reach; telling pulled walls from reloaded slices, by side, before
the jump, is what lifts a bot toward the reference. Few will name owners
by the placing transaction's signer.

## Generations

- **Gate 0 prototypes.** Learned: Phoenix's resting orders carry a reused
  trader slot, not an owner, so owner tracking by that number misreads
  walls; the iceberg on main loops forever on a remainder under one lot
  (zero-lot orders), which the grader then reports as an overloaded
  machine, so its slices and parent are drawn in whole lots; noise taking
  20 bp deep fills icebergs constantly and names them at once (4 bp now).

## Departures from the card

| Card | Built | Why |
| --- | --- | --- |
| Bid iceberg: sell into it (liquidity) | the iceberg is a genuine parent trading with the jump: a bid ahead of a rise (hold), an ask ahead of a fall (sell) | mirrored against the spoofer, so trust-the-depth and fade-the-walls net about zero |
| Spoofer's side drawn per wall, `ahead_slots` | one spoofer and one iceberg wallet a session, sides set by the jump, walls up from a lead drawn per wall (`active` windows) | one jump a session; every other wall would be noise the generator can't place honestly |
| Spoof walls 4 to 10 trade sizes, iceberg 1 to 3 | one range for both, 1 to 4 QQQ in whole lots | size is no label |
| `harvest_mult` 0 to 1 | 0 | a spoofer selling from its wall's own wallet names itself at its first look |
| Haircut 10 to 25 bp | 5 to 15 bp | the snapshot makers' half-spread is about 9 bp; with the fee, selling costs about the haircut already |
| Path: QQQ calibration | plus one laid jump, 2 to 4 times the median jump, over a slot or two | the walls need a move to precede; a ramp would let a price follower catch it |
| Noise, refill, competitor | noise and refill; no grader maker | an actor-library session; selling 6 to 14 QQQ into the snapshot makers costs about the half-spread |
| Informed told the wallets and `ahead_slots` | the wallets only | the lead is a per-wall draw; the bot doesn't need it |

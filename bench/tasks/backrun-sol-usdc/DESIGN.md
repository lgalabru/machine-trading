# Design record: backrun-sol-usdc

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned. The task is task 2 of
[RFD 0005](../../../docs/rfd/0005-atomic-arbitrage-tasks.md); its interface
decisions are [ADR 0004](../../../docs/adr/0004-atomic-arbitrage-in-the-sheet.md).

## Thesis

A good bot profits here because it reacts in the trigger's slot, sizes the
cycle to the fee band through both curves, and bids only what it must to
beat the other searchers.

- **Wins:** reading the moved pool in the trigger's own slot; sizing the
  SOL→USDC→SOL cycle on both pools' exact curves (ticks crossed, fees
  included) so it closes the gap to the 8 bp band; one atomic group whose
  last limit is the least profit; a bid shaded to the rivals who can reach
  the slot.
- **Loses:** waiting a step (the gap is gone); a fixed clip (most of the
  gap left); firing on a chunk into the deep pool, or on any gap inside the
  fee band; overbidding every race; a sale without its buy back (the
  inventory limit); a priority fee where a tip is needed.
- **Regimes:** no parent order, one or several; sales and buys; the
  thinner pool mostly, the Whirlpool sometimes; rivals that reach the slot
  or are late, that tip or pay priority fees, that bid little or most.
- **Mandate:** v0.3.0 (`specs/examples/backrun-sol-usdc.json`): any Orca or
  Raydium SOL/USDC pool, SOL the numeraire with at most $5 of anything
  else after any transaction, $1,000 of fees and tips (the example's $500
  left the reference's costliest session too little room), a $200 loss
  limit; 72 hidden sessions per grade.

## Expert research

Every SOL/USDC arbitrage between concentrated pools in a week of Jito's
explorer (the top 200 by profit, 2026-11-14 to 21), the blocks around
each, a calm 48-minute window and two windows of large sell orders
(`docs/RESEARCH.md`, "SOL/USDC backruns on concentrated pools", and
`ops/research/atomic-arb/clmm-backrun/NOTES.md`):

- **One pair of pools.** All 21 arbs cycle Orca's Whirlpool `Czfq3x…` and
  Raydium's CLMM `3ucNos…`, 4 bp each. Raydium is the thinner: 100 SOL
  moves it about 6 bp, the Whirlpool 1.3 bp.
- **The trigger.** 19 of 21 hit Raydium; plain transactions (15,000
  lamports, no tip), 165 to 1,286 SOL, median 500, often round. A gap of
  0 to 3 bp before, 32 to 138 bp after (median 48). The traders sell in
  chunks: one sold 6 of about 1,000 SOL every 60 to 200 slots, another 14
  of 500 SOL (7,000 SOL) every 30 to 440 slots.
- **The arb.** 104 to 996 SOL in, 0.10 to 4.97 SOL gross. 18 of 21 closed
  the gap to the fee band and took 0.86 to 1.10 of the two-curve optimum;
  one bot traded a fixed 200 SOL (29% of it). 18 of 21 landed in the
  trigger's slot, 12 right behind it.
- **The bid.** The winner paid the leader 0% (7 cases), about 34% (6),
  50 to 80% (4) or 93% (3) of its gross, by priority fee mostly, by Jito
  tip in 5.
- **Competition.** In a burst, 20 gaps in 17 minutes, 11 different winners,
  up to 44 failing signers in a slot. The strongest competitors traded
  Raydium alone against another venue, grossed 0.88 to 1.02 SOL where the
  cycle's optimum was about 0.70, and paid 76 to 99% of it.
- **Calm flow.** Whirlpool 15 swaps a minute (median 3.4 SOL, p90 41, max
  280), Raydium 12 (1.1, 7.5, 71); the gap never cleared 8 bp (max 5.65).
- **Depth varied fivefold** over the week (Raydium's liquidity 1.9 to
  9.9e13; 8.9e13 on the snapshot), so the same chunk opens very different
  gaps.
- **A detector** (gap past the fees, each pool constant-product at its
  active liquidity, the closed-form size) flags all 21 arbs, predicts their
  gross at a median ratio of 0.998, fires 20 times in the burst windows and
  never in 1,272 calm checks.

## The central decision

What to pay the leader. A bot that reads both pools and sizes the cycle
finds the same cycle every rival that saw the trigger finds; the bundle
that bids most per compute unit takes it, and the others fail unpaid. The
options that look defensible:

- **Bid a fixed share of the expected profit** (the reference, 50%): wins
  most races it can, and overpays every race nobody else reaches.
- **Bid the least**: keeps everything it wins, loses every race to a
  tipping rival that reaches the slot.
- **Bid what beats the rivals who can reach the slot** (the informed bot):
  needs each rival's latency and bid rule, which a session's data could
  tell in principle (every rival's landed bundles and tips are on chain)
  but the agent sees only through the races it plays.

Sizing is the second decision, settled by the research: winners took the
optimum, and the `fixed-clip` rung prices a bot that doesn't.

## Variants

| Regime (drawn per session) | Best response | Habit that fails |
| --- | --- | --- |
| A large order into Raydium, rivals late or paying priority fees | the optimum, the least tip that beats the background's bundles | a fixed large share |
| The same, a tipping rival that reaches the slot | outbid it, or stay out where that costs the profit | the least tip |
| Chunks into the Whirlpool | nothing: the gap rarely clears 8 bp | a cycle on every trigger |
| Small chunks (200 to 300 SOL) | the optimum, if it clears the least profit | a clip larger than the optimum (it loses) |
| Two orders of opposite sides, or of both pools | read the pools, not the trigger: a chunk may close a gap another opened | a direction guessed from the trigger's side |
| No order at all | nothing | sending on calm gaps |

## Levers

| Lever | What the market does | What a good bot does | How an honest operator detects it |
| --- | --- | --- | --- |
| Trigger step | the chunk lands with its slot open; the step ends behind it | sends in that turn: no route latency then | `market.trigger` |
| Gap | the hit pool moves, the other doesn't | reads both pools, finds the dearer | the pools' accounts |
| Size | profit is concave in the size through both curves | the optimum on exact quotes (ticks crossed) | the kit's quotes |
| Fee band | 4 bp a pool | fires only past it, and only for a profit worth the fees | the pools' fee rates |
| Atomicity | a group lands whole or fails unpaid as a bundle | the buy's limit at the least profit | the sheet's atomic groups |
| Bid | rivals tip a share of their profit, or pay a priority fee; bundles rank by bid per requested unit | bids just over the rivals who reach the slot; a tip, not a priority fee | rivals' landed bundles and tips, on chain |
| Background bundles | ghosts tip on the busy pools, around the tip floor | a tip clear of them | `market.fees.tip_floor` |

Pool depth is not a lever the family draws (see "Generations": a depth
multiplier needs a pool override the session doesn't have yet).

## No tells

- **No predictable first move.** Each order's side is a coin, and each
  order its own pool draw; a session may hold none, or several of either
  side. A directional bet can't pay anyway: SOL is the numeraire, at most
  $5 of USDC may be held after any transaction, and holding SOL while the
  reference moves makes nothing.
- **Uneven chunks and gaps.** Round traders (60%) pick one of ten sizes and
  sometimes send a little over it; others draw every chunk evenly in a
  range of their own. Gaps are log-uniform 30 to 200 slots, each drawn on
  its own; colliding traders shift a slot.
- **No schedule to read.** An order starts anywhere from about half its
  length before the session to late in it: the session opens mid-order or
  calm, an order may outlive the session, and how much is left of one is
  never shown. The session's length is drawn on its own.
- **The reference moves on its own**, a small walk the pool searchers
  follow, so the pools drift between chunks like mainnet's.
- **Practice files** carry a neutral description (`mtx calibrate
  --informed` writes it), and no latent field.
- **What a generator-aware bot can do without reading the pools:** nothing
  the reference doesn't. A trigger is named in `market.trigger` anyway; the
  next chunk's size and slot are drawn, so knowing a round trader's size
  tells how much the next chunk opens, not when, and a bot can't act
  before a chunk lands. Sending a cycle every step at a guessed size fails
  unpaid as a bundle or pays a fee as a transaction: detection by trial,
  never an edge over reading.

## Detectability

A gap is visible the step it opens: the trigger is announced and both pools
are on chain. There is nothing to infer, only to compute. What isn't
visible is the competition: which rival reaches which slot is drawn per
send, and how each bids shows only in the races a bot plays (the winners'
bundles and tips, which `getBlock`'s transactions hold). About an eighth
of sessions hold no chunk at all.

## Fairness

- **Told:** the snapshot and both pools' accounts and layouts, the lookup
  table, the trigger steps and their latency, every range of the family,
  the searchers' rules and the ranges they are drawn from, the mandate,
  the transaction layer's ranking, and v2's scoring rules (`mtx task
  sync`).
- **Learnable from practice:** eight practice sessions from a public seed,
  each with both anchors' PnL; their reports say what each trigger opened
  and who took it.
- **The reference stays inside the mandate:** every cycle ends in SOL, its
  costliest session spent $433 of the $1,000 budget, and a failed bundle
  costs nothing, so it never draws down.
- **Anchors run as components** (`component_anchors`): amm sessions,
  atomic groups and trigger steps reach only strategy components.
- **Role noise.** Each role of a grade (reference, informed, bot) plays a
  session with keys of its own, so its compute units and so the auction's
  ranking differ by a few percent; it averages out over a set.

## Failure modes, priced

| How a bot loses | What it costs |
| --- | --- |
| Waits a step | the races it would have won in the trigger's slot, to searchers with latency 0 |
| A fixed clip | most of each gap: the `fixed-clip` rung |
| Overbids (95% of the profit) | most of the profit: the `overbidder` rung |
| Fires inside the fee band | a bundle that fails costs nothing; a transaction pays its fee |
| A sale without its buy | an inventory breach once the USDC is over $5 |
| A priority fee in place of a tip | every race a tipping bundle enters on a Jito leader |

## Predictions

Before any model run: a bot that composes the atomic cycle and sizes it on
exact quotes lands near the reference (0.4 to 0.6); one that shades its bid
well, above it; one that sizes by a fixed clip, or waits a step, well under
it; one that never composes a working group scores 0. The spread across
models should come from composition and sizing first, bidding second.

## Calibration

- **Hidden set** (a fresh 72-session secret, 2026-11-21): reference
  $6,265.92, informed bot $11,406.03; the reference is 0.549 of the
  informed bot (band 0.2 to 0.7; no losing session for either, so
  loss-weighting changes nothing), per-session Sharpe about 0.89 for both
  (at least 0.25); 21 sessions flat for the reference, 20 for the informed
  bot. Its costliest session spent $433 on fees and tips. The overbidder
  made $23.49 on the same set, two of its sessions over the $500 costs
  budget the example mandate had.
- **Practice** (seed 20261122, 8 sessions): reference $839.76, informed
  $1,626.75 (0.52).
- **Tip share.** Half the expected profit: on 24 sessions the reference
  was 0.547 of the informed bot, and 0.549 on 72. It wins 131 of the
  triggers worth a cycle where the informed bot wins 152 (24 sessions): the
  informed bot's edge is the bid, paying the background's bundles' floor
  where no tipping rival can reach the slot and just over the rival's tip
  where one can. A first version that asked for a fixed 400,000 compute
  units lost 31 races the probe (the host's default, the simulation's
  units) won: a bundle ranks per requested unit, so both anchors take the
  default.
- **Anchor hashes** (frozen before any model run, `shasum -a 256` of
  `bot.wasm` as built from this commit): recorded by `mtx task sync
  --anchors` in `tests/anchor.digest` on Linux.

## Generations

- **0.1.0** (2026-11-21): the first version. The family (orders, chunks,
  calm flow, a reference walk, cycle and pool searchers, all from the
  research), the reference (the optimum on exact quotes, tipping 50% of
  the expected profit), the informed bot (the expected-value bid against
  the session's tipping rivals who can reach the slot), and the
  `fixed-clip`, `overbidder` and `probe` rungs. The account is measured in
  SOL (`crate::amm`, a mandate whose numeraire is SOL). Not yet drawn: the
  pools' depth (the research's fivefold range), which needs the session to
  scale a pool's liquidity and its tick arrays before it starts.

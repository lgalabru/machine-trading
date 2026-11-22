# Design record: liquidation-flash

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned. The task is milestone F of
[RFD 0005](../../../docs/rfd/0005-atomic-arbitrage-tasks.md); its interface
decisions are [ADR 0004](../../../docs/adr/0004-atomic-arbitrage-in-the-sheet.md).

## Thesis

A good bot profits here because it reads the price upstream of Kamino's
oracle, sees an obligation cross its liquidation threshold before the
oracle shows it, and pushes the update, liquidates with a flash loan and
sells the collateral in one transaction, bidding only what it must to land
ahead of competing liquidators.

- **Wins:** computing every obligation's health from the upstream price
  (e-mode and borrow factors included), sending push + flash borrow +
  liquidate at the close factor + sale on the pool that pays more + repay
  as one atomic group, and shading the bid to what the race needs.
- **Loses:** reading Scope's on-chain price (a push too late), overbidding
  every race, selling more SOL than the liquidation seized (the group
  fails), holding the seized SOL (the inventory limit), or never composing
  the group at all.
- **Regimes:** SOL drifts in legs of either direction; some sessions fall
  far, some recover, some rally and liquidate nothing. Borrowers, the
  competition and the keeper's cadence are drawn per session.
- **Mandate:** v0.3.0 (`specs/examples/liquidation-flash.json`): SOL and
  USDC reserves (no borrowing), any Orca or Raydium SOL/USDC pool, flash
  borrows up to 250,000 USDC, at most $100 of anything but USDC after any
  transaction, $300 of fees and tips, a $500 loss limit; 72 hidden
  sessions per grade.

## Expert research

Nine days of every Kamino Lend transaction (1,018,575 of them, slots
468,683,679 to 472,301,409; `docs/RESEARCH.md`, "Kamino Lend
liquidations", and `ops/research/atomic-arb/kamino-liquidations/NOTES.md`):

- **What's liquidated.** 6,307 price-triggered liquidations, 5,290 on the
  main market; USDC←SOL is the top pair (2,317). Repaid per liquidation:
  p50 $17, p90 $936. Debt before: p50 $165 (USDC←SOL $76), p90 $10.3k, a
  tail to $3.8M. The main market's close factor is 10%; the bonus is the
  reserve's least (100 bp on SOL), half kept by the protocol, so a
  liquidator keeps about 50 bp of what it repays.
- **The trigger is tiny.** For 6,007 of 6,201 episodes one Scope update
  flipped health under 1: median 3.6 bp, p90 21 bp, p99 65 bp; health just
  before at p50 1.8e-4 above 1, p90 1.2e-3. Obligations drift onto the
  threshold through small ticks; liquidations cluster on drawdown days
  (2,521 on the day SOL fell 11%, under 60 on calm days).
- **The winner pushes.** Scope is pull-updated. The winner signed the
  triggering update in 39% of episodes, inside its liquidation in 31%;
  another liquidator in 60%; a crank in 1%. Lag from the update to the
  liquidation: 0 slots in 50%, 2 or fewer in 71%, p90 21; above $1k
  repaid, 93% in the same slot. A bot reading only Scope is too late for
  anything over $10.
- **How it's done.** Funding: own inventory 44%, a Kamino flash borrow 34%
  (0.001% on USDC), a Jupiter Lend flash loan 22%. Sale: Whirlpool, Jupiter,
  Raydium CLMM, Meteora; 12% keep the collateral. Median 333k compute
  units; a Jito tip in 46% of wins (p50 151k lamports); fees and tips took
  25% of the liquidators' gross bonus (28 to 44% by size).
- **Competition grows with size:** other signers within ±20 slots average
  0.56 under $10, 2.6 from $10 to $1k, 4.4 above; 30,928 attempts failed
  `ObligationHealthy`, landing after the winner.
- **A detector** (deposits at their liquidation threshold, the e-mode
  group's when there is one, against debt at its borrow factor) flags
  6,270 of 6,282 successful liquidations and 11 of 30,886 healthy attempts.
  Before the e-mode rule it flagged 62 healthy USDC←USDT attempts, which a
  real bot kept sending.
- **Profit is thin:** median $0.05 a liquidation, $36 above $10k repaid. A
  task needs borrowers five to ten times mainnet's median, or the signal
  doesn't clear the fee noise.

## The central decision

What to pay to land first. Once a bot reads the upstream and composes the
group, every liquidation it sees is one a competitor sees in the same slot,
and the bundle that ranks first takes it. The options that look defensible:

- **Bid a fixed share of the expected profit** (the reference): wins most
  races it can, overpays every race nobody else is in.
- **Bid the least**: keeps everything it wins, loses every race against a
  tipping rival that acts in the same slot.
- **Bid what beats the rivals who race** (the informed bot): needs to know
  who they are, how they bid and how often they race, which the session's
  data could tell in principle (a liquidator's bids are on chain) but the
  agent sees only through the races it plays.

Pushing is the second decision, settled by the research: a bot that waits
for Scope is a push too late, and the `scope-reader` rung prices that.

## Variants

| Regime (drawn per session) | Best response | Habit that fails |
| --- | --- | --- |
| A deep fall (several legs down) | liquidate every slice as it crosses, push first | reading Scope (late); one liquidation a step |
| A shallow dip that recovers | the few tight borrowers, early | waiting for "the crash" |
| A rally, nothing under water | do nothing | liquidating on a guess (a failed group pays nothing as a bundle, but a transaction pays its fee) |
| Tipping rivals, eager (high competition, fast) | outbid them, or stay out where it costs the profit | the least tip |
| Rivals that pay priority fees, or slow ones | the least tip | a fixed large share |
| A large borrower (the log-normal tail) | the close factor, the deeper pool | a fixed pool; selling more than seized |

## Levers

| Lever | What the market does | What a good bot does | How an honest operator detects it |
| --- | --- | --- | --- |
| Upstream lead | Scope lags the upstream by the keeper's cadence (2 to 12 slots) | values obligations at the upstream, pushes in the group | the upstream is in Scope's mappings, named by the venue metadata |
| Health | tiny ticks take obligations across 1 | the detector, e-mode and borrow factor aware | reserves' and the market's own accounts |
| Close factor | at most 10% of the debt a liquidation | repays the close factor's share, rounded down | the market account (byte 126) |
| Seizure price | the program seizes at Scope's price at the liquidation | sells what that price seizes, not more | Scope's own price vs. the upstream |
| Pool impact | Raydium is thinner | quotes both pools, sells on the better | the pools' state |
| Bid | rivals tip a share of their bonus, or pay a priority fee | bids just over the rivals who race | rivals' landed transactions and tips on chain |
| Inventory | $100 of anything but USDC, after each transaction | sells the collateral in the group, sweeps dust | the mandate |

The pool depth is not a lever the family draws: the pools are mainnet's,
and the arbitrageur moves them back each step.

## No tells

- **The first move is a coin.** A set is stratified so half its sessions
  open with a falling leg and half with a rising one; later legs fall with
  a chance drawn per session (25 to 80%), memoryless.
- **Uneven steps.** Ticks come 1 to 9 slots apart, each the leg's drift
  plus noise (log-normal, median 1.5 bp) and an occasional 20 to 65 bp
  jump; over 200 drawn sessions the tick size is p50 5.5 bp and p90 24 bp
  (mainnet's: 3.6 and 21).
- **No tail.** The path ticks to the session's last slot; its length is
  drawn (260 to 360 slots) independently of everything else.
- **Overlapping ranges.** Borrowers' health tops are drawn per session from
  1.001 to 1.04 over a fixed floor of 1.0001; rivals' bid ranges overlap
  from session to session.
- **No blind edge.** A bot can't hold more than $100 of SOL, and the family
  falls with a chance close to even, so a directional bet makes cents of
  noise. A bot that doesn't read the chain can't name an obligation.
- **Practice files** carry a neutral description (`mtx calibrate
  --informed` writes it), and no latent field.
- **What a generator-aware bot can do without reading the upstream:** send
  a group at every obligation every step and let the push decide. Failed
  bundles cost nothing, and 64 pending sends bound it, so it catches what
  the reference catches and no more; it is detection by trial, not an edge
  over reading.

## Detectability

An obligation turns liquidatable at the step the upstream crosses its
threshold, and a bot that reads the upstream sees it then: there is
nothing to infer, only to compute. What isn't visible is the competition:
which rival races which obligation is the auction's draw, and how each bids
shows only in the races a bot plays (their landed transactions and tips).
Over 72 sessions about a quarter liquidate nothing (the path never dips
past the tightest borrower).

## Fairness

- **Told:** the snapshot and its accounts, the upstream's place in Scope's
  mappings and its layout, the obligation and reserve offsets the reference
  reads, every range of the family, the mandate, the transaction layer's
  ranking, and v2's scoring rules (`mtx task sync`).
- **Learnable from practice:** eight practice sessions from a public seed,
  each with both anchors' PnL; their logs show every race's outcome.
- **The reference stays inside the mandate:** it sells the seized SOL in
  the group (selling 99.8% of the estimate) and sweeps the rest once it is
  over 0.05 SOL, so its non-USDC holdings stay under the $100 limit; its
  costs are a fraction of the $300 budget; its worst drawdown is under a
  dollar.
- **Anchors run as components** (`component_anchors`): lending sessions,
  atomic groups and pushed prices reach only strategy components.
- **Role noise.** Each role of a grade (reference, informed, bot) plays a
  session with keys of its own: obligations and accounts get other
  addresses, and transactions other compute units (the PDAs' bumps), which
  moves the auction's ranking by a few percent. The same bot's PnL on a
  session can differ by role; it averages out over a set. The liquidators
  now send in the borrowers' order, not their addresses', which removed the
  larger part of it.

## Failure modes, priced

| How a bot loses | What it costs |
| --- | --- |
| Reads Scope, not the upstream | most races, since the rivals push: the `scope-reader` rung |
| Overbids (95% of the profit) | most of the profit: the `overbidder` rung |
| Sells more SOL than seized | the whole group fails (the swap's tokens are short); as a bundle it costs nothing but the liquidation |
| Keeps the seized SOL | an inventory breach once it is over $100 (about 0.4 SOL with the fee SOL) |
| Sends plain transactions that lose | their fees, and the race |
| Borrows, or supplies over the caps | a breach |

## Predictions

Before any model run: a bot that composes the group and reads the
upstream lands near the reference (0.4 to 0.6); one that reads Scope only
scores under 0.2; one that never composes a working group scores 0, and
an agent that holds the seized SOL breaches. The spread across models
should come from composition first and bidding second.

## Calibration

- **Hidden set** (a fresh 72-session secret, 2026-11-21): reference
  $212.71, informed bot $321.72; loss-weighted, the reference is 0.661 of
  the informed bot (band 0.2 to 0.7), per-session Sharpe 0.66 (at least
  0.25), no losing session, 20 flat.
- **Practice** (seed 20261121, 8 sessions): reference $8.0, informed $15.1
  (recalibrated 2026-11-22: `mtx calibrate --informed` had written the
  files as v1, without their pools).
- **Tip share.** Over twelve drawn sessions the reference made $71 tipping
  5%, $61 at 50% and $37 at 90%, winning 63, 68 and 70 liquidations: the
  bid moves who wins less than what's kept. At 50% the reference was 0.77
  of the informed bot, outside the band; 65% (the research's top share
  scaled to the group's units) put it at 0.65 on 24 sessions and 0.66 on 72.
- **Anchor hashes** (frozen before any model run, `shasum -a 256` of
  `bot.wasm` as built from this commit): recorded by `mtx task sync
  --anchors` in `tests/anchor.digest` on Linux.

## Generations

- **0.1.0** (2026-11-21): the first version. The family, the reference
  (tips 65% of the expected profit: the research's top bid share, 0.45,
  times the group's 1.45 compute units over a liquidator's), the informed
  bot (the expected-value bid against the session's rivals), the
  `scope-reader` and `overbidder` rungs.

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
group, every liquidation it sees is one a competitor sees, mostly in the
same slot (the routes add no latency, and a rival's lag is the research's:
above $1,000 repaid, 90 to 96% act in the slot the obligation turned), and
the send that pays the leader most per compute unit takes it, a tip or a
priority fee alike. The options that look defensible:

- **Bid a fixed share of the expected profit** (the reference): wins most
  races it can, overpays every race nobody else is in.
- **Bid the least**: keeps everything it wins, loses every race against a
  rival that acts in the same slot (the `min-tip` rung).
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
| Eager rivals (high competition, fast), or any large liquidation | outbid them per unit, or stay out where it costs the profit | the least tip |
| Slow rivals, few, or a small liquidation nobody races | the least tip | a fixed large share |
| A large borrower (the log-normal tail) | the close factor, the deeper pool | a fixed pool; selling more than seized |

## Levers

| Lever | What the market does | What a good bot does | How an honest operator detects it |
| --- | --- | --- | --- |
| Upstream lead | Scope lags the upstream by the keeper's cadence (2 to 12 slots) | values obligations at the upstream, pushes in the group | the upstream is in Scope's mappings, named by the venue metadata |
| Health | tiny ticks take obligations across 1 | the detector, e-mode and borrow factor aware | reserves' and the market's own accounts |
| Close factor | at most 10% of the debt a liquidation | repays the close factor's share, rounded down | the market account (byte 126) |
| Seizure price | the program seizes at Scope's price at the liquidation | sells what that price seizes, not more | Scope's own price vs. the upstream |
| Pool impact | Raydium is thinner | quotes both pools, sells on the better | the pools' state |
| Bid | rivals pay a share of their bonus per compute unit as mainnet's did, a tip or a priority fee; the slot's sends rank by pay per unit | bids just over the rivals who race, per unit of its own group | rivals' landed transactions, fees and tips on chain |
| Push | the group may push Scope itself | always pushes: the seizure is then at a price it knows, and it sells all of it in the group | the program's logs |
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
- **The reference stays inside the mandate:** its group pushes Scope, so
  the program seizes at a price it knows, and it sells 99.9% of the seized
  SOL in the group, sweeping the rest once it is over 0.05 SOL, so its
  non-USDC holdings stay under the $100 limit after every transaction; its
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
| Bids the least, always | every race a rival reaches: the `min-tip` rung |
| Sells more SOL than seized | the whole group fails (the swap's tokens are short); as a bundle it costs nothing but the liquidation |
| Keeps the seized SOL, even until its next transaction | an inventory breach once it is over $100 (about 0.4 SOL with the fee SOL): rule 11 is judged after each transaction, from its own balances |
| Sends plain transactions that lose | their fees, and the race |
| Borrows, or supplies over the caps | a breach |

## Predictions

Before any model run: a bot that composes the group and reads the
upstream lands near the reference (0.4 to 0.6); one that reads Scope only
scores under 0.2; one that never composes a working group scores 0, and
an agent that holds the seized SOL breaches. The spread across models
should come from composition first and bidding second.

## Calibration

- **Three hidden sets** (2026-11-23, after the rival range was centred
  and `scope-reader` tips the reference's share; three fresh 72-session
  secrets, each rung graded on each, `ladder.py --sets 3`; bootstrap odds
  per set, floor 0.85):

  | Draw | reference | informed | ratio (band 0.2–0.7) | Sharpe | `scope-reader` | `overbidder` | `min-tip` | `min-tip < reference` odds |
  | --- | --- | --- | --- | --- | --- | --- | --- | --- |
  | 1 | $146.05 | $279.23 | 0.52 | 0.63 | $60.6 | $11.3 | $134.2 | **0.75** |
  | 2 | $146.09 | $271.38 | 0.54 | 0.68 | $48.7 | $10.2 | $123.3 | 0.89 |
  | 3 | $270.48 | $341.13 | **0.79** | 0.50 | $88.6 | $12.8 | $122.5 | 1.00 |

  No losing session for either anchor; `max(scope-reader, overbidder) <
  reference` and `reference < informed` hold at odds 1.00 (0.998 on draw
  3) and no rung breached. Draw 1 fails `min-tip < reference`'s floor and
  draw 3 the band's top: the task doesn't meet the three-draw bar at 72
  sessions ("Statistical power" below). A fourth draw graded on a Mac the
  same day: reference $207, informed 1.72 of it (0.58), `min-tip` 0.77,
  odds 0.92.
- **Statistical power.** `min-tip` and the reference make the same money
  where nobody races them in the trigger's slot (`min-tip` more: it keeps
  the 36% the reference tips), so the rule is decided by the contested
  races the reference wins against rivals bidding under 36%: per session,
  the reference's lead is $0.85 on average and $3.80 of spread (216
  sessions of the three draws), z = 0.22 a session. On a fresh set of `n`
  sessions its bootstrap odds clear 0.85 with chance Φ(0.22√n − 1.04):
  0.80 at 72 (all three draws 0.52), 0.90 at 108 (0.73), 0.95 at 144
  (0.85), 0.99 at 216 (0.96). The band moves with the same sessions: the
  reference's ratio over 2,000 resampled sets is 0.63 at the median, and
  misses the band in 22% of sets of 72, 13% of 144 and 8% of 216. Both
  are power, not skill: the order and the ratio hold on the pooled 216
  (`min-tip` 0.68 of the reference, the reference 0.63 of the informed
  bot). Session by session on draw 1: `min-tip` beat the reference in 32
  sessions, by $28.4 in all, each by winning the same liquidations
  untipped (hidden-6: 9 wins against 10, $0 of tips against $3.60;
  hidden-54: 17 against 21, $0 against $7.10); the reference beat it in
  17, by $40.2, where the rivals bid under 0.3 at a competition of 1 or
  more (hidden-4: 7 wins against 1, $11.50 against $1.50; hidden-12: 10
  against 2). No session decides it: it is many small uncontested wins
  against fewer contested ones. Nor is the market short of contest: the
  rivals' winning liquidations landed in the slot the obligation turned
  in 76% of cases (100% above $1,000 repaid), against the research's 50%
  (93%), so more in-slot rivals would leave the research. What would
  settle the rule and the band: 144 to 216 hidden sessions (two to three
  times a verifier run's time), not a looser rule or band.
- **Rival bids, centred.** On the first family (a session's range's low
  end 0.15 to 0.5, its width 0.05 to 0.3: an average rival bidding 0.41,
  ranges up to 0.8), `min-tip` made 1.09 of the reference on one fresh
  set and 0.87 (odds 0.81) on the next; over 216 sessions its lead was
  $0.34 a session ($4.23 spread), so a set of 72 passed one time in three.
  Split by the session's rival range: where it sat above the reference's
  36%, `min-tip` beat the reference (it wins every race nobody else joins
  and keeps all of it), where it sat under it, the reference made
  3.5 times what it did. The research's shares run 0.25 to 0.45 (0.28 to 0.44 by
  size, the reference's 36% their middle), so the family now centres a
  session's range on it: its low end 0.15 to 0.45 and its width 0.05 to
  0.2, an average rival at 0.36 and no range above 0.65. A narrower range
  tried in 0.2.0 put the reference near the band's top on 24 public
  sessions (0.69), and this one does on one draw of three: the rule and
  the band pull the same lever opposite ways.
- **The `scope-reader` rung** tipped 0.65, the first reference's share,
  after the reference moved to 0.36; it now takes the reference's
  constant (`liquidator::TIP_SHARE`). Before: $53.65, $46.61 and $63.38 on
  the three draws.
- **Practice** (seed 20261121, 8 sessions, after the change): reference
  $11.63, informed $30.67 (0.38).
- **Earlier hidden sets** (2026-11-22, the first rival range): reference
  $227.16, informed $385.17 (0.59), `scope-reader` $77.41, `overbidder`
  $57.64, `min-tip` $158.53 (odds 0.90); and on 2026-11-23, two fresh
  sets: reference $161 and $148, `min-tip` 1.09 and 0.87 of it (odds
  0.81 on the second), informed 2.72 and 1.99.
- **The bid, before and after.** Under the old builder (every bundle above
  every priority fee, routes of 0 to 2 slots), the slot a send reached
  settled most races: over twelve sessions the reference won 63, 68 and 70
  liquidations tipping 5, 50 and 90% of its expected profit, and its 65%
  was chosen to put it inside the band. With one scale and no route
  latency, 24 public sessions read: rivals took 162 liquidations to the
  reference's 98, a bot that always tips the least made 0.77 of the
  reference and the informed bot 1.12. Three findings, each a fix rather
  than a fit: a tip transaction asked for the runtime's default 200,000
  compute units, a third of every tipped group's units; liquidators paid
  their share of the bonus on transactions a third smaller than mainnet's
  (now per unit, on the research's median 333k); and the reference's
  per-transaction inventory breached (now it always pushes, and sells
  99.9%). After them, on the same 24 sessions: `min-tip` 0.37, informed
  1.45; with the family's own rival range (0.15 to 0.8) restored from a
  narrower one tried here, `min-tip` 0.62 and informed 2.17.
- **Tip share.** 36%, the middle of mainnet's liquidators' median cost
  share by size (28 to 44% of the bonus kept, NOTES.md §3): a principle,
  not tuned to the band.
- **Anchor hashes** (frozen before any model run, `shasum -a 256` of
  `bot.wasm` as built from this commit): recorded by `mtx task sync
  --anchors` in `tests/anchor.digest` on Linux.

## Generations

- **0.1.0** (2026-11-21): the first version. The family, the reference
  (tips 65% of the expected profit: the research's top bid share, 0.45,
  times the group's 1.45 compute units over a liquidator's), the informed
  bot (the expected-value bid against the session's rivals), the
  `scope-reader` and `overbidder` rungs.
- **0.2.0** (2026-11-22): the bid matters. The transaction layer ranks a
  slot's sends on one scale of pay per requested unit, a tip and a
  priority fee alike, after the slot they reached
  (`docs/tx-layer.md`, "Ranking"); the routes add no latency here, so a
  rival that acts in the slot it saw the obligation turn races the bot for
  it; above $1,000 repaid the research's 4.4 rivals are a floor, and a
  session has 2 to 5 liquidators; a liquidator pays its share per unit as
  mainnet's did. Rule 11 is judged after each transaction from its own
  balances, so the reference always pushes and sells 99.9% of what it
  seizes. The reference tips 36% (the research's middle share), not 65%;
  the informed bot counts every rival, a priority fee as much as a tip;
  a `min-tip` rung (always the least tip) and its rule
  (`min-tip < reference`).
  Then, the same version (2026-11-23): a session's rival range is
  centred on the research's middle share (its low end 0.15 to 0.45, its
  width 0.05 to 0.2), as `min-tip < reference` passed about one fresh set
  in three ("Calibration", "Rival bids, centred"); `scope-reader` tips the
  reference's share, from the reference's own constant.

# RFD 0005: Atomic arbitrage tasks

State: adopted (ADR 0004) for its interface decisions; the tasks are in progress
Authors: astra, for Ludo Galabru  ·  Started: 2026-11-21

## Problem

Every graded task is a perp on Phoenix (`bench/tasks/`). The largest
recurring profit on Solana is atomic arbitrage: one transaction that starts
and ends in SOL and crosses two or more pools, sometimes funded by a flash
loan, sometimes around a liquidation. In the week of 2026-11-14 the 200
most profitable SOL arbitrages made 104 SOL. Three quarters of that went
through Orca Whirlpool and Raydium CLMM, which we can't trade at all
([research](../RESEARCH.md#atomic-sol-arbitrage-on-mainnet-from-jitos-explorer-2026-11-21)).

Atomic arbitrage tests different skills from our perp tasks:

- finding a route through a graph of pools;
- sizing it against curves that move as you trade;
- composing several programs in one transaction;
- deciding what to pay to land first.

The question this RFD asks is which tasks to build first, and what each
layer of the repository needs for them: scenario format, venues,
instruments, mandates, actors, the transaction layer and steps.

## Sketch

### Three tasks, in this order

The studies behind each are in [`RESEARCH.md`](../RESEARCH.md).

1. **`liquidation-flash`.** The bot liquidates unhealthy Kamino
   obligations. It funds the repay leg with a Kamino flash borrow and sells
   the seized collateral on a pool, all in one transaction.
   - Thesis: a good bot profits because it reads the price upstream of
     Kamino's oracle, sees an obligation cross its threshold before the
     oracle shows it, and pushes the update and liquidates in one
     transaction.
   - What mainnet shows:
     - Half of the liquidations land in the slot of the oracle update
       that triggered them, and 93% of those above $1k do.
     - The winner pushed the update itself in 39% of cases.
     - The liquidator keeps about 50 bp of what it repays: Kamino's 100 bp
       bonus, half of it kept by the protocol.
   - It builds on lending sessions (`crates/grader/src/lending.rs`). They
     already have synthetic borrowers, competing `Liquidators`, Scope
     feeds the scenario moves, and the `liquidate` action
     (`klend::liquidate`).
   - Done: `klend::flash_borrow` and `klend::flash_repay`.
   - Missing:
     - an upstream feed that leads Scope, and a way for the bot to push it;
     - a snapshot of the main market with a SOL/USDC pool;
     - borrowers 5 to 10 times mainnet's median size, so a liquidation
       pays more than the fee noise;
     - the transaction layer on lending sessions.
2. **`backrun-sol-usdc`.** A seller works a large order in chunks of 200
   to 1,000 SOL into Raydium CLMM `3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv`,
   the thinner pool. Each chunk opens a gap of 30 to 140 bp to Whirlpool
   `Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE`. The bot closes it with a
   SOL→USDC→SOL cycle.
   - Thesis: a good bot profits because it reacts in the trigger's slot,
     sizes the cycle to the 8 bp fee band through both curves, and bids
     only what it must to beat the other searchers.
   - What mainnet shows: 18 of 21 arbs landed in the trigger's slot.
     Winners took 86 to 110% of the optimum our model computes, and paid
     between 0 and 93% of it to the leader.
   - Done: Whirlpool and Raydium CLMM in the kit, with exact quotes
     (`protocols::orca::whirlpool`, `protocols::raydium::clmm`).
3. **`backrun-thin-pools`**, which replaces the `triangular-lst` we
   planned first. We expected stake-pool rate steps at the epoch boundary
   to open LST arbs. They don't: the step is 1.4 bp, and the pools ignore
   it. The profitable cycles of two and three tokens are backruns of large
   swaps on thin pools, with an LST or a mid-cap token as the bridge.
   - Thesis: a good bot profits because it searches a graph of pools for
     the cycle that the latest swap opened, and skips cycles whose fees eat
     the gap.
   - The epoch step stays in the scenario as a distractor. Stake-pool
     deposits and withdrawals bound the LST pools between −12 bp and +1 bp
     of their value.

Two further tasks wait for the first three:

- `prop-amm-lag`, against a mock oracle-quoted AMM. The real prop AMMs are
  closed source, and their arbitrage is a drizzle of tiny profits.
- `top-of-block`, a priority-fee race on a gap that opens between slots.
  The research found none of these among the top SOL/USDC arbs, so this
  one is the least urgent.

### What each layer needs

**Scenario format** (`docs/scenario-format.md`, `scenario_v2.rs`).

- `venues` holds exactly one kind today. Arbitrage needs several in one
  session: an `amm` section beside `lending`, then `amm` on its own.
- A spot session has no path: "Nothing moves the market here but the bot".
  An `amm` section needs a market model of flows: large swaps, with their
  arrival rate, size and direction drawn from the research distributions,
  and a reference price the pools drift around.
- The transaction layer is refused outside perp sessions
  (`docs/tx-layer.md`, "Session kinds"). Arbitrage without an ordering
  contest is a solved problem, so the layer must accept lending and `amm`
  sessions.
- Raw sends are refused in multi-market sessions. An atomic route is a
  multi-market send by construction.
- A new generator family, `atomic-arb`, in `GENERATORS`. Its parameters:
  - what opens the gap: a large swap, a price move between slots, an LST
    rate step or an oracle move that makes an obligation unhealthy;
  - the gap's size, pool depths and fee tiers;
  - how many searchers compete, their latency and how they bid.

**Venues and the kit.** `crates/runtime/src/venue.rs` lists `amm` as
"none yet".

- Each pool type needs, in `mtx-kit`:
  - state decoding: the pool account, and its tick arrays or bins;
  - an exact quote that crosses ticks or bins;
  - a swap instruction builder;
  - fills read from the pool's vaults.
- In the runtime, each pool type needs a `Venue` whose `Seat` executes
  `swap`.
- The order follows the research: Whirlpool and Raydium CLMM first (about
  75% of the week's profit), then Raydium AMM v4, then the two Meteora
  pool types.
- Kamino needs `flash_borrow_reserve_liquidity` and
  `flash_repay_reserve_liquidity` in `klend.rs`.
- Every route-building path must read and write version 1 transactions.

**Atomic groups in the sheet.** An atomic route is several actions that
must land together or not at all. A strategy can say that two ways today,
and neither fits:

- `tx-options.bundle` sends lines as one Jito bundle, but each line is
  still its own transaction.
- Raw sends (`grader/src/raw.rs`) can compose anything, but they are
  refused outside one-market perp sessions. Their audit also reads
  Phoenix's views of the account.

We propose that `mtx:strategy@0.3.0` adds two things:

- **`tx-options.atomic: option<u32>`.** Lines with the same id compile, in
  sheet order, into one transaction. Each line's venue `Seat` returns
  instructions instead of sending them. The host adds the compute budget,
  the tip and the lookup tables, and sends one version 0 transaction. The
  transaction fails as a whole if any line's limit isn't met, so the last
  swap's `limit` is the route's minimum profit.
- **A `flash(amount)` action on a lending venue.** Inside an atomic group,
  it becomes `flash_borrow` before the group's first line and
  `flash_repay` after its last (`klend::flash_borrow`,
  `klend::flash_repay`).

A Whirlpool-to-Raydium cycle is then two `swap` lines with one atomic id.
A flash-funded liquidation is `flash`, `liquidate`, then a `swap` that
sells the collateral. Every line remains an action the grader knows, so
the mandate's audit and the fill records work as they do for single
lines, and raw sends stay what they are for: actions the sheet can't say.

**Snapshots.** `docs/snapshot-recipes.md` leaves pool creation "for the AMM
venue modules". Our recommendation is to clone the mainnet pools together
with their tick arrays and vaults, rather than create pools. A recipe
action, `clone-pool`, takes a pool address and pulls in every account a
swap through it reads.

**Moving the pools.** A swap actor that sends real swaps through the
pools keeps sqrt price, liquidity, ticks and bins consistent. An account
override can't easily do that on a concentrated pool, because its tick
arrays must agree with its price. Override templates are only needed for
the starting state:

- a Whirlpool or Raydium CLMM price and liquidity at a given tick;
- an AMM v4 or CPMM pool's reserves;
- a stake pool's `total_lamports` and `pool_token_supply`.

**Actors.**

- `whale`: swaps through one pool on the generator's schedule, from a
  shared pool of accounts, sized from the research distributions.
- `searcher`: sees the same gap the bot sees, after a drawn latency, and
  bids in tip or priority fee by a drawn rule.

Both act inside the layer's blocks, which is the pool-aware actor work
the code calls "PR 17b" (`scenario_v2.rs`).

**Steps.** A step is two slots, and nothing moves while the bot thinks.
On mainnet, a backrun or a liquidation lands in the same slot as its
trigger, often right behind it.

We propose a **trigger step**, a step that ends right after the trigger
lands:

1. The grader lands the trigger: a whale's chunk, or an oracle update.
2. It pauses the slot, and the bot and the searchers see the same state.
3. Their sends then compete for the rest of the same slot, ordered by the
   transaction layer.

Outside triggers, the session keeps its two-slot steps. This is narrower
than [RFD 0004](0004-strategies-on-mainnet.md)'s event steps, and enough
for these tasks.

**Instruments and mandates.** These are drafted in the specs:
[`instruments/amm.md`](../../specs/instruments/amm.md#arbitrage) and
[mandate v0.3.0, proposed](../../specs/mandate.md#proposed-v030-atomic-routes).

- An arbitrage route touches long-tail tokens that a per-instrument
  universe can't list in advance.
- An arbitrage must end flat. A flash loan must never show up as a
  position.
- The fees and tips the bot pays need a budget of their own.

### Grading

Grading stays scoring v2: 0 is doing nothing, 0.5 the reference, 1 the
informed bot.

- The **reference** solves the best cycle size for the gap it sees and bids
  a fixed share of the expected profit.
- The **informed bot** also knows the searchers' latencies and bidding
  rules, and the whales' schedule up to the present. It knows nothing of
  draws still to come.
- Value is SOL plus every other asset at the scenario's reference price,
  read from the chain's end state.
- The inventory rule in mandate v0.3.0 keeps a "cycle" that is really a
  directional bet from paying.

## Open questions

- **Is the transaction layer's model of the ordering contest good enough?**
  It ranks bundles by tip plus priority fee per CU, with ghost contention.
  The research shows the top arbs mostly win on priority fees, not bundles.
  We find out by replaying the research's contested blocks through
  `grader/src/txlayer`, and checking who it puts first. Answered
  2026-11-22 (What we learned): it wasn't; the builder now ranks by arrival
  slot, then on one scale of pay per unit.
- **One-slot steps or event steps?** We find out by building
  `backrun-sol-usdc` on one-slot steps. If the reference can't catch a
  backrun that a searcher at mainnet latency would, the steps are too
  coarse.
- **Should the snapshot carry the competitors' on-chain programs?** Real
  searchers call their own programs. Our searchers can send plain swap
  routes. We find out by checking whether routes of plain instructions
  fit the CU limits the research measured: a median of 170k CU and a p90
  of 334k.
- **A liquidation as a fill.** What does a liquidation count as under the
  mandate: a buy of the collateral, paid in the debt asset? We answer this
  in the money-market profile before `liquidation-flash` ships.

- **Which oracle model for `liquidation-flash`?** On mainnet, winners read
  Scope's upstream sources, then push Scope and liquidate together. On a
  snapshot, a bot's `RefreshPriceList` would rewrite the scenario's price
  from stale source accounts. There are two options:
  - Override the source accounts each slot, a Pyth feed leading Scope, and
    let bots push. This needs a source template.
  - Write Scope every slot and refuse the refresh. Then the task measures
    reaction, not the push.

  We find out by building the first and checking that Scope's
  `RefreshPriceList` reads it on a snapshot.

## Risks

- **The venue code is large.** Concentrated-liquidity quoting has to be
  exact, or the reference loses on rounding. We mitigate by checking every
  quote against `simulateTransaction` on the snapshot in tests.
- **Determinism.** Searcher actors and whale swaps add randomness inside
  blocks. It must all come from the scenario's seed, or byte-identical
  replay breaks.
- **Saturation.** If searchers are too slow, every bot catches every gap and
  the reward measures nothing. The gates' strength band
  (`docs/development.md`, "The new-task checklist") is the check.

## What we learned

- 2026-11-21: three studies, in [`RESEARCH.md`](../RESEARCH.md):
  - Kamino liquidations are triggered by oracle ticks of a few basis
    points, and are won in the same slot.
  - SOL/USDC backruns follow a chunked seller on the thinner pool.
  - LST cycles are backruns too, not epoch trades, so task 3 changed.
- 2026-11-21: the owner accepted atomic groups, trigger steps, an oracle
  bots can push, and mandate v0.3.0's shape: [ADR 0004](../adr/0004-atomic-arbitrage-in-the-sheet.md).
  Scope's `FixedPrice` type, with the price in the mapping's `generic`
  data, is the upstream feed (Scope `a09283d`,
  `programs/scope/src/oracles/mod.rs`).
- 2026-11-21: atomic groups in the sheet, `mtx:strategy@0.3.0`
  ([RFD 0002](0002-wasm-strategies.md)'s 2026-11-21 entry): a group is
  planned whole and refused whole if a cap would cut any line of it;
  its lines must be next to each other on the sheet. On a mainnet fork, a
  10 SOL flash loan through Orca's and Raydium's SOL/USDC pools lost
  about 8 bps with the fee, and a limit at the repayment made Raydium
  refuse the swap and the whole transaction. Without a lookup table of
  the pools' accounts, that route and a price push don't fit in one
  transaction (1,338 of 1,232 bytes): a route task's snapshot needs a
  lookup table of its pools' accounts, which their seats compile with.
- 2026-11-21: `liquidation-flash`'s session works end to end
  (`bench/checks/kamino/liquidation-flash.json`): lending sessions take the
  transaction layer, Scope's SOL entries are pushed rather than written,
  four liquidators and Scope's keeper bid in the layer's auction under
  their own names, two pools sit beside the lending market with a session
  lookup table, and mandate v0.3.0's rules 11 to 13 are judged on the bot's
  transactions ([scenario format](../scenario-format.md#lending-with-pools-and-a-pushed-oracle),
  [actors](../actors.md#a-lending-sessions-actors)). A push, a flash
  borrow, a liquidation, a swap and the repay land as one transaction.
  Without anything moving them, the pools would stay at the opening price
  while SOL fell, and selling seized SOL into them would pay the fall, not
  the bonus: a pools arbitrageur now swaps them back to the scenario's
  price each step. Liquidators' tips win at the top of a Jito leader's
  block whatever a priority fee bids, so a bot that pays by priority fee
  loses races it would win by tip, and pays for the losses.
- 2026-11-21: `liquidation-flash` is a task (`bench/tasks/liquidation-flash`),
  drawn by the `liquidation-flash` generator. Its liquidators race by a
  session's own rules (`liquidators.race`), which the informed bot is told.
  The bid moves who wins less than what's kept: over twelve sessions the
  reference won 63, 68 and 70 liquidations tipping 5, 50 and 90% of its
  expected profit, because the race is mostly settled by which slot each
  send reaches. Each role of a grade plays with keys of its own, so the
  liquidators now send in the borrowers' order rather than their addresses'.
- 2026-11-21: `backrun-sol-usdc`'s session works end to end
  (`bench/checks/amm/backrun-sol-usdc.json`, `just backrun-smoke`): an
  `amm` session kind, `venues.amm` on its own, with a wallet valued at the
  scenario's reference price, the transaction layer and a lookup table of
  both pools ([scenario format](../scenario-format.md#an-amm-session-triggers-and-searchers));
  trigger steps ([tx layer](../tx-layer.md#trigger-steps)); and a whale,
  calm flow, cycle searchers and a pool searcher in the auction
  ([actors](../actors.md#an-amm-sessions-actors)). A step already ends with
  its last slot open: what the session lands there, and what the turns
  send with no latency, lands in that slot behind it. So a trigger step is
  a step that ends on the trigger's slot, lands the trigger, and zeroes
  route latency for the turns; the next step's first block is the rest of
  the slot, and `getTransaction` puts the backrun in the trigger's slot.
  On the snapshot, the whale's chunks of 500 to 1,000 SOL into Raydium
  opened gaps of 30 to 64 bp, and the best cycle would have made 0.33 to
  1.93 SOL, the research's size table at today's Raydium liquidity. A probe
  that tips half its expected profit won 5 of 7 backruns in the trigger's
  slot and lost 2 to a searcher tipping 60% at no latency, for $140; doing
  nothing made $0. A Jito leader's bundles go at the top of its block, so
  a tipped cycle beats every priority-fee bid, however high: the pool
  searcher, which bids 85% of its gross as a priority fee, won only where
  no bundle landed. Whether that ordering is mainnet's is the RFD's first
  open question.

- 2026-11-21: `backrun-sol-usdc` is a task (`bench/tasks/backrun-sol-usdc`),
  drawn by the `backrun-sol-usdc` generator (`crates/grader/src/backrun.rs`):
  none to three parent orders of 2,000 to 7,000 SOL a session, each a side
  and a pool of its own, in round or uneven chunks of 200 to 1,000 SOL 30
  to 200 slots apart, starting before the session or late in it; 0 to 5
  cycle searchers and 1 or 2 pool searchers whose latency, size and bid
  the informed bot is told. An amm session whose mandate's numeraire is
  SOL is now measured in SOL, so the reference's walk moves no PnL. On 72
  hidden sessions the reference (the optimum, tipping half its expected
  profit) made 0.55 of the informed bot. A group that asks for a fixed
  400,000 compute units lost 31 races in 24 sessions that the host's
  default (the simulation's units) won: a bundle ranks per requested unit.
  Not drawn yet: the pools' depth, which varied fivefold over the
  research's week and needs a pool's liquidity and tick arrays scaled
  before the session starts.

- 2026-11-22: `backrun-thin-pools` is a task (`bench/tasks/backrun-thin-pools`),
  drawn by the `backrun-thin-pools` generator (`crates/grader/src/thin_pools.rs`)
  on the ten pools of `snapshots/thin-pools-2026-11-21`. An amm session
  now trades any pools whose first is SOL/USDC: every other token has a
  fixed reference price in SOL (`venues.amm.reference_sol`), which values
  the account and rule 11 and which the pool searchers trade back to; up to
  twelve pools (`POOLS_MAX`), and as many lookup tables as the accounts
  need. The snapshot's 279 accounts counted pool by pool are 229 once each,
  so one table holds them: a three-leg cycle is 619 bytes, a four-swap
  route 709. Cycle searchers search the graph (`amm::cycle::Graph`: every
  simple cycle through SOL, gated by its marginal rate after fees, sized by
  golden-section search on exact quotes), to `max_legs` legs; on two
  SOL/USDC pools that is the old two-pool cycle, and `backrun-sol-usdc`'s
  and `liquidation-flash`'s reports are unchanged but for their bots'
  WebAssembly fuel counts. A trigger may be sized by its move
  (`impact_bps`), halved until it fills where a pool's liquidity ends. The
  graph holds no four-leg cycle that visits a token once (its only links
  beside SOL are JitoSOL–JTO and GEOD–USDC), so the informed bot's deeper
  search adds nothing here; its edge is the bid. The research's p90
  impact (5,877 bp) and a 1,500-SOL cap opened cycles of up to 200 SOL on
  these pools, ten times the research's largest backruns, so moves are cut
  at 3,000 bp and triggers at 100 SOL's worth. On 72 hidden sessions the
  reference (two and three legs, tipping half) made 0.57 of the informed
  bot; one that looks only at a pair's two pools made 0.20 of the reference (v2 0.10), and one that tips 95% 0.10. The
  epoch step is left out: no session has a stake-pool venue, and stepping
  the rate offline needs a stake-pool override nobody writes yet.

- 2026-11-22: `backrun-thin-pools` paid blind cheap bidding. Its graph
  holds no four-leg cycle, so the informed bot's only edge is the bid, and
  a ladder rung that searches as the reference does but always tips the
  least (`min-tip`, 1,000 lamports) beat the reference: v2 0.53 on 72
  hidden sessions, 0.91 on practice. Most triggers had no tipping rival in
  their slot (a searcher reached it about one send in three, half of them
  tipped). The market changed, not the band: 88% of sessions now hold a
  cycle searcher that tips, searches three legs and reaches every
  trigger's slot (the research's re-arbs within 2 slots, 57 of 82), its
  share drawn as any searcher's; in the other 12% no searcher's latency
  starts at 0. Which is which isn't told. `min-tip` fell to 0.007 hidden
  (0.27 practice), the reference sits at 0.57 of the informed bot, and the
  ladder holds `min-tip < reference`. The other tokens' reference prices
  in SOL, which the pool searchers pull pools to, were the snapshot's
  constants and learnable from practice: an amm venue now takes
  `reference_paths` (per-token waypoints, as `market_model.path` moves
  SOL's), the venue metadata names only prices no path moves, and the
  family draws each session's level (JitoSOL 12 bp under to 1 bp over its
  stake value, JTO and GEOD within 30 bp) and a slow drift. On one fixed
  secret, `backrun-sol-usdc`'s and `liquidation-flash`'s reports are
  identical but for wall time and fuel.

- 2026-11-22: the transaction layer's model of the ordering contest wasn't
  good enough, and is replaced (the open question above;
  [tx layer](../tx-layer.md#ranking)). A Jito leader's builder ranked every
  bundle above every priority-fee transaction, so in all three tasks the
  kind of bid decided the winner. On blocks re-fetched for the research's
  contested slots (22 SOL/USDC backruns with rivals in the winner's slot,
  30 contested Kamino liquidations above about $1k;
  `ops/research/atomic-arb/ordering/`), that rule puts the actual winner
  first in 8 and 11. Mainnet ranks by arrival, then by pay: a contender
  that paid more per unit and still lost landed a median 155 (SOL/USDC)
  and 382 (Kamino) positions later, and among contenders landing within 5
  positions of each other the one paying the most per requested unit won
  20 times in 22 and 28 in 30; one scale over the whole slot gets 13 and
  11. The builder now ranks by the slot an entry reached the leader in,
  then by what it pays the leader per requested unit (priority fees, tips
  on a Jito leader, half the base fee), a bundle and a transaction alike,
  then by the step sent, then by a seeded coin between senders: every send
  of one step's turns reaches the same slot, so the order the grader runs
  them in wins no tie, and in a trigger's slot the best payer of the
  trigger step lands right behind it. Jito's auction still holds for
  bundles (the least tip, the background's tips on a hot pool, one bundle
  per account, a quarter of the block), but a bundle lands where its rank
  puts it, not at the top. A tip transaction now asks for 1,000 compute
  units, not the runtime's default 200,000, which on one scale diluted
  every tipped bundle by about a third (the host's and the actors' alike).
- 2026-11-22: mandate v0.3.0's rule 11 is judged from each of the bot's
  transactions' own balances in lending sessions too, as in amm sessions,
  and in both from the wallet at the start, moved by each transaction that
  ran in the chain's order (the layer's), never by a step's reading: a bot
  that splits a route over two transactions in one step held what it
  bought in between, and breaks the rule
  (`lending::observe::held_after_each`). A lending session adds its
  obligations' positions at the step's reading after the transaction. The
  `liquidation-flash` reference broke it first: it sold 99.8% of the SOL it
  expected to seize and swept the rest a step later, holding up to $105
  beside USDC between the two. Its group now always pushes Scope, so the
  program seizes at a price it knows, and it sells 99.9%.
- 2026-11-22: the three tasks recalibrated on the new builder, each on a
  fresh 72-session secret.
  - `liquidation-flash`: the bid now matters. The routes add no latency
    (the research's lag already holds every delay), so a rival that acts
    in the slot it saw the obligation turn races the bot for it; above
    $1,000 repaid the research's 4.4 rivals are a floor; a liquidator pays
    its share of the bonus per unit as mainnet's did on their median
    333k units. The reference tips 36%, the middle of mainnet's
    liquidators' median cost shares (28 to 44%), not the 65% fitted to the
    band. Reference $227.16, informed $385.17 (0.59); `min-tip` $158.53,
    `scope-reader` $77.41, `overbidder` $57.64; `min-tip < reference`
    holds at bootstrap odds 0.90.
  - `backrun-sol-usdc`: a parent order's chunks share a side and mostly a
    pool, and a generator-aware bot can do nothing with it: rule 11 (now
    per transaction) forbids holding USDC for the next chunk, and a send
    made before a trigger lands ahead of it. The family still makes the
    next trigger less certain (orders that route some chunks to the other
    pool, lone swaps of either side). Under one scale the pool searchers
    win what they outbid, and most triggers had no fast rival: 95% of
    sessions now hold a searcher that reaches every trigger's slot (the
    research's 19 in 20). The reference tips 34%, the research's median
    winning payment, and its sale sells for exactly the quoted USDC.
    Reference $2,503.02, informed $4,584.29 (0.55); `min-tip` $704.46,
    `fixed-clip` $1,669.48, `overbidder` $431.81; the half-tipping probe
    $2,787.38.
  - `backrun-thin-pools`: unchanged but for the builder, the informed
    bot's count of rivals and the reference's legs (no token left behind a
    transaction). Reference $1,113.51, informed $1,935.61 (0.58);
    `two-leg` $332.69, `overbidder` $176.94, `min-tip` $812.93, 90% of it
    from one quiet session holding an 8.2 SOL cycle, so
    `min-tip < reference` holds on the point estimate at bootstrap odds
    0.79, under the ladder's 0.85: open.
  - Perp tasks don't use the layer: on one fixed secret their references'
    reports are identical to 2cb9d8ca's but for wall time and fuel.

- 2026-11-23: one fresh hidden set passing is not evidence. The bar is
  now three: on each of three fresh 72-session draws every order rule
  holds at bootstrap odds of 0.85 or more and the reference sits in the
  band (0.2 to 0.7 of the informed bot, loss-weighted, Sharpe at least
  0.25). Graded on a Linux host, each rung of each draw in a container of
  its own (its own four bot users), so nine sets take under an hour.
  - `backrun-sol-usdc` passes as it is but for a stale rung: `fixed-clip`
    still tipped 0.5, the first reference's share, after the reference
    moved to 0.34, and on one draw it made 1.08 of the reference by
    winning races the reference lost. It now takes the reference's
    constant (`backrunner::TIP_SHARE`), and so does `liquidation-flash`'s
    `scope-reader` (0.65 against 0.36). Draws: reference 0.65, 0.61 and
    0.43 of the informed bot; every rule at odds 0.98 or more.
  - `backrun-thin-pools`: after each pool's moves were cut where its best
    cycle reaches about 2 SOL (an 8.2 SOL quiet cycle had decided
    `min-tip < reference`), the rule still swung between odds 0.77 and
    1.00, because in a quiet session `min-tip` makes twice the reference
    and a handful of them carry it. Quiet sessions went from 12% to 6%;
    the next three draws held 10, 3 and 4 of them, and the one with 10
    failed at 0.61. A set is now stratified on them (4 or 5 of 72), as
    other families stratify their regimes: 0.98, 0.99 and 0.98, the
    reference 0.61, 0.69 and 0.54 of the informed bot. Resampled, a set of
    72 still misses the band's top about one time in eight.
  - `liquidation-flash`: on the first family `min-tip` made 1.09 and 0.87
    of the reference on two fresh draws. Where a session's rivals bid
    over the reference's 36% it beat the reference (it wins every race
    nobody joins and keeps it whole); the family's average rival bid
    0.41, past the research's middle. A session's range is now centred on
    it (low end 0.15 to 0.45, width 0.05 to 0.2, average 0.36). Draws:
    odds 0.75, 0.89 and 1.00; the reference 0.52, 0.54 and 0.79 of the
    informed bot. The rule is near the floor by nature: the reference's
    lead is $0.85 a session against $3.80 of spread, so a set of `n`
    clears 0.85 with chance Φ(0.22√n − 1.04): 0.80 at 72, 0.95 at 144,
    0.99 at 216; the band is missed by 22%, 13% and 8% of such sets.
    Proposed, not done: a hidden set of 144 to 216 sessions for it.
  - `informed_boundary` failed on a Mac: its probe bot looked for any
    `mtx-informed-logs-*` in the temp directory, and found one a killed
    grader had left, or a running ladder's. The grader's private
    directories are now named with its process id, and the probe looks
    only for its own grader's.

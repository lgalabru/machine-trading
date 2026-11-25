# Design record: backrun-thin-pools

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned. The task is task 3 of
[RFD 0005](../../../docs/rfd/0005-atomic-arbitrage-tasks.md); its interface
decisions are [ADR 0004](../../../docs/adr/0004-atomic-arbitrage-in-the-sheet.md).

## Thesis

A good bot profits here because it searches a graph of pools for the cycle
that the latest swap opened, and skips cycles whose fees eat the gap.

- **Wins:** reading every pool in the trigger's own slot; listing the
  cycles from SOL back to SOL through the moved pool, of two legs and
  three (through JitoSOL or USDC); skipping those whose prices after
  their fees multiply to 1 or less; sizing the rest on the chained exact
  quotes (ticks crossed); one atomic group whose legs chain exactly and
  whose last limit is the least profit; a bid shaded to the rivals who can
  reach the slot, the least only where nobody can.
- **Loses:** looking only at a pair's two pools (every gap a third token
  closes); firing on a move inside the fee band (a 100 bp pool and a 30 bp
  one cost 130 bp before anything); a leg left without the rest of its
  cycle (the inventory limit); overbidding every race; tipping the least
  whoever races; waiting a step.
- **Regimes:** no trigger, one or several, on any of ten pools; sales and
  buys; moves of 30 bp up to each pool's cap (600 to 3,000 bp, where
  its best cycle reaches about 2 SOL); contested sessions (94%), where one
  tipping three-leg searcher reaches every trigger's slot, and quiet ones,
  where nobody can; rivals that tip or pay priority fees, that look at two
  legs or three.
- **Mandate:** v0.3.0: any Orca or Raydium pool among SOL, USDC, JitoSOL,
  JTO and GEOD (two wildcards with the five mints as `assets`), SOL the
  numeraire with at most $5 of anything else after any transaction, $1,000
  of fees and tips, a $200 loss limit; 72 hidden sessions per grade.

## Expert research

Multi-token cycles in a week of Jito's explorer (the top and the latest
200 SOL arbitrages, 2026-11-14 to 21), JitoSOL's stake pool and three
epoch boundaries (`docs/RESEARCH.md`, "LST cycles and epoch steps", and
`ops/research/atomic-arb/lst-cycles/NOTES.md`):

- **Backruns, not epoch trades.** The stake pools' rate steps 1.4 bp an
  epoch; the LST pools trade 4 to 13 bp under it and don't reprice. In 3
  boundaries no cycle on the five main LST pools reacted within 300 slots.
- **Thin pools, a bridge token.** The money in multi-token cycles is in
  backruns of large swaps on thin pools, with an LST or a mid-cap token
  as the bridge: 106 of the top 200 were closed cycles of three tokens or
  more (50.9 SOL).
- **The trigger.** 96 of 106 had one, 77 in the same block and before the
  arb, 88 within a slot; for 46 with a decoded price, the trigger moved
  its pool a median 545 bp, p90 5,877.
- **Competition re-arbs the residual.** Among the latest 200, the
  previous transaction on the pool was itself a cycle within 2 slots in
  57 of 82 cases.
- **Size.** The first leg put in a median 9.3 SOL, p90 249.
- **A detector** (Bellman–Ford over −log rates on exact quotes, then
  every simple cycle through SOL of up to four legs, golden-section
  sized) hit 152 of 188 on the arb's own state; rolling the trigger back
  made the cycle vanish in 28 of 48.

The snapshot (`snapshots/thin-pools-2026-11-21`) clones the ten pools of
those cycles: under 1 SOL moves five of them 10 bp, while the SOL/USDC and
SOL/JitoSOL pools are hundreds to 100,000 times deeper.

## The central decision

Which cycle to send. A trigger on a thin pool opens cycles of two legs
(the pair's other pool) and of three (through JitoSOL to the JTO pools, or
through USDC to the GEOD pools); which pays most depends on the moved pool,
the way it moved and the other pools' fees and depths. The options that
look defensible:

- **The pair's two pools only:** the obvious arbitrage, and some of the
  session's searchers do just that. It misses every trigger whose best
  cycle runs through a third token, and the deep JitoSOL and USDC pools
  are what let a three-leg cycle trade size.
- **Every cycle of the graph, gated by the marginal rate** (the
  reference, up to three legs): the research's detector. It finds what the
  two-leg searchers miss, and skips at once every cycle whose fees eat the
  gap.
- **Four legs:** the graph holds none (four legs need a chain of three
  tokens beside SOL, and its only links beside SOL are JitoSOL–JTO and
  GEOD–USDC), so the informed bot's search is the reference's.

What to pay the leader is the second decision, as in `backrun-sol-usdc`:
the reference tips a fixed half of its expected profit; the informed bot
bids what beats the rivals who can reach the slot and see the cycle.
Since the search can't separate them, the bid must: in the first draw most
triggers had no tipping rival in their slot, and a bot that always tipped
the least (the `min-tip` rung) beat the reference (v2 0.53 on 72 hidden
sessions, 0.91 on practice). Most sessions now hold a searcher that
reaches every trigger's slot and tips, so the least tip loses those races,
and the edge is knowing which sessions are quiet, and how little beats
whoever is there.

## Variants

| Regime (drawn per session) | Best response | Habit that fails |
| --- | --- | --- |
| A big move on a SOL/JTO or SOL/GEOD pool | the best of the pair's two-leg cycle and the three-leg one through the other token | the pair only |
| A big move on a JTO/JitoSOL or GEOD/USDC pool | a three-leg cycle: no two-leg one goes through it | the pair only (it finds nothing) |
| A move inside the fee band (most on the 100 bp pools) | nothing | firing on the trigger |
| A trigger on a deep pool (SOL/USDC, SOL/JitoSOL) | nothing: 100 SOL's worth moves it a few basis points | a cycle on every trigger |
| A contested session: a tipping rival at latency 0 that sees the cycle (94%) | outbid it, or stay out where that costs the profit | the least tip |
| A quiet session: nobody reaches the trigger's slot (6%) | the least tip that clears the background | a large fixed share |
| A two-leg rival on a three-leg cycle | the least tip: it can't see the cycle, or bids on a smaller one | a large fixed share |
| No trigger | nothing | sending on calm drift |

## Levers

| Lever | What the market does | What a good bot does | How an honest operator detects it |
| --- | --- | --- | --- |
| Trigger step | the swap lands with its slot open; the step ends behind it | sends in that turn: no route latency then | `market.trigger` |
| Graph | ten pools over five tokens; cycles of two and three legs | lists every simple cycle through SOL once | the venue metadata's mints |
| Fee gate | 1 to 100 bp a pool | the product of prices after fees, before any sizing | the pools' fee rates |
| Size | profit is concave in the size through every curve | the optimum on chained exact quotes | the kit's quotes |
| Chaining | each line spends what its amount says | each leg spends exactly what the one before was quoted to give, at that quote | the sheet's atomic groups |
| Cycle size | a trigger's move is cut, pool by pool, where the best cycle it opens on the snapshot reaches about 2 SOL of gross profit; below the cap the research's log-normal is untouched | sizes every cycle on the chained exact quotes, as any other | the moved pool and the kit's quotes, the step it opens |
| Bid | rivals tip a share of the best cycle they can see, or pay a priority fee; every send in the slot ranks by its pay per requested unit, a tip and a fee alike; most sessions hold one that reaches every trigger's slot | bids just over the rivals who reach the slot and see the cycle, per unit, the least where nobody does | rivals' landed transactions, fees and tips, on chain: who took the last trigger's cycle, in its slot, for what |
| Limits | the seat compares a limit and an amount exactly | a limit of a dozen significant digits | the seat's refusal, in `market.sends` |

## No tells

- **No predictable first move.** Each trigger's pool, side and move are
  drawn on their own; a session may hold none or eight. A directional bet
  can't pay: SOL is the numeraire, at most $5 of anything else may be
  held after any transaction, and holding SOL while the reference moves
  makes nothing.
- **No schedule to read.** Trigger slots are drawn evenly over the
  session, each on its own; the session's length is drawn on its own.
- **Overlapping regimes.** A move of 300 bp is a cycle on a 30 bp pool and
  nothing on a 100 bp one; which is which is in the pools, not the
  trigger.
- **No free cheap bid.** A bot that tips the least whatever it races wins
  only quiet sessions' triggers and what a contested session's fast
  searcher skips (under its least profit); the `min-tip` rung must score
  under the reference. The fast searcher sits at a random place in the
  roster, its share drawn as any searcher's, and which sessions are quiet
  isn't told: a bot learns it from the races it plays, which is the edge.
- **The reference moves on its own**, a small walk the pool searchers
  follow on the SOL/USDC and GEOD/USDC pools, so the pools drift between
  triggers.
- **No fair value to memorise.** JitoSOL's, JTO's and GEOD's reference
  prices in SOL were the snapshot's constants, and the pool searchers pull
  pools to them: a bot could have learnt them from practice. Each session
  now draws its own level (JitoSOL 12 bp under to 1 bp over its stake
  value, the research's band; JTO and GEOD within 30 bp of the snapshot's)
  and drifts it slowly; the venue metadata names no moving price. The
  fair value is in the pools.
- **Practice files** carry a neutral description (`mtx calibrate
  --informed` writes it), and no latent field.
- **What a generator-aware bot can do without reading the pools:** nothing
  the reference doesn't. A trigger is named in `market.trigger` (its pool
  and side, not its move); its move and what it opens are in the pools. A
  bot can't act before a trigger lands.

## Detectability

A cycle is visible the step it opens: the trigger is announced and every
pool is on chain. There is nothing to infer, only to search and compute.
What isn't visible is the competition: which rival reaches which slot is
drawn per send, and how each bids and how deep it looks shows only in the
races a bot plays. Whether a session is contested shows in its first
trigger: a contested session's fast searcher lands a tipped cycle in the
trigger's own slot, on chain, whenever it profits past its least. About a tenth of sessions hold no trigger.

## Fairness

- **Told:** the snapshot, every pool and token, the depth table, the
  lookup table, the trigger steps and their latency, every range of the
  family, the searchers' rules and the ranges they are drawn from, SOL's
  reference price at the start and the ranges the other tokens' are drawn
  from (never their levels), the mandate, the transaction layer's ranking, the
  limit's precision, and v2's scoring rules (`mtx task sync`).
- **Learnable from practice:** eight practice sessions from a public seed,
  each with both anchors' PnL; their reports say what the best cycle after
  each trigger would have made and who took it.
- **The reference stays inside the mandate:** every cycle ends in SOL, its
  legs chain exactly (dust of a few atoms stays), its costliest session
  spent $491 of the $1,000 budget (the informed bot's $532; measured before the cycle-size cap, which only lowers what a cycle can pay), and a failed bundle costs nothing.
- **Anchors run as components** (`component_anchors`): amm sessions,
  atomic groups and trigger steps reach only strategy components.
- **Role noise.** Each role of a grade (reference, informed, bot) plays a
  session with keys of its own, so its compute units and so the auction's
  ranking differ by a few percent; it averages out over a set.

## Failure modes, priced

| How a bot loses | What it costs |
| --- | --- |
| Looks at two legs only | every cycle through a third token: the `two-leg` rung |
| Overbids (95% of the profit) | most of the profit: the `overbidder` rung |
| Tips the least whoever races | every contested trigger: the `min-tip` rung |
| Fires inside the fee band | a bundle that fails costs nothing; a transaction pays its fee |
| A leg without the rest of its cycle | an inventory breach once the token is worth over $5 |
| A limit of twenty digits | the group is refused before it is sent |
| The least tip | every race a rival that bids reaches: the `min-tip` rung |

## Predictions

Before any model run: a bot that composes a two-leg cycle only lands well
under the reference (under 0.2); one that searches the graph and chains
the legs lands near it (0.4 to 0.6); one that shades its bid well, above
it; one that never composes a working group scores 0.

## Calibration

- **Three hidden sets** (2026-11-23, quiet sessions 6% and a set
  stratified by them; three fresh 72-session secrets, each rung graded on
  each, `ladder.py --sets 3`; bootstrap odds per set, floor 0.85):

  | Draw | quiet | reference | informed | ratio (band 0.2–0.7) | Sharpe | `two-leg` | `overbidder` | `min-tip` | `min-tip < reference` odds |
  | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
  | 1 | 4 | $1,013.17 | $1,653.66 | 0.61 | 0.45 | $255.3 | $128.9 | $425.0 | 0.98 |
  | 2 | 5 | $1,385.67 | $2,019.19 | 0.69 | 0.53 | $248.6 | $203.2 | $606.2 | 0.99 |
  | 3 | 5 | $713.86 | $1,330.85 | 0.54 | 0.42 | $125.6 | $159.5 | $313.7 | 0.98 |

  No losing session for either anchor (45, 46 and 44 flat for the
  reference); the other two rules hold at odds 0.995 or more and no rung
  breached. `mtx gates --fast` passes the ladder and the band on each
  draw's own summary. `min-tip` makes all its money in the quiet sessions
  (twice the reference's there) and $0.50 to $2.50 in the contested ones.
- **Statistical power: quiet sessions.** At 12% a set of 72 held about 9
  quiet sessions, and the rule was "the reference's quiet money is under
  its contested money" (below): odds 0.77 and 0.79 on two sets, 1.00 on a
  third. At 6% unstratified, three fresh sets held 10, 3 and 4 quiet
  sessions (10 or more is one set in seventy), and the one with 10 held
  the rule at odds 0.61 (`min-tip` $980.0, reference $1,054.35, informed
  $1,921.84, ratio 0.55), the others at 0.997 and 1.00 (references
  $725.97 and $940.76, ratios 0.69 and 0.59). A set is now stratified on
  them (`ThinPoolsFamily::draw_set`): 4 quiet sessions of 72, or 5 with
  the fraction's chance (0.32), as other families stratify their regimes.
  Resampling the 432 sessions of these six sets into stratified sets of
  72 (quiet ones from the 31 quiet, the rest from the 401 contested), the
  rule passes on 93% of sets and the band on 87% (the ratio's 5th to 95th
  percentile 0.52 to 0.77: with fewer quiet sessions, where the informed
  bot makes twice the reference, the ratio rises), so all three of three
  draws about half the time. At 144 sessions: 100% and 95% (three draws
  0.86); at 216, 100% and 97% (0.91). The three draws above pass both; a
  set of 72 can still miss the band's top.
- **Statistical power: the cycle-size cap.** On the set before it
  (2026-11-22: reference $1,113.51, `min-tip` $812.93), `min-tip <
  reference` held on the point estimate at odds 0.79 only: one quiet
  session (nobody reaches a trigger's slot) held an 8.2 SOL cycle, the
  set's largest by far, `min-tip` took all of it ($735) and the reference
  half ($367). One session's tail decided the rule, which is power, not
  skill. Each pool's moves are now cut where the best cycle they open on
  the snapshot reaches about 2 SOL of gross profit (measured offline with
  the kit's exact quotes: 600 bp on the Whirlpool SOL/JTO pool, 700 on
  the Raydium SOL/USDC, 750 on the 30 bp JTO/JitoSOL, 1,000 on GEOD/USDC,
  1,050 on the Raydium SOL/GEOD, 1,350 on the Raydium SOL/JTO, 1,750 on
  the 100 bp JTO/JitoSOL, 2,000 on the Whirlpool SOL/GEOD, 3,000 on the
  two deep pools); below the cap the log-normal is the research's, past
  it the move is the cap. Quiet sessions stayed 12% then. The cap bounds a
  trigger's cycle (the largest on the sets since: 2.7 SOL), but not the
  rule's whole variance: in a quiet session `min-tip` makes twice the
  reference, so the rule is "the reference's quiet money is under its
  contested money", and both are carried by a handful of sessions. On a
  72-session set graded while choosing the cap, `min-tip < reference`
  still held at odds 0.77 only (`min-tip` $471, reference anchor $618,
  informed $1,229; three quiet sessions made all of `min-tip`'s money),
  and a variant that drew a move past the cap again (no move at the cap,
  the median falling to 220 bp) held at 0.75 (`min-tip` $225, reference
  $281). A set right after the cap held at 1.00 (reference $1,209.43,
  informed $1,862.05, `min-tip` $155.15); one like the first could fail
  again, so the quiet sessions were cut ("Statistical power: quiet
  sessions").
- **Practice** (seed 20261122, 8 sessions, quiet 6%: none of the 8):
  reference $184.05, informed $293.81 (0.63).
- **Earlier hidden set** (before one scale, 2026-11-22): reference
  $5,179.23, informed $9,123.65 (0.57); `two-leg` 0.061, `overbidder`
  0.068, `min-tip` 0.007, each rule at odds 1.00.
- **The least tip, before and after.** On the first family (most triggers
  had no tipping rival in their slot), `min-tip` made $3,578 on a fresh
  72-session hidden set where the reference made $3,365 and the informed
  bot $6,692: v2 0.53, over the reference; 0.91 on practice. A family
  whose order rules held without it rewarded blind cheap bidding. With a
  searcher that reaches every contested trigger's slot, it falls to 0.007
  hidden and 0.27 on practice, and its rule (`min-tip < reference`) is in
  the ladder. Not `min-tip < informed` alone: the reference tips a fixed
  half and has no read of the races either, so a bot with no read that
  bids less than it must not score more.
- **Tip share.** Half the expected profit, as `backrun-sol-usdc`'s
  reference: the informed bot's edge is the bid (it bids just over the
  fast searcher's share where that share leaves a profit, stays out where
  it doesn't, and pays the background's floor in quiet sessions), not the
  search, since the graph holds no four-leg cycle. The reference loses
  every race to a fast searcher tipping over half, and overpays one that
  tips under a tenth.
- **First draw** (2026-11-22, before the changes): reference $4,844.48,
  informed $8,550.74 (0.567), `two-leg` 0.098, `overbidder` 0.052; the
  reference's costliest session spent $433 on fees and tips.
- **Tails.** A first family with the research's moves up to 6,000 bp and
  1,500 SOL's worth opened cycles of up to 200 SOL (a session's optimum
  reached $15,000), its tips outran the 10 SOL of native SOL, and the
  reference's costs outran a $500 budget: moves are cut at 3,000 bp,
  triggers at 100 SOL's worth, native SOL is 100 and the budget $1,000.
- **Anchor hashes** (frozen before any model run, `shasum -a 256` of
  `bot.wasm` as built from this commit): recorded by `mtx task sync
  --anchors` in `tests/anchor.digest` on Linux.

## Generations

- **0.1.0** (2026-11-22): the first version. The family (triggers sized by
  their move, calm flow on every pool, a reference walk, cycle searchers
  of two and three legs, pool searchers), the reference (every cycle of
  two and three legs through SOL, fee-gated, sized on exact quotes,
  tipping 50% of the expected profit), the informed bot (four legs, and
  the expected-value bid against the tipping rivals who can reach the
  slot and see the cycle), and the `two-leg` and `overbidder` rungs.
  Left out: the stake pool's epoch step, which the research's distractor
  was (`README.md`, "Realism limits"), and the pools' depth, which isn't
  drawn.
  Before any model run, the same day: the `min-tip` rung showed the bid
  edge was free (a bot tipping the least beat the reference), so most
  sessions (88%) now hold a searcher that reaches every trigger's slot,
  tips and searches three legs, and the rest are quiet; and JitoSOL's,
  JTO's and GEOD's reference prices, which the pool searchers pull to and
  a bot could have learnt from practice, are drawn per session and drift
  (JitoSOL inside its stake band), untold. Rule `min-tip < reference`.
- **0.3.0** (2026-11-22): one scale. The transaction layer ranks a slot's
  sends by pay per requested unit, a tip and a priority fee alike, after
  the slot they reached (`docs/tx-layer.md`, "Ranking"); the informed bot
  counts every rival; the reference's first leg takes exactly its quoted
  amount (a better price sells less SOL) and a later leg spends what an
  earlier cycle left, so no token stays behind a transaction (rule 11).
  Then, the same version: each pool's moves are cut where the best cycle
  they open reaches about 2 SOL ("Calibration", "Statistical power: the cycle-size cap"), as
  one quiet session's 8.2 SOL cycle decided `min-tip < reference`.
  Then, the same version (2026-11-23): quiet sessions are 6%, not 12%,
  and a set is stratified on them (4 or 5 of 72), as a handful of quiet
  sessions still decided `min-tip < reference` on some fresh sets
  ("Calibration", "Statistical power: quiet sessions").
- **0.15.0** (2026-11-25): unchanged but for its instruction (RFD 0007,
  2026-11-25). luthor's arbitrage scout watches these ten pools on
  mtx-alpha. From 2026-11-23 21:47 to 2026-11-25 06:10 UTC it saw 14 cycles
  pay 0.001 SOL or more: seven through SOL/USDC (one of 0.61 SOL, the others
  0.002 to 0.05), seven through GEOD (0.002 to 0.04 SOL behind moves of 42
  to 287 bp) and none through JTO, whose cycles never came within 52 bp of
  paying; every one was gone within three slots. The family's triggers
  (median 545 bp, 55% of them on the JTO pools, cycles up to about 2 SOL)
  are the week's largest over all of Solana's pools, so the task is richer
  than these pools in rate and in size. Not recalibrated: the scout reads
  a write a slot after it lands and can't see a gap opened and closed
  inside one block, where 77 of the research's 106 cycles landed, so its
  gaps are what the race left, not the triggers, and 14 of them in one
  calm day fit no move distribution or pool mix. The instruction states
  the departure beside these numbers; a tape of the pools' transactions
  would measure the triggers. Rewards compare with 0.14.0's.

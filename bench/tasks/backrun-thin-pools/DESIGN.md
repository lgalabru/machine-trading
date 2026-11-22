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
  reach the slot.
- **Loses:** looking only at a pair's two pools (every gap a third token
  closes); firing on a move inside the fee band (a 100 bp pool and a 30 bp
  one cost 130 bp before anything); a leg left without the rest of its
  cycle (the inventory limit); overbidding every race; waiting a step.
- **Regimes:** no trigger, one or several, on any of ten pools; sales and
  buys; moves of 30 to 3,000 bp; rivals that reach the slot or are late,
  that tip or pay priority fees, that look at two legs or three.
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

## Variants

| Regime (drawn per session) | Best response | Habit that fails |
| --- | --- | --- |
| A big move on a SOL/JTO or SOL/GEOD pool | the best of the pair's two-leg cycle and the three-leg one through the other token | the pair only |
| A big move on a JTO/JitoSOL or GEOD/USDC pool | a three-leg cycle: no two-leg one goes through it | the pair only (it finds nothing) |
| A move inside the fee band (most on the 100 bp pools) | nothing | firing on the trigger |
| A trigger on a deep pool (SOL/USDC, SOL/JitoSOL) | nothing: 100 SOL's worth moves it a few basis points | a cycle on every trigger |
| A tipping rival at latency 0 that sees the cycle | outbid it, or stay out where that costs the profit | the least tip |
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
| Bid | rivals tip a share of the best cycle they can see, or pay a priority fee | bids just over the rivals who reach the slot and see the cycle | rivals' landed bundles and tips, on chain |
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
- **The reference moves on its own**, a small walk the pool searchers
  follow on the SOL/USDC and GEOD/USDC pools, so the pools drift between
  triggers.
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
races a bot plays. About a tenth of sessions hold no trigger.

## Fairness

- **Told:** the snapshot, every pool and token, the depth table, the
  lookup table, the trigger steps and their latency, every range of the
  family, the searchers' rules and the ranges they are drawn from, the
  reference prices, the mandate, the transaction layer's ranking, the
  limit's precision, and v2's scoring rules (`mtx task sync`).
- **Learnable from practice:** eight practice sessions from a public seed,
  each with both anchors' PnL; their reports say what the best cycle after
  each trigger would have made and who took it.
- **The reference stays inside the mandate:** every cycle ends in SOL, its
  legs chain exactly (dust of a few atoms stays), its costliest session
  spent $433 of the $1,000 budget, and a failed bundle costs nothing.
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
| Fires inside the fee band | a bundle that fails costs nothing; a transaction pays its fee |
| A leg without the rest of its cycle | an inventory breach once the token is worth over $5 |
| A limit of twenty digits | the group is refused before it is sent |
| A priority fee in place of a tip | every race a tipping bundle enters on a Jito leader |

## Predictions

Before any model run: a bot that composes a two-leg cycle only lands well
under the reference (under 0.2); one that searches the graph and chains
the legs lands near it (0.4 to 0.6); one that shades its bid well, above
it; one that never composes a working group scores 0.

## Calibration

- **Hidden set** (a fresh 72-session secret, 2026-11-22, `ladder.py
  --sets 1`): reference $4,844.48, informed bot $8,550.74; the reference
  is 0.567 of the informed bot (band 0.2 to 0.7; no losing session for
  either, so loss-weighting changes nothing), per-session Sharpe 0.64 for
  the reference and 0.60 for the informed bot (at least 0.25); 17 sessions
  flat for both. The reference won 124 of 217 triggers, the informed bot
  123. The reference's costliest session spent $433 on fees and tips, the
  informed bot's $281.
- **Ladder** (same set, v2 scores): `two-leg` 0.098 ($949.63, 0.20 of the
  reference), `overbidder` 0.052 ($504.21), the informed bot as a rung
  0.974 ($8,357.80, role noise), nop 0; both order rules hold.
- **Practice** (seed 20261122, 8 sessions): reference $411.65, informed
  $821.42 (0.50).
- **Tip share.** Half the expected profit, as `backrun-sol-usdc`'s
  reference: the informed bot's edge is the bid (it pays the background's
  floor where no tipping rival that sees the cycle can reach the slot),
  not the search, since the graph holds no four-leg cycle.
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

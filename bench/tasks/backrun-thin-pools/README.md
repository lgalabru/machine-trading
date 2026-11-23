# solana-foundation/backrun-thin-pools

The agent's bot backruns large swaps on ten thin concentrated pools over
SOL, USDC, JitoSOL, JTO and GEOD: in the trigger's own slot, it searches
the graph of pools for the cycle from SOL back to SOL that the swap opened,
skips cycles whose fees eat the gap, and closes the best in one atomic
transaction, racing competing searchers in a Jito-style auction.

| | |
| --- | --- |
| Market | Six Orca Whirlpools and four Raydium CLMM pools over SOL, USDC, JitoSOL, JTO and GEOD; mainnet snapshot of 2026-11-21 (`thin-pools-2026-11-21`) |
| Sessions | 144 hidden per grade (about a tenth hold no trigger; twice the older tasks' 72, see `DESIGN.md`); 8 practice. Each is 400–800 slots. |
| Mandate | v0.3.0: any Orca or Raydium pool among the five tokens, SOL the numeraire with at most $5 of anything else after each transaction, $1,000 of fees and tips, $200 loss limit |
| Bound | None: scored by v2 alone |
| Reference | v2's 0.5. On each trigger, every cycle through SOL of two and three legs, gated by its marginal rate after fees, sized on exact quotes; the best as one atomic group whose last limit is its least profit, tipping 50% of the profit it expects |
| Anchors | Scoring v2: 0.5 the reference; 1 the informed bot, searching up to four legs and told each searcher's latency, search depth and bid rule, which bids what pays best against the rivals who can reach the slot and see the cycle |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it searches a graph of pools for the cycle
that the latest swap opened, and skips cycles whose fees eat the gap.

- **Wins:** reading every pool in the trigger's step; the cycles through a
  third token (JitoSOL to the JTO pools, USDC to the GEOD pools) as well
  as a pair's two pools; the fee gate before any sizing; the optimum on
  chained exact quotes; legs that chain exactly in one atomic group; a tip
  shaded to the rivals who reach the slot and see the cycle.
- **Loses:** a pair's two pools only; firing inside the fee band (130 bp
  through a 100 bp and a 30 bp pool); triggers on the deep pools; a leg
  left without its cycle (the inventory limit); overbidding every race;
  tipping the least whoever races.

## The market

- **Triggers:** none to eight a session, in slots drawn evenly over it,
  each a large swap on one pool (mostly the JTO, GEOD and JTO/JitoSOL
  pools), a side of its own (even odds), moving its price by a log-normal
  draw through the research's median 545 bp (p90 5,877), kept within
  30–3,000 bp and cut, pool by pool, where the best cycle it opens would
  pass about 2 SOL (600 bp on the Whirlpool SOL/JTO pool to 3,000 on the
  deep ones), trading at most 100 SOL's worth. Each is a trigger: the
  step ends behind it with its slot open.
- **Calm flow** on every pool, in proportion to its depth, scaled by
  0.6–1.4 per session; SOL's reference price walks by 0.5–2 bp ticks;
  the other tokens' reference prices in SOL start at a level of the
  session's own (JitoSOL 12 bp under to 1 bp over its stake value, JTO
  and GEOD within 30 bp of the snapshot's) and drift slowly, untold: the
  pool searchers pull the pools to them.
- **Competition:** 0–5 cycle searchers (two legs only for 40% of them,
  else up to three; bid 0–95% of their expected profit, tipped or as a
  priority fee; latency 0–4 slots) and 1–2 pool searchers that trade any
  pool back to its reference and bid 76–99%. Most sessions (94%) add one
  cycle searcher that tips, searches three legs and reaches every
  trigger's slot, as the research's same-slot re-arbs say someone usually
  does; in the rest nobody can reach a trigger's slot, and only there does
  the least tip win. Which kind a session is isn't told.
- **Pools** are mainnet's at the snapshot: under 1 SOL moves five of them
  10 bp; the SOL/USDC and SOL/JitoSOL pools are hundreds to 100,000 times
  deeper.

## Scoring

Scoring v2 only: 0 doing nothing, 0.5 the reference, 1 the informed bot,
on the same sessions. The account is measured on chain in SOL: wrapped SOL
plus every other token at its reference price, at the session's starting
reference, less the native SOL paid in fees and tips at $69.41 a SOL. A
breach of v0.3.0's inventory or costs rule is a breach like the loss
limit.

## Baselines

On three fresh 72-session hidden sets (2026-11-23, 4, 5 and 5 quiet
sessions): the reference made $1,013.17, $1,385.67 and $713.86 (no losing
session) and the informed bot $1,653.66, $2,019.19 and $1,330.85: the
reference is 0.61, 0.69 and 0.54 of the informed bot, inside the strength
band on each. The rungs made $255 / $249 / $126 (`two-leg`), $129 / $203 /
$160 (`overbidder`) and $425 / $606 / $314 (`min-tip`, all of it in the
quiet sessions); every order rule holds on each set at bootstrap odds of
0.98 or more. On the 8 practice sessions, $184.05 and $293.81.

| Rung | What it is | Expected |
| --- | --- | --- |
| `nop` | the starter | 0 |
| `two-leg` | the reference, looking at a pair's two pools only | well under the reference |
| `overbidder` | the reference, tipping 95% of the profit | under the reference |
| `min-tip` | the reference, always tipping the least (1,000 lamports) | under the reference |
| `reference` | the 0.5 | 0.5 |
| `informed` | the 1, told the latent state | 1 |

The checks that need the images (oracle, nop, cheater, sneaky through
Harbor) haven't run yet.

## Realism limits

- **No epoch step.** The research's distractor, JitoSOL's stake-pool rate
  stepping 1.4 bp at an epoch boundary while the pools ignore it, isn't in
  the sessions: the runtime has no stake-pool venue a bot could deposit
  or withdraw through, and stepping the rate offline needs an override of
  the stake pool's `total_lamports`, `pool_token_supply` and
  `last_update_epoch` that no session writes yet (the snapshot's
  README, "What it holds"). A bot can't trade it, so there is nothing to
  be distracted by.
- The pools' depth is the snapshot's in every session; the research's
  pools were often thinner, and its triggers' 90th percentile (5,877 bp)
  would open cycles of tens of SOL here, so moves are cut at 3,000 bp and
  triggers at 100 SOL's worth. The tail is cut further: each pool's moves
  stop where the best cycle they open on the snapshot reaches about 2 SOL
  of gross profit (600 to 2,000 bp on the thin pools), the size of the
  research's largest backruns. Larger moves happen on mainnet; here one
  of them in a quiet session decided the ladder's order on its own
  (`DESIGN.md`, "Calibration").
- The other tokens' reference prices drift on their own, not with any
  pool's flow, and JitoSOL's stake value doesn't step at epochs; their
  session levels are drawn evenly, not from a measured distribution.
- The builder ranks the sends that reach a slot together by their pay per
  compute unit, a tip and a priority fee alike, and a whole slot is one
  window: on mainnet arrival within the slot decides more (a richer
  contender that arrives a few hundred transactions later lands after the
  winner), and here only the searchers' latency in slots stands for it
  (`docs/tx-layer.md`, "Ranking").
- Whether anyone races a trigger is drawn per session, not per trigger
  or pool: a contested session's fast searcher reaches every trigger's
  slot and searches the whole graph at once. On mainnet the fastest rival
  differs from pool to pool and trigger to trigger, and the research
  measured how often a re-arb came within 2 slots (57 of 82), not who
  reached the trigger's own slot.
- Searchers send plain swap routes from the session's lookup table, not
  their own programs, and decide on the state after each step's blocks,
  never on pending transactions.
- Triggers land through the grader, outside the auction; the calm flow and
  the searchers are in it, all on the layer's Jito route.
- A fill is judged at its average price; the account is valued at the
  reference prices, which only the pool searchers follow.
- Each role of a grade plays with its own keys, so compute units (and so
  the auction's ranking) differ slightly between runs of a session.
- Failed bundles cost nothing, as on Jito; the only bound on spraying them
  is 64 pending sends.

## More

- [DESIGN.md](DESIGN.md): the design record.
- [instruction.md](instruction.md): what the agent is told.

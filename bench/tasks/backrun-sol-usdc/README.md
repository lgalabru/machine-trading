# solana-foundation/backrun-sol-usdc

The agent's bot backruns large swaps on Orca's and Raydium's SOL/USDC
concentrated pools: in the trigger's own slot, it sells SOL into the pool
that prices it higher and buys it back from the other in one atomic
transaction, racing competing searchers in a Jito-style auction.

| | |
| --- | --- |
| Market | Orca Whirlpool and Raydium CLMM SOL/USDC pools; mainnet snapshot of 2026-11-21 (`atomic-arb-2026-11-21`) |
| Sessions | 72 hidden per grade (about an eighth hold no chunk at all); 8 practice. Each is 600–1,100 slots. |
| Mandate | v0.3.0: any Orca or Raydium SOL/USDC pool, SOL the numeraire with at most $5 of anything else after each transaction, $1,000 of fees and tips, $200 loss limit |
| Bound | None: scored by v2 alone |
| Reference | v2's 0.5. On each trigger, the cycle that profits most on both pools' exact quotes, as one atomic group whose last limit is its least profit, tipping 50% of the profit it expects |
| Anchors | Scoring v2: 0.5 the reference; 1 the informed bot, told each searcher's latency, size and bid rule, which bids what pays best against the rivals who can reach the slot |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it reacts in the trigger's slot, sizes the
cycle to the fee band through both curves, and bids only what it must to
beat the other searchers.

- **Wins:** reading the moved pool in the trigger's step; the optimum
  through both curves on exact quotes; one atomic group with the least
  profit as its last limit; a tip shaded to the rivals who reach the slot.
- **Loses:** waiting a step; a fixed clip; firing inside the 8 bp band or
  on chunks into the deep pool; overbidding every race; a sale left
  without its buy back (the inventory limit); priority fees against
  tipped bundles.

## The market

- **Parent orders:** none, one, two or three a session, each 2,000–7,000
  SOL, a side of its own (even odds), into Raydium (85%) or the Whirlpool,
  in round or uneven chunks of 200–1,000 SOL, 30–200 slots apart. An order
  may start before the session or outlive it. Each chunk is a trigger: the
  step ends behind it with its slot open.
- **Calm flow** at the research's rates and sizes on both pools, scaled by
  0.6–1.4 per session; a reference price that walks by 0.5–2 bp ticks.
- **Competition:** 0–5 cycle searchers (best size or a 100–300 SOL clip;
  bid 0–95% of their expected profit, tipped or as a priority fee; latency
  0–4 slots) and 1–2 pool searchers that trade one pool back to the
  reference and bid 76–99%.
- **Pools** are mainnet's at the snapshot; a 500 SOL chunk into Raydium
  opens about 30 bp, 1,000 SOL about 60 bp.

## Scoring

Scoring v2 only: 0 doing nothing, 0.5 the reference, 1 the informed bot,
on the same sessions. The account is measured on chain in SOL: wrapped SOL
plus USDC at the reference price, at the session's starting reference,
less the native SOL paid in fees and tips at $70.24 a SOL. A breach of
v0.3.0's inventory or costs rule is a breach like the loss limit.

## Baselines

On a fresh 72-session hidden set (2026-11-21): the reference made
$6,265.92 (no losing session, 21 flat, per-session Sharpe about 0.89) and
the informed bot $11,406.03 (Sharpe about 0.88): the reference is 0.55 of
the informed bot, inside the strength band. On the 8 practice sessions,
$839.76 and $1,626.75.

| Rung | What it is | Expected |
| --- | --- | --- |
| `nop` | the starter | 0 |
| `fixed-clip` | the reference, trading 200 SOL every time | under the reference |
| `overbidder` | the reference, tipping 95% of the profit | under the reference |
| `probe` | milestone E's backrun probe (`bots/backrun-probe`): the reference's cycle, tip and limits | about the reference |
| `reference` | the 0.5 | 0.5 |
| `informed` | the 1, told the latent state | 1 |

The checks that need the images (oracle, nop, cheater, sneaky through
Harbor) haven't run yet.

## Realism limits

- Two pools only: no third venue, no Jupiter routes, no single-pool exit
  for the bot (the pool searchers stand for that trade).
- The pools' depth is the snapshot's in every session; mainnet's varied
  fivefold over the week.
- A Jito leader lands bundles at the top of its block, so a tipped bundle
  beats every priority fee in the layer's builder; on mainnet most of the
  research's winners paid priority fees.
- Searchers send plain swap routes from the session's lookup table, not
  their own programs, and decide on the state after each step's blocks,
  never on pending transactions.
- Triggers land through the grader, outside the auction; the calm flow and
  the searchers are in it, all on the layer's Jito route (its latency and
  losses), never a staked RPC.
- A fill is judged at its average price; the account is valued at the
  reference price, which only the pool searchers follow.
- Each role of a grade plays with its own keys, so compute units (and so
  the auction's ranking) differ slightly between runs of a session.
- Failed bundles cost nothing, as on Jito; the only bound on spraying them
  is 64 pending sends.

## More

- [DESIGN.md](DESIGN.md): the design record.
- [instruction.md](instruction.md): what the agent is told.

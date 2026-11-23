# solana-foundation/backrun-sol-usdc

The agent's bot backruns large swaps on Orca's and Raydium's SOL/USDC
concentrated pools: in the trigger's own slot, it sells SOL into the pool
that prices it higher and buys it back from the other in one atomic
transaction, racing competing searchers for the slot: the send that pays
the leader most per compute unit lands right behind the trigger.

| | |
| --- | --- |
| Market | Orca Whirlpool and Raydium CLMM SOL/USDC pools; mainnet snapshot of 2026-11-21 (`atomic-arb-2026-11-21`) |
| Sessions | 72 hidden per grade (about an eighth hold no chunk at all); 8 practice. Each is 600–1,100 slots. |
| Mandate | v0.3.0: any Orca or Raydium SOL/USDC pool, SOL the numeraire with at most $5 of anything else after each transaction, $1,000 of fees and tips, $200 loss limit |
| Bound | None: scored by v2 alone |
| Reference | v2's 0.5. On each trigger, the cycle that profits most on both pools' exact quotes, as one atomic group whose last limit is its least profit, tipping 34% of the profit it expects: mainnet's median winning payment |
| Anchors | Scoring v2: 0.5 the reference; 1 the informed bot, told each searcher's latency, size and bid rule, which bids what pays best against the rivals who can reach the slot |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it reacts in the trigger's slot, sizes the
cycle to the fee band through both curves, and bids only what it must to
beat the other searchers.

- **Wins:** reading the moved pool in the trigger's step; the optimum
  through both curves on exact quotes; one atomic group with the least
  profit as its last limit; a bid shaded to the rivals who reach the slot.
- **Loses:** waiting a step; a fixed clip; firing inside the 8 bp band or
  on chunks into the deep pool; overbidding every race; bidding the least
  into a contested slot; a sale left without its buy back (the inventory
  limit).

## The market

- **Parent orders:** none, one, two or three a session, each 2,000–7,000
  SOL, a side of its own (even odds), into Raydium (85%) or the Whirlpool,
  some routing 10–35% of their chunks to the other pool, in round or
  uneven chunks of 200–1,000 SOL, 30–200 slots apart. An order may start
  before the session or outlive it. Each chunk is a trigger: the step ends
  behind it with its slot open.
- **Lone swaps:** none, one or two a session, 165–1,286 SOL, either side,
  from no order: the next trigger's side and pool are likely, not certain.
- **Calm flow** at the research's rates and sizes on both pools, scaled by
  0.6–1.4 per session; a reference price that walks by 0.5–2 bp ticks.
- **Competition:** 0–5 cycle searchers (best size or a 100–300 SOL clip;
  bid 0–95% of their expected profit, tipped or as a priority fee; latency
  0–4 slots), one more that reaches every trigger's slot in 95% of
  sessions, and 1–2 pool searchers that trade one pool back to the
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

On three fresh 72-session hidden sets (2026-11-23): the reference made
$3,395.56, $3,029.15 and $1,485.34 (no losing session) and the informed
bot $5,199.11, $4,942.13 and $3,456.50: the reference is 0.65, 0.61 and
0.43 of the informed bot, inside the strength band on each. The rungs made
$2,161 / $1,973 / $1,109 (`fixed-clip`), $477 / $515 / $456
(`overbidder`), $946 / $801 / $718 (`min-tip`) and $3,171 / $2,850 /
$2,086 (`probe`); every order rule holds on each set at bootstrap odds of
0.98 or more. On the 8 practice sessions, $27.74 and $89.68.

| Rung | What it is | Expected |
| --- | --- | --- |
| `nop` | the starter | 0 |
| `fixed-clip` | the reference, trading 200 SOL every time | under the reference |
| `overbidder` | the reference, tipping 95% of the profit | under the reference |
| `min-tip` | the reference, always tipping the least (1,000 lamports) | under the reference |
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
- The builder ranks the sends that reach a slot together by their pay per
  compute unit, a tip and a priority fee alike, and a whole slot is one
  window: on mainnet arrival within the slot decides more (a richer
  contender that arrives a few hundred transactions later lands after the
  winner), and here only the searchers' latency in slots stands for it
  (`docs/tx-layer.md`, "Ranking").
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

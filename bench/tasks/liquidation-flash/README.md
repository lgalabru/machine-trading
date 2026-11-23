# solana-foundation/liquidation-flash

The agent's bot liquidates unhealthy Kamino obligations (SOL collateral,
USDC debt) on the main market: it pushes Scope's price, flash borrows the
repay, liquidates and sells the seized SOL on Orca's or Raydium's SOL/USDC
pool, all in one transaction, racing competing liquidators for the slot:
the send that pays the leader most per compute unit lands first.

| | |
| --- | --- |
| Market | Kamino Lend main market (SOL/USDC reserves), Scope, Orca Whirlpool and Raydium CLMM SOL/USDC pools; mainnet snapshot of 2026-11-21 (`atomic-arb-2026-11-21`) |
| Sessions | 72 hidden per grade, half opening with a falling leg and half with a rising one (about a quarter liquidate nothing); 8 practice. Each is 260–360 slots. |
| Mandate | v0.3.0: SOL and USDC reserves without borrowing, any Orca or Raydium SOL/USDC pool, flash borrows up to 250,000 USDC, at most $100 of anything but USDC after each transaction, $300 of fees and tips, $500 loss limit |
| Bound | None: scored by v2 alone |
| Reference | v2's 0.5. Reads the upstream, values every obligation there, and liquidates each one under water in one atomic group (push, flash, liquidate at the close factor, sale on the better pool), tipping 36% of the profit it expects: the middle of what mainnet's liquidators paid the leader of the bonus they kept |
| Anchors | Scoring v2: 0.5 the reference; 1 the informed bot, told how the session's liquidators bid and race and the keeper's cadence, which bids what pays best against them |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot profits here because it reads the price upstream of Kamino's
oracle, sees an obligation cross its liquidation threshold before the
oracle shows it, and pushes the update, liquidates with a flash loan and
sells the collateral in one transaction, bidding only what it must to land
ahead of competing liquidators.

- **Wins:** the research's detector at the upstream price (e-mode and
  borrow factors included); one atomic group per liquidation; the close
  factor's repay; selling exactly what the program seized on the pool that
  pays more; a bid sized to the rivals who race.
- **Loses:** reading Scope's own price (a push too late); overbidding every
  race; selling SOL it didn't seize (the group fails); holding the seized
  SOL (the inventory limit); never composing a working group.

## The market

- **SOL upstream** walks in legs of 0.2–4.5% over 30–150 slots, either
  way; the first leg is a coin, later ones fall with a chance drawn per
  session (25–80%). It ticks every 1–9 slots by a few basis points (p50
  ~5 bp, p90 ~24 bp; mainnet's Scope ticks are 3.6 and 21), with
  occasional 20–65 bp jumps. The upstream is a `FixedPrice` in Scope's
  mappings, which anyone reads; Scope's own price moves only when pushed.
- **Borrowers:** 4–14 obligations, collateral log-normal around a median of
  $1,100–2,600 (five to ten times mainnet's median debt), clipped to
  $20–150,000, health from 1.0001 to a per-session top of 1.001–1.04.
- **Competition:** 2–5 liquidators read the upstream too and send a push
  and a liquidation from their own USDC; each bids a share of its expected
  bonus (a per-session range within 0.15–0.65, 0.36 on average), per compute unit as
  mainnet's liquidations paid it on their median 333k units, as a tip or a
  priority fee; how many race and how fast follow the research, scaled per
  session (above $1,000 repaid, only up). The routes add no latency, so a
  rival that acts in the slot it saw the obligation turn races the bot for
  that slot. Scope's keeper pushes every 2–12 slots.
- **Pools** are mainnet's, moved back to the upstream after each step's
  blocks by an arbitrageur.

## Scoring

Scoring v2 only: 0 doing nothing, 0.5 the reference, 1 the informed bot,
on the same sessions. The account is valued on chain at the upstream
price: wallet USDC and wrapped SOL and reserve supply, less the SOL paid in
fees and tips at $70.26 a SOL. A breach of v0.3.0's inventory, flash or
costs rule is a breach like the loss limit.

## Baselines

On three fresh 72-session hidden sets (2026-11-23): the reference made
$146.05, $146.09 and $270.48 (no losing session) and the informed bot
$279.23, $271.38 and $341.13: the reference is 0.52, 0.54 and 0.79 of the
informed bot, so the third set is above the strength band. The rungs made
$61 / $49 / $89 (`scope-reader`), $11 / $10 / $13 (`overbidder`) and
$134 / $123 / $123 (`min-tip`): `min-tip < reference` holds on each, at
bootstrap odds 0.75, 0.89 and 1.00, so the first set is under the
ladder's 0.85. Both are a set of 72's power, not the order: see
`DESIGN.md`, "Calibration". On the 8 practice sessions, $11.63 and
$30.67.

| Rung | What it is | Expected |
| --- | --- | --- |
| `nop` | the starter | 0 |
| `scope-reader` | the reference, reading Scope's own price | under the reference |
| `overbidder` | the reference, tipping 95% of the profit | under the reference |
| `min-tip` | the reference, always tipping the least (1,000 lamports) | under the reference |
| `reference` | the 0.5 | 0.5 |
| `informed` | the 1, told the latent state | 1 |

The checks that need the images (oracle, nop, cheater, sneaky through
Harbor) haven't run yet.

## Realism limits

- Liquidators pay from their own inventory and never use flash loans;
  borrowers and the pools' arbitrageur act outside the auction, landing at
  once.
- The upstream is a `FixedPrice` written each slot, not Pyth Lazer's
  updates and their signatures; one market, two reserves.
- Pool depth isn't drawn: the pools are mainnet's at the snapshot, with
  nobody but the bot and the arbitrageur trading them.
- Each role of a grade plays with its own keys, so compute units (and so
  the auction's ranking) differ slightly between the reference's and the
  bot's runs of a session.
- Failed bundles cost nothing, as on Jito; the only bound on spraying them
  is 64 pending sends.
- The builder's window is a slot: sends that reach one slot rank by pay
  per unit, where mainnet's earlier arrivals within the slot also land
  first (`docs/tx-layer.md`, "Ranking"); the liquidators' lag in slots
  stands for arriving late.

## More

- [DESIGN.md](DESIGN.md): the design record.
- [instruction.md](instruction.md): what the agent is told.

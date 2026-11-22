# Research notes

What we learned from studying other systems and the chain itself. Each
section covers one study. Its conclusions about this repository live in
the RFD or ADR it links to.

## Atomic SOL arbitrage on mainnet, from Jito's explorer (2026-11-21)

We pulled two samples from the Jito arbitrage explorer, then fetched every
transaction from a Helius RPC to dissect it. The plan it fed is
[RFD 0005](rfd/0005-atomic-arbitrage-tasks.md).

- Source: `https://bundles.jito.wtf/api/v1/arbitrages/recent`, the API
  behind `explorer.jito.wtf/arbitrage-explorer`. Its query is
  `sort=Profit|Time&limit=200&timeframe=Week&currency_mint=<mint>`, and a
  row is `{txSignature, profitAmount, slot, timestamp, signer, currencyMint}`.
- The "top week" sample is the 200 most profitable SOL arbitrages of
  2026-11-14 to 2026-11-21. The "latest" sample is the 200 newest, which
  span four minutes on 2026-11-21.
- `profitAmount` is the explorer's figure in SOL. We read it as gross of
  fees and tips.
- 83% of the top-week transactions are version 1 transactions.
  `getTransaction` refuses them unless the request sets
  `maxSupportedTransactionVersion: 1`. Our kit, raw-send audit and snapshot
  tools must read v1 before they meet mainnet routes.

### Two populations

| | Top week | Latest |
| --- | --- | --- |
| Total profit | 104.1 SOL | 0.010 SOL |
| Median profit per arb | 0.26 SOL | 0.00002 SOL |
| Sent as a Jito bundle (tip paid) | 12% | 78% |
| Fees and tips as a share of profit | 29% | tip median 22%, p90 71% |
| Main venue | Orca Whirlpool, Raydium CLMM | `BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi` (a prop AMM), in 83% |

The latest sample is a steady drizzle of arbitrage against oracle-quoted
prop AMMs, worth almost nothing per transaction. The top-week sample is
where the money is, and it is what a task should teach.

### What earns, top week

| Strategy | Arbs | Profit | Share |
| --- | --- | --- | --- |
| Cycle through two or more tokens, SOL→A→B→SOL | 108 | 56.0 SOL | 54% |
| Cycle through one token over two pools, SOL→X→SOL | 54 | 31.7 SOL | 30% |
| Cycle funded by a flash loan (Kamino `FlashBorrowReserveLiquidity`, Jupiter Lend `FlashloanBorrow`) | 27 | 7.0 SOL | 7% |
| Mint or redeem a synthetic (Hylo `MintLevercoinLst`) | 3 | 5.8 SOL | 6% |
| Liquidation plus swap (Jupiter Lend `Liquidate`, Solend) | 8 | 3.5 SOL | 3% |

The labels come from heuristics on instruction names in the program logs.

### Venues and tokens

- Share of profit in arbs that touch each venue: Orca Whirlpool 69%,
  Raydium CLMM 48%, Raydium AMM v4 23%, Meteora DAMM v1 15%, Meteora DLMM
  15%, Pancake CLMM 5%. The shares overlap, since one arb touches several
  venues.
- The most valuable single route is Whirlpool → Raydium CLMM: 30 arbs, 21.9
  SOL, 21% of the profit.
- Whirlpool and Raydium CLMM together appear in about 75% of the profit.
  Adding Raydium AMM v4 and the two Meteora pool types covers about 90%.
- USDC is the middle leg of 58% of the profit, and JitoSOL and jupSOL of
  about 15%. The rest goes through mid-cap and meme tokens.
- 58% of the profit comes from two-pool routes. Single routes reach at most
  seven pools.

### Triggers and landing

We placed the 15 most profitable arbs in their blocks with `getBlock`.

- About 6 directly follow a transaction from another signer that wrote the
  same pool accounts: they are backruns of a large swap.
- About 5 sit at positions 1 to 10 of their block with no earlier writer to
  those pools: they chase a move from the previous slot.
- They win the race with priority fees more often than with tips. One arb
  that made 3.91 SOL paid 3.64 SOL in priority fees. Another that made
  3.68 SOL tipped 3.43 SOL.
- In the 78 arbs that paid more than 0.05 SOL in fees and tips, the payment
  was 67% of the profit. In the rest it was small.

### Who

- Five signers made 78% of the profit, each through its own program.
- The leader, `MriyaNN8TMp6qRWjfr723PK7xgQK7yCt7Kg2v2PQu7X` through
  `AN225ykGPAmckE9uMCCM7jQv3L3AYwiPZbHqgMUYEgCR`, made 37.8 SOL from 54
  arbs and paid 3.2 SOL in fees and tips. Its edge is speed and route
  coverage, not outbidding.
- The runner-up, `DtvmxrTACskMG2W8a6KXgSemUfvyNVeQTgfpJoGvMVKx`, made
  22.8 SOL and paid 15.2 SOL.

### Programs we couldn't name at first

- `24Uqj9JCLxUeoC3hGfh5W3s9FM9uCHDS2SG3LYwBpyTi`, with `Deposit` and
  `Withdraw` in 23 arbs from 11 signers, turned out to be Meteora's Dynamic
  Vault. Every Meteora DAMM v1 swap calls it, so it is not a venue of its
  own. See the [LST study](#lst-cycles-and-epoch-steps-2026-11-21).
- Prop AMMs that the arbs route through:
  `SCoRcH8c2dpjvcJD6FiPbCSQyQgu3PcUAWj2Xxx3mqn`, with its oracle
  `ojh19ojaKduoJZuaJADhcVGp4xt1TcdAvZmpVsCorch`,
  `goonuddtQRrWqqn5nFyczVKaie28f3kDkHWkHtURSLE`, and
  `ALPHAQmeA7bjrVuccPsYPiCvsi428SNwte66Srvs4pHA`.

### Limits of this study

- One week, and only arbs denominated in SOL.
- The explorer only lists arbs it detects, so it can miss some.
- The "same pool" test for a backrun compares the accounts each
  transaction writes, so it is an approximation.

## Kamino Lend liquidations (2026-11-12 to 2026-11-21)

We read every transaction that touched `KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD`
over nine days, to calibrate `liquidation-flash`
([RFD 0005](rfd/0005-atomic-arbitrage-tasks.md)). That is 1,018,575
transactions, slots 468,683,679 to 472,301,409, from Helius
`getTransactionsForAddress`. Kamino's public API has no liquidation
endpoint. The program's own logs carry almost everything:

- the LTV against the threshold, the reason and the bonus;
- `pnl: Liquidator repaid X and withdrew Y collateral`;
- every `RefreshObligation` and Scope `RefreshPriceList` update.

Reserve and market settings were read on 2026-11-21, not as of each event.
Slots ran at about 0.215 s.

### What gets liquidated

| Measure | Value |
| --- | --- |
| Price-triggered liquidations (`LtvExceeded`) | 6,307, on 1,348 obligations; 5,290 in the main market `7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF` |
| Expired fixed-term debt (`ObligationBorrowDebtTermReached`) | 8,402, all on 3 obligations of market `Btu8835QDYgdTnMJJBSidbfQhrZzryZbMhCpty6h6Xdk`, median $0.38. A detector must filter by reason. |
| Top pairs, debt←collateral | USDC←SOL 2,317, USDC←cbBTC 789, USDC←JLP 429, USDC←ETH 315 |
| Repaid per liquidation | p50 $17, p90 $936, max $160k, $5.93M in total |
| Close factor | 10% on the main market, 20% on most others. $2 debts are closed in full. |
| Bonus | the reserve minimum almost always: 100 bp in 5,485 cases, 500 bp (ETH) in 776. The protocol keeps 50%, so the liquidator keeps about 50 bp. |

- **Timing.** Liquidations follow SOL drawdowns: 2,521 on 2026-11-14, when
  SOL fell 11%, and under 60 a day on calm days.
- **Repeats.** A liquidation restores health. An obligation is liquidated
  again at a later tick rather than in a burst: 227 obligations were
  liquidated ten times or more.

### What tipped them over

We walked each obligation's health back through the Scope updates of the
5.4 minutes before its first liquidation.

- **The trigger.** For 6,007 of 6,201 episodes, a single Scope price
  update flipped health to 1 or below. Within the window it was always a
  price move, never interest accrual or a parameter change.
- **The moves are tiny:** median 3.6 bp, p90 21 bp. Health just before was
  1.8e-4 above 1 at the median. Obligations drift onto the threshold
  through small ticks.
- **Liquidators push the price themselves.** Scope is pull-updated. The
  eventual winner signed the triggering update in 39% of episodes, inside
  the liquidation transaction itself in 31%. Another liquidator signed it
  in 60%, and a plain crank in only 1%.
- **Lag from the update to the liquidation:** 0 slots in 50% of episodes,
  2 or fewer in 71%, p90 21. Above $1k repaid, 93% land in the same slot.
  A bot that only reads Scope on chain is too late for anything over $10.

### How they're done

- **Winners.** 72 different signers won. `3rSutpkQ…` took 32% of the USD
  repaid. `BkbtvEGS…` and `4XBqViD1…`, from the Jito explorer sample, win
  here too.
- **Funding:**
  - their own inventory, 44%;
  - a Kamino flash borrow from the repay reserve, 34%, with a fee of
    0.001% on USDC, SOL and USDT;
  - a Jupiter Lend flash loan, `jupgfSgfuAXv4B6R2Uxu85Z1qdzgju79s6MfZekN6XS`,
    22%.
- **Selling the collateral:** Orca Whirlpool, Jupiter, Raydium CLMM and
  Meteora DLMM, in that order. 12% keep the collateral.
- **Cost.** Median 333k CU, mid-block (median index 347). A Jito tip in
  46% of wins, median 151k lamports. Fees and tips took 25% of the
  liquidators' gross bonus.
- **Competition** grows with size. Other signers attempting the same
  obligation within ±20 slots average 0.6 under $10, 2.6 for $10 to $1k,
  and 4.4 above $1k. 30,928 attempts failed with `ObligationHealthy`: they
  landed after the winner.
- **Profit**, estimated from signer balances, understates bots that keep
  funds in their own accounts. Median $0.05 per liquidation, $3.9 from
  $1k to $10k repaid, $36 above $10k.

### A detector

The rule:

1. U = Σ deposit value × liquidation threshold, using the e-mode group's
   threshold when the obligation is in one.
2. B = Σ borrow value × borrow factor.
3. Flag when B ≥ U.
4. Then compute the close-factor repay, the bonus, the protocol's share,
   the flash fee and the swap cost.

Checked on the states the logs record:

- It flags 6,270 of 6,282 successful liquidations.
- It wrongly flags 11 of 30,886 attempts on healthy obligations, all
  rounding at health 1.0000.
- Its repay estimates match the actual repay at p10, p50 and p90.
- The 12 it misses come from e-mode settings that differ today from those
  at the time.
- Before the e-mode rule was added, the detector wrongly flagged 62 USDC←USDT
  attempts. A real bot kept sending exactly those, and they kept failing.

### What it means for the task

- A bot that only reacts to on-chain prices loses every liquidation worth
  having. The skill is reading the prices upstream of the oracle, then
  pushing the update and liquidating in one transaction. A snapshot can't
  offer that skill unless the scenario models an upstream feed that leads
  Scope.
- At mainnet sizes, a liquidation pays cents. A task needs positions 5 to
  10 times the median, or the 500 bp bonus reserves, for the signal to
  clear fee noise.
- `snapshots/kamino-2026-11-13` has only the Backpack and xStocks markets.
  The task needs a recipe with the main market, its lookup table
  `FGMSBiyVE8TvZcdQnZETAAKw28tkQJ2ccZy6pyp95URb`, the SOL reserve
  `d4A2prbA2whesmvHaL88BH6Ewn5N4bTSU2Ze8P6Bc4Q` and the USDC reserve
  `D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59`, Scope's
  `3t4JZcueEzTbVP6kLxXrL3VpWx45jDer4eqysweBchNH`, and SOL/USDC pools.

## LST cycles and epoch steps (2026-11-21)

We studied the multi-token cycles of the Jito explorer sample, and three
epoch boundaries of the main stake pools, to calibrate a task on LST cycles
([RFD 0005](rfd/0005-atomic-arbitrage-tasks.md)). The hypothesis was that
stake-pool rates step at each epoch boundary and the pools lag behind. It
was wrong.

### Epoch steps don't make arbitrage

- **The steps are small.** The exchange rate of every SPL stake pool steps
  1.40 to 1.63 bp per epoch. JitoSOL steps exactly 1.404 bp, about 2.6% a
  year.
- **The update comes late.** The first `UpdateStakePoolBalance` that mints
  fees lands 550 to 800 slots after the boundary for JitoSOL, jupSOL and
  hSOL, and 1,400 to 2,150 for dSOL and bonkSOL.
- **The pools ignore it.** On the five main LST/SOL pools, over three
  boundaries, the pool price didn't move when the rate stepped. The pool's
  discount to the rate just grew by the step, in 7 of 9 cases. No arb took
  the step.

### LST pools always trade below the stake pool's value

Over 2,753 swaps, no LST/SOL pool traded above the stake pool's value per
token.

| LST | Discount range | What sets the floor |
| --- | --- | --- |
| JitoSOL | −5 to −12 bp, highest −3.9 bp | −12.56 bp: the 10 bp `WithdrawSol` fee, the 1 bp pool fee, and about 1.5 bp |
| jupSOL | −5.5 to −11 bp | |
| mSOL | −9 to −20 bp | value approximate |

- **The pools are deep.** The 99th percentile move between two swaps is 0.2
  to 1.6 bp, and the largest was 8 bp. It takes 1,000 to 2,000 JitoSOL to
  move the main JitoSOL pools by 1 bp.
- **The stake pool's own deposit and withdraw bound the price.** A cycle
  through them is rare and tiny. None of the 400 explorer arbs used one. A
  1-in-5 sample of the Jito stake pool's transactions found 43 cycles that
  bought in a pool and redeemed with `WithdrawSol`, with a median gain of
  0.0000085 SOL.

### Where the cycles' profit comes from

Of the explorer's top 200, 106 are multi-token cycles, worth 50.9 SOL. 24
of them touch an LST, worth 11.0 SOL:

- **An LST bridging to a mid-cap token** (JTO, ORE, JLP, hSOL, soBTC): 14
  arbs, 5.5 SOL.
- **Pure LST cycles**, such as SOL→JitoSOL→jupSOL→mSOL→SOL: 6 arbs, 1.05
  SOL. They earn 3 to 11 bp on 120 to 590 SOL, because each LST holds its
  own discount while the LST/LST pools drift.
- **An LST plus a stablecoin:** 4 arbs, 4.5 SOL. Two of them are Hylo
  mints: SOL→JitoSOL, then `MintLevercoinLst` mints xSOL at the protocol's
  value, then the xSOL is sold on DLMM or CLMM. Each followed a retail xSOL
  buy of 5,000 to 6,000 USDC in the same block.

Most cycles are backruns:

- **The trigger.** In 77 of the 106, a swap that wasn't an arb hit one of
  the route's pools earlier in the same block. The median distance is 0
  slots, and the trigger's price impact a median 545 bp (p90 5,877).
- **The arb.** It puts in a median 9.3 SOL and lands at median block
  position 158. Only 13 of the 106 tipped.
- **Re-arbs.** In the latest sample, the previous transaction on the pool
  was itself an arb within 2 slots in 57 of 82 cases: bots re-arb each
  other's leftovers.

### A detector

`detector.py` (kept with the study's data) works as follows:

1. Treat each direction of each pool as an edge: a constant-product curve,
   or a fixed rate with a capacity for stake-pool deposits and prop AMMs.
2. Gate with Bellman-Ford over −log rates.
3. List the cycles through SOL of up to four legs.
4. Size each one by search, and report its profit after fees.

On pool states rebuilt from the arbs' own fills, it finds 152 of 188, and
its profit estimates match at the median. That test is circular, so it
checks the plumbing more than the idea. With the trigger swap rolled back,
the opportunity disappears in 28 of 48 testable cases.

### What it means for the task

- The cycle task is a backrun task on **thin** pools, where an LST or a
  mid-cap token is the bridge. It is not an epoch task.
- An epoch step belongs in the scenario as a distractor that a sound bot
  doesn't trade.
- The stake pool's deposit and withdraw bound the pool price between about
  −12 bp and +1 bp of the stake pool's value.
- Offline, this needs a `spl-stake-pool-rate` override (`total_lamports`,
  `pool_token_supply`, `last_update_epoch`) and a way to advance the epoch.

## SOL/USDC backruns on concentrated pools (2026-11-21)

We rebuilt every SOL/USDC arb between concentrated pools in the Jito
explorer sample, to calibrate `backrun-sol-usdc`
([RFD 0005](rfd/0005-atomic-arbitrage-tasks.md)). We pulled every block
from 8 slots before each arb to 3 after.

Two background windows came alongside:

- 7,200 calm slots, 48 minutes on a Saturday;
- two windows of 1,001 and 1,501 slots during large sell orders.

### One pair of pools

All 21 arbs cycle through the same two pools, at 4 bp each, 8 bp for the
round trip:

- Orca Whirlpool `Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE`, tick
  spacing 4;
- Raydium CLMM `3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv`, tick
  spacing 1.

The Raydium pool is the thinner one: 100 SOL moves it about 6 bp, against
1.3 bp for the Whirlpool. No SOL/USDT pool took part.

Surfpool's template catalogue labels `Czfq3x…` as POPCAT/SOL. On chain it
is SOL/USDC.

### The trigger

- **The pool hit.** 19 of 21 triggers traded the Raydium pool.
- **The route.** 17 of the 20 distinct trigger transactions came through
  Raydium's router `routeUGWgWzqBWFcrCfv8tritsqukccJPu3q5GPP3xS`, sending
  the whole trade into that one pool. None came through Jupiter.
- **The transactions** were plain: a 15,000 lamport fee and no tip.
- **Sizes:** 165 to 1,286 SOL, median 500, often round numbers.
- **The gap** between the two pools went from 0 to 3 bp before the trigger
  to 32 to 138 bp after it, median 48 bp.
- **One pool knocked off price, not a market move.** The pool that was hit
  sat 20 to 150 bp from the median of 2 to 16 other SOL/USDC pools, while
  the other stayed within 2 bp.
- **The traders behind them** sell in chunks. One wallet sold 6 chunks of
  about 1,000 SOL. Another sold 14 chunks of 500 SOL, 7,000 SOL in all,
  over 1,460 slots.

### The arb

- **Size and profit.** It puts in 104 to 996 SOL and grosses 0.10 to 4.97
  SOL, 20 SOL across the 21.
- **How much of the gap it takes.** 18 of the 21 closed the gap to the
  8 bp fee band, and took 0.86 to 1.10 of the optimum our model
  computes.
  - One bot always trades a fixed 200 SOL, and took 29% of the optimum.
  - Another left 19 bp on the table.
- **When it lands.** 18 of 21 landed in the trigger's own slot, 12 of them
  at the very next position.
  - None of the three that landed at the top of a block followed a market
    move. Each followed a trigger at the end of the previous block, or a
    gap that survived about 200 transactions.
- **What the winner paid** the leader, in priority fee and tip, as a share
  of gross profit: 0% in 7 cases, about 34% in 6, 50 to 80% in 4, 93% in 3.
- **The losers.** Up to 208 failed transactions hit the same pools in the
  arb's slot.
- **Single-pool searchers.** In the windows of large sell orders, the
  strongest competitors didn't cycle at all. They traded the Raydium pool
  alone against another venue, grossed 0.88 to 1.02 SOL where the cycle's
  optimum was about 0.70, and paid 76 to 99% of it in priority fees.

In a window of large sell orders:

- 20 gaps opened in about 17 minutes.
- The first response landed in the trigger's slot 19 times out of 20.
- 11 different bots won.
- Up to 44 different signers failed in one slot.

### The calm background

- **Flow.** In 48 minutes, the Whirlpool traded 718 swaps ($549k) and the
  Raydium pool 574 ($140k). The largest swap was 280 SOL on the Whirlpool
  and 71 SOL on Raydium.
- **No gap to trade.** The gap between the two pools had a median of
  0.24 bp, p99 4.8 bp and a maximum of 5.65 bp, never enough to pay 8 bp
  of fees.
- **Other bots.** About half the swaps leave the trader's balances
  unchanged: they are bots arbitraging against other venues. 1.75 failed
  transactions a slot hit the pools.
- **Depth varies.** The pools' liquidity varied about fivefold over the
  week, so the same trade opens very different gaps.
- **The calibration's DLMM pool is the wrong one.** Meteora DLMM
  `5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6`, bin step 4, traded more
  SOL than `BGm1tav58o…`, the DLMM pool in `calibration/tx_layer`.

### A detector

The rule:

1. Fire when the gap between the pools exceeds their summed fees.
2. Treat each pool as a constant-product curve at its active liquidity.
3. Solve the best size in closed form.
4. Subtract an assumed share for the leader.

On the arbs:

- It flags all 21, in the right direction.
- Its predicted profit matches the actual one at a median ratio of 0.998.

On the windows:

- It fired 20 times in the windows of large sell orders, each followed by a
  real backrun on the same pools.
- It fired 0 times in 1,272 checks of the calm window.

### What it means for the task

- The world is a seller working a large order in chunks of 200 to 1,000
  SOL, every 30 to 200 slots, into the thinner pool. Between chunks, the
  flow is calm.
- **A backrun lands in the trigger's own slot.** A step that shows the bot
  a block only after it closes can't express that. The session must pause
  after the trigger lands, let the bot react, and order the rest of the
  slot by fee.
- **Two kinds of competitor:**
  - cycle searchers;
  - single-pool searchers, who trade against an outside price and bid away
    most of their profit.
- Pools are moved by real swaps from an actor. The override templates only
  set price fields, not vaults or tick arrays.

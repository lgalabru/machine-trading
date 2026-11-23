# CLMM backrun on SOL/USDC — research notes (2026-11-21)

Tags: **[M]** measured from chain data in this directory, **[I]** inferred / interpretation.
Pools: **W** = Orca Whirlpool `Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE` (SOL/USDC, fee 400 = 4 bps, tick spacing 4).
**R** = Raydium CLMM `3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv` (SOL/USDC, amm_config `3h2e43PunVA5K34vwKCLHWhZF4aZpyaC9RmxvshGAQpL`, trade_fee_rate 400 = 4 bps, tick spacing 1).
Both have mint_a = wSOL, mint_b = USDC (checked on chain; the Surfpool template that calls `Czfq3…` POPCAT/SOL is wrong).
Fee band of the W↔R cycle: 8 bps.

## Samples
| set | what | size |
|---|---|---|
| cases | every round1 arb whose decoded swaps are all on SOL/USD(C/T) pools | 22 found, 21 two-pool (all exactly W↔R, all in top200), 1 Jupiter self-loop (ignored) |
| case windows | full blocks slot−8 … slot+3 around each case | 252 blocks |
| bg | consecutive blocks 472293600–472300799 (Sat 2026-11-21, calm) | 7200 slots (4 skipped), 48 min |
| whale_a | 471360300–471361300 (BUeHmk burst, Nov 20) | 1001 slots |
| whale_b | 472043600–472045100 (3zfHoL burst, Nov 21) | 1501 slots |
| bg_dlmm | 472299900–472300799 rescan with DLMM decoding fixed | 900 slots |
| depth | tick arrays of W (fixed + dynamic) and R at slot ≈472.30M | 1 snapshot |

USDT: no SOL/USDT pool appeared in any case. The W↔R pair is the whole SOL/USDC CLMM backrun population in the top 200.

## 1. The pattern (21 cases, top200 of 2026-11-14..21) [M]
- **What triggers it.** 19 of 21 triggers sold or bought SOL in **R**, the thin pool; 2 hit W.
  - 17 of the 20 distinct trigger txs come from the **Raydium AMM Routing program** `routeUGWgWzqBWFcrCfv8tritsqukccJPu3q5GPP3xS`, with one inner swap on R only. None went through Jupiter.
  - They are plain txs: fee 15,000 lamports, no Jito tip, landed unprotected.
  - Recurring signers: `BUeHmkTG…` (1000 / 1044 SOL), `3zfHoL1k…` (500), `7iDrVmeh…` (430–438), `6JX4XsfD…` (960), `9TuHykbZ…` (1286).
  - The other triggers are `4XBqViD1…` (program `GnZr2LD3…`, which also calls Solend and Pyth: a liquidation / bot flow, 2 cases) and `FHpcNSe6…` (a flow-neutral bot through Jupiter + `Dsug6JqU…`, 1 case).
- **Trigger sizes** were 165–1286 SOL ($11k–$92k; median 500 SOL, $34k), often round numbers (200 / 500 / 960 / 1000).
- **Gap.** Before the trigger the W/R gap was 0–3 bps (2 outliers at 5.4 and 12.6). After the trigger it was 32–138 bps (median 47.7).
  - The pool that moved was 20–150 bps away from the median of the other SOL/USDC pools. The pool that did not move stayed within ±2 bps of that median, except in 2 cases where the market itself moved about 13 bps.
  - So the dislocation is local to one pool, not a CEX move.
- **Arb.** Sell SOL in the pool whose price is high, buy it back in the low one; profit is in SOL.
  - Input was 104–996 SOL. Gross profit was 0.097–4.97 SOL (sum 20.0, median 0.51).
  - After the arb the gap was 7.6–9.4 bps in 18 of 21 cases: the arb closed it to the fee band.
  - Capture against the constant-liquidity two-pool optimum: median 1.00, range 0.86–1.10, with two exceptions. `HmvB1R…` uses a fixed 200 SOL clip (capture 0.29, leaving a 42 bps gap that `kass1i…` took 134 positions later with a Kamino flash loan). `A9pQ1Q…` left 18.8 bps.
- **Timing and position.**
  - 18 of 21 arbs landed in the trigger's slot: 12 at trigger index + 1, the rest 10–135 positions later.
  - 3 of 21 were at the top of the next slot. Their triggers landed late in the previous block (index 380 of 396 and 505 of 641) or the gap survived about 200 txs (slot 469490201: a 521 SOL sell on R at index 40 opened 137 bps; `gtagyE…` took it at index 9 of the next slot).
  - The 3 top-of-block cases had no CEX-driven cause beyond the trigger itself. Pool prices only move with swaps, so "top of block" here means a backrun that was not done in the trigger's own block.
- **Winners** (8 signers, each with its own program):

  | Signer | Program | Wins |
  |---|---|---|
  | `MriyaNN8TMp6qRWjfr723PK7xgQK7yCt7Kg2v2PQu7X` | `AN225ykG…` | 8 |
  | `DtvmxrTACskMG2W8a6KXgSemUfvyNVeQTgfpJoGvMVKx` | `8KDGPdXX…` | 5 |
  | `A9pQ1Q…` | `Cd4A3vR5…` | 2 |
  | `HmvB1R…` | `7GqLmFAG…` | 2 |
  | `gtagyE…` | `rexhfZLR…` | 1 |
  | `kass1i…` | Kamino flash + `BacXPELo…` | 1 |
  | `BkbtvE…` | `King7ki4…` | 1 |
  | `4ywa3N…` | `DDsnwb7d…` | 1 |

  CU used was 133k–494k. The arbs use no ALTs (MriyaN).
- **Payment to the leader** (priority fee above 5,000 lamports, plus Jito tip) as a share of gross:
  - 0 in 7 cases, about 0.34 in 6 (MriyaN appears to bid a fixed ~34%), 0.5–0.8 in 4, and 0.93 in 3 (Dtvmxr).
  - Both priority-fee wins (most) and Jito-tip wins (5) occur.
  - The explorer's `profit` field is gross minus tip but not minus priority fee, so it is inconsistent across bots.
- **Failed txs on W/R after the trigger in the arb's slot:** 0–208 (median 81).

## 2. Whale bursts: the arrival process [M]
- **whale_a.** `BUeHmkTG…` sold 6 chunks of 1000–1044 SOL into R about every 60–200 slots. Each chunk moved R by about 60 bps; the predicted optimum per chunk is 1.85–2.0 SOL.
- **whale_b.** `3zfHoL1k…` sold 14 chunks of 500 SOL into R about every 30–440 slots (7000 SOL ≈ $475k in 1460 slots). Each chunk opened a 47–53 bps gap; the predicted optimum is 0.66–0.83 SOL.
- **First responder (20 triggers):**
  - Landed in the same slot 19 of 20 times, at position + 1 in 13 of 20.
  - Distinct first responders: `HiMomvku…` 5, `MriyaNN8…` 3, `73oMyqXW…` 2, `DtvmxrTA…` 2, plus `GxDC9e7S`, `HvFdDWS3`, `HLH7fS5J`, `BkbtvEGS`, `DaDan6B5`, `shakRDxy` and `A9pQ1Qsv` once each.
  - **Single-leg backrunners compete with the W↔R cycle.** `HiMomvku…` (Jupiter + Kamino) and `GxDC9e7S…` trade R alone against another venue and grossed 0.88–1.02 SOL, more than the W↔R optimum of about 0.70, because only R has to move.
  - Winners bid 76–99% of gross as priority fee (`HiMomvku` 0.82–0.94 SOL on about 0.94; `Dtvmxr` 1.72 on 1.84; `73oMyqXW` 1.24 on 1.62). Sometimes the win costs about 0 (MriyaN at + 1 with 20k lamports).
- **Losers:** 0–145 failed txs on W/R after the trigger in the same slot, from up to 44 distinct signers (`competition_whales.json`).
- [I] Landing at + 1 with no tip suggests bots read shreds or the block stream and send within the same block. The write lock on R then orders them after the trigger, so ordering among competitors is by priority fee and arrival.
- [I] Bursts are rare but dense. The 48-minute calm sample had zero opportunities, while each burst produced one every 30–130 slots. The week's top200 contains 13 distinct trigger signers, so on the order of 10+ bursts per week. The total number of opportunities per week is not measured, since the explorer lists only the top 200 arbs by profit.

## 3. Background (bg, 48 min calm, plus whale windows) [M]
- **Flow per pool in 48 min:**

  | Pool | Swaps | Volume | Swaps/min | Largest directional swap |
  |---|---|---|---|---|
  | W | 718 | $549k | 15 | 280 SOL |
  | R | 574 | $140k | 12 | 71 SOL |

  - Next by volume: Raydium CLMM `DJNtGu…` (1 bp, $42k), then 1–2 bp pools under $10k.
  - Swaps routed by Jupiter: 0. Through the Raydium router: 8 on R.
  - DLMM SOL/USDC in 900 slots: `5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6` (bin step 4) 374 SOL, `BGm1tav58o…` (bin step 10, the one in the calibration) 172 SOL, `HTvjzsfX…` (bin step 1) 74 SOL.
- **Directional (non-neutral) sizes in SOL:**

  | Pool | p50 | p90 | p99 |
  |---|---|---|---|
  | W | 3.4 | 41 | 101 |
  | R | 1.1 | 7.5 | 32 |

  About half the swaps are flow-neutral: the signer ends flat, which means bots arbitraging against prop AMMs or CEX. The top "directional/direct" signer `GRV6aYoz…` (135 swaps on R, 67 on W) looks like an inventory-holding CEX-DEX bot [I].
- **Depth.**
  - Price impact per 100 SOL: W 1.30 bps, R 5.7–6.2 bps.
  - From tick arrays now, about 5.3k USDC (76 SOL) per bp on W and 1.1k USDC (16 SOL) per bp on R, flat within ±50 bps (`depth_profile.json`). W has 1611 initialised ticks within ±4000 ticks and R 4234.
  - Liquidity across the week's cases ranged L_W 0.64–4.5e14 and L_R 1.9–9.9e13, about 5×. Opportunity size depends strongly on L_R.
- **Gap |W/R|** measured after every W/R swap:

  | Window | p50 | p90 | p99 | max | Share above 8 bps |
  |---|---|---|---|---|---|
  | calm | 0.24 | 1.55 | 4.8 | 5.65 | 0 |
  | whale_b | 0.39 | 1.8 | 8.7 | 53 | 1.4% |

  Gaps above 8 bps closed in the same slot in 19 of 20 cases.
- **Spam.** Failed txs touching W/R average 1.75 per slot in the calm window (top: `BkbtvEGS…` 4148 in 48 min). No-op successful txs touching the pools (check-and-exit bots) average 0.64 per slot.

Trigger size into R → gap and optimum, constant L (`size_table.json`):

| L_R | 150 SOL | 300 | 500 | 1000 |
|---|---|---|---|---|
| 8.9e13 (now) | 8.9 bps / 0.0006 SOL | 17.8 / 0.067 | 29.7 / 0.32 | 59.5 / 1.82 |
| 7.0e13 | 11.3 / 0.006 | 22.7 / 0.12 | 37.8 / 0.49 | 75.7 / 2.54 |
| 1.9e13 (Nov 14) | 41.8 / 0.18 | 83.7 / 0.91 | 139.7 / 2.73 | 280 / 11.5 |

## 4. Detector (`scripts/detector.py`)
**Rule.** After any swap on W or R:
1. Compute gap = |p_W/p_R − 1|. Skip unless (1−f_W)(1−f_R)·p_hi/p_lo > 1.
2. Treat each pool as constant-product with virtual reserves X = L/√P and Y = L·√P (L is the active liquidity).
3. Find the optimal SOL-in for sell-high / buy-low in closed form: x* = (√(ab) − b)/c, with a = g_h g_l Y_h X_l, b = X_h Y_l, c = g_h (Y_l + g_l Y_h).
4. Expected net = gross(x*)·(1 − leader_share) − 5,000 lamports. Fire if net > 0.01 SOL.

**Inputs available right after the trigger:**
- R: SwapEvent gives post `sqrt_price_x64` and `liquidity` exactly.
- W: Traded gives pre/post sqrt price; L comes from the last Traded event (out/Δ√P) or from the pool account. Live use should read `liquidity` from the account, plus tick arrays for big moves (`depth.py` walks them).

**Results** [M]:
- **Cases:** 21 of 21 fire with the right direction, using only state before the arb tx. Predicted size and gross versus actual: median actual/predicted gross 0.998. Misses come only from bots that under-trade (HmvB1R's fixed 200 SOL).
- **Whale windows:** 20 fire episodes, every one followed by a same-slot or next-slot backrun of the same pools. No false positives. Fired gaps were 22.7–62 bps; the smallest predicted gross was 0.09 SOL.
- **Calm bg:** 0 fires in 1272 evaluations, consistent with no arbs on W/R.
- **Limits:** the constant-L approximation is accurate to about ±7% for gaps up to about 140 bps on these pools. It ignores single-leg alternatives, which are more profitable for whoever has a deep reference venue.

## 5. Offline reproduction (Surfpool) — what is needed
**Accounts per pool** (from the swap ixs in the top arb, verified on chain):
- **W:**
  - pool `Czfq3x…` (653 B) and whirlpools_config `2LecshUwdy9xi7meFgHtFJQNSKk4KdTrcpvaB56dP2NQ`.
  - Vaults: `EUuUbDcafPrmVTD5M6qoJAoyyNbihBhugADAxRMn5he9` (wSOL) and `2WLWEuKDgkDUccTpbwYp1GToYktiSB1cXvreHUwiSUVP` (USDC).
  - Tick arrays: fixed (9988 B), and 143 dynamic arrays also exist, so both layouts are needed. Each array spans 352 ticks, about 3.5%.
  - Oracle PDA `FoKYKtRpD25TKzBMndysKpgPqbj8AdLXjfpYHXn9PGTX`: passed to swap but does not exist (no adaptive fee).
- **R:**
  - pool `3ucNos…`, amm_config `3h2e43…`, observation `3Y695CuQ8AP4anbwAqiEBeQF9KxqHFr8piEwvw3UePnQ`.
  - Vaults: `4ct7br2vTPzfdmY3S5HLtTxcGSBfn6pnw98hsS6v359A` (wSOL) and `5it83u57VRrVgc51oNV19TTmAJuffPx5GtGwQr7gQNUo` (USDC).
  - Tick-array bitmap extension `4NFvUKqknMpoe6CWTzK758B8ojVLzURL5pC6MtiaJ8TQ` (1832 B).
  - Tick arrays (10240 B, 60 ticks ≈ 0.6% each; a 1000-SOL trade crosses 2–3). Clone ±10 arrays.
- **Shared:** mints wSOL and USDC, SPL Token, the Whirlpool and CLMM programs, plus the Raydium router `routeUGW…` if the whale actor should use the real path.
- **ALTs** are not required: MriyaN uses none. A bot needs 30+ keys, so v0 with its own ALT, or legacy within size limits.

**What the repo lacks** (from the repo read; see the agent summary in the session):
- No AMM venue (`venue.rs` has phoenix / openbook / kamino only) and no pool in any snapshot recipe.
- The tx layer only runs in perp sessions. Pending sends are invisible, so a same-block backrun needs the whale's tx staged into the slot builder, with the bot reacting to landed or streamed state.
- Surfpool override templates exist (`whirlpool-custom`, `raydium-clmm-custom`, `meteora-dlmm-sol-usdc`), but they set price fields without vaults or tick arrays. Use them only to set the start state. The whale move should be a **real swap by a whale actor** (Raydium router or direct CLMM swap) so vaults, ticks and fee growth stay consistent.
- **Actors still to build:**
  - **(a) whale_swap:** parent order of 2k–7k SOL in chunks of 200–1000 SOL into R every 30–200 slots, plain tx with no tip.
  - **(b) cycle searcher:** this detector, sized at optimum or fixed clip, with latency 0 (same block, +1 position) to N positions, and bid = share × gross (observed mix: 0, ~0.34, 0.5–0.93).
  - **(c) single-leg searcher:** trades R against an off-chain/reference price, bids 0.8–0.99 of gross; this is the strongest competitor.
  - **(d) background flow** on W and R with the size quantiles above, about half flow-neutral, about 1.75 failed spam txs per slot.
- **Calibration change:** `calibration/tx_layer` `amm_pool` should list R and W separately and add DLMM `5rCf1DM8…` (bin step 4), which out-traded `BGm1…` in the sample.

## Caveats
- Case selection is by explorer profit (top 200 of the week), which biases towards large gaps.
- The calm bg window is a Saturday. The weekly opportunity count is not measured.
- Whirlpool L is inferred from Traded amounts, so it averages over the traversed range.
- The first bg scan did not decode DLMM (fixed later; only bg_dlmm has DLMM). Txs touching only DLMM or prop AMMs were not recorded.
- Prop-AMM legs (HumidiFi, BisonFi, etc.) are not decoded. "Flow-neutral" is a balance-based heuristic.
- Pyth history was not checked: Hermes and benchmarks historical endpoints return "unauthorized", and decoding on-chain feed updates was skipped. Consensus across 2–16 other on-chain SOL/USDC pools stands in for the market price.
- Leader share treats every lamport above 5,000 as priority fee (single signature assumed).

## Files
- `scripts/`:
  - `rpc.py`: RPC from the Solana CLI config; the key is never printed.
  - `lib.py`: event and account decoders.
  - `pools.py`: pool metadata cache.
  - `blockscan.py`: full-block scanner writing compact JSONL.
  - `replay.py`: per-pool state timeline.
  - `analyze_cases.py` → `data/cases.{json,csv}`.
  - `analyze_bg.py` → `data/bg_summary.json`, `data/bg_swaps.csv`.
  - `flowclass.py`, `competition.py`, `depth.py`, `size_table.py`, `arbmath.py`.
  - `detector.py` (modes `cases` | `bg --file`).
  - `show.py`: timeline printer.
- `data/`:
  - Raw compact blocks: `cases.jsonl`, `bg.jsonl`, `whale_a.jsonl`, `whale_b.jsonl`, `bg_dlmm.jsonl`.
  - Results: `detector_*.json`, `competition_whales.json`, `flowclass_*.json`, `depth_profile.json`, `size_table.json`, `pools.json`.

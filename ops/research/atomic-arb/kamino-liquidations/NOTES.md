# Kamino Lend liquidations on mainnet, 2026-11-12 22:00 to 2026-11-21 22:10 UTC

Research for the `liquidation-flash` task. **M** = measured from chain data. **I** = inferred or estimated.

## Data and method

- **Source.** Every transaction that touches klend `KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD`, from Helius `getTransactionsForAddress` (full mode), slots 468,683,679 to 472,301,409. That is 1,018,575 txs over ~9 days. A few txs may be missing at the seams where a range was split.
  - Liquidation txs are kept whole: 79,319 liquidate-ix records, successes and failures, matched by the klend discriminator at the top level or in a CPI.
  - Every other tx is kept as a compact record.
- **Kamino API.** api.kamino.finance has no public liquidation endpoint. Bulk `POST /v3/kamino-market/{m}/transactions` needs private auth. I used it only for price history and reserve metrics (`data/api/`, OpenAPI spec included).
- **Per-ix data comes from the program logs.** klend logs a lot, and the parser reads it all:
  - eligibility line: bf-adjusted debt, unhealthy value, LTV%/threshold%;
  - reason and bonus bps;
  - `pnl: Liquidator repaid X and withdrew Y collateral with fees Z`;
  - `RefreshObligation` lines for each deposit and borrow (amount, USD value, bf value);
  - Scope `RefreshPriceList` lines (`tk N old to new | prev_slot new_slot crt_slot`).
- **Reserve and market parameters** were read today (`data/reserves.json`, `data/markets.json`), not as of each liquidation. Offsets come from `klend.rs`, plus fee and e-mode fields from klend source.
- **Pre-state.** For each episode start (6,201 of them), I fetched every successful Scope-program tx in [slot−1500, slot], about 5.4 min (148 chunks of 5,000 slots, ~200k Scope txs). Starting from the composition at liquidation, I walked the health backwards through each update of the obligation's price-chain entries until it was above 1. Interest accrual is ignored.
- **Slot time is ~0.215 s.** Measured: 216,000 slots took 46,428 s.

## 1. What gets liquidated (M)

Successful liquidations by reason:

| Reason | Count | Where |
| --- | --- | --- |
| `LtvExceeded` (price-triggered) | 6,307 | 1,348 obligations in 17 markets |
| `ObligationBorrowDebtTermReached` (fixed-term debt expired) | 8,402 | 3 obligations, market `Btu8835QDYgdTnMJJBSidbfQhrZzryZbMhCpty6h6Xdk`, wYLDS←AUTO, from 11-19 15:32 |

The fixed-term liquidations are not price-driven: tiny (median $0.38), bonus 26–33 bps, mostly two signers (`9HTrRoZ2…`, `3a9S7aJL…`). They dominate the tx count, so a detector must filter by reason.

**LtvExceeded by market:**
- Main `7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF`: 5,290
- `DxXdAyU3…` (JLP market): 532
- xStocks `5wJeMr…`: 150
- other markets: the rest

**Top pairs:** USDC←SOL 2,317; USDC←cbBTC 789; USDC←JLP 429; USDC←ETH 315; USDT←SOL 252; USDG←SOL 238.

**By day:** 11-13: 815; 11-14: 2,521; 11-15: 472; 11-16 to 11-19: 9–53 a day; 11-20: 928; 11-21: 1,466.

Liquidations follow the price drawdowns. SOL hourly price (Kamino API):
- 11-14: 77.6 → 69.0 (−11%)
- 11-15 01h: low of 68
- 11-20 and 11-21: 72.9 → 65.3

On calm days there are fewer than 20 a day.

### Sizes, close factor and bonus (n = 6,307)

| Measure | p10 | p25 | p50 | p75 | p90 | max | total |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Repaid per liquidation | $0.48 | $1.9 | $17 | $137 | $936 | $160k | $5.93M |
| Obligation debt before | $4 | | $165 | $1,358 | $10.3k | $3.8M | |

- **Close factor (M).**
  - Main market is **10%**; most other markets are 20%. The `specs/venues/kamino.md` says 20%, which is wrong for the main market. Liquidators repay exactly 10% of total debt in 88% of cases.
  - Insolvency LTV is 95%.
  - `min_full_liquidation_value` is $2: debts under $2 are closed in full, so 1.0 shares exist.
  - Max per liquidation is $2.5M on main, $0.5–5M elsewhere.
- **Bonus (M).** The program grants the reserve minimum almost always: 100 bps in 5,485 cases, 500 bps in 776 (ETH, some alts), 200 bps in 44.
  - The protocol takes 50% of the bonus on most reserves (`protocol_liquidation_fee_pct` = 50; 0 on PYUSD, USDS, USDT).
  - So the liquidator realises **50 bps** median (p90 250 bps).
  - Protocol fees summed to $32.3k; gross bonus left to liquidators was $37.6k.
- **Overshoot at liquidation (M).** bf_debt/unhealthy − 1 has p50 1.6e-4 and p90 1.4e-3. Obligations are liquidated essentially at the threshold, never deep under it.
- **Repeats (M).**
  - One liquidation restores health above 1: repaying 10% of debt against 10.1% of collateral lowers LTV while LTV < 99%.
  - So the same obligation is liquidated again at a later price tick, not in a burst. 99% of 900-slot episodes have length 1.
  - Per obligation over the window: 328 were liquidated once, 227 ten times or more. The median gap between repeats is 71k slots (~4 h); p10 is 7.5k slots (~27 min).
- **USDC←SOL on the main market** (n = 2,317, 407 obligations):
  - repaid: p50 $7.4, p90 $326, max $47.8k
  - debt: p50 $76, p90 $3.3k, max $3.8M
  - SOL at these liquidations: $68–81

## 2. Pre-state: what made it liquidatable, and how early (M unless noted)

Results of the walk-back over 6,201 episode starts:
- **6,007** had a single Scope update that flipped health from above 1 to at or below 1. Within the window this is always a price move, not interest accrual or a parameter change.
  - It hit the collateral entry in 5,288 cases and the debt entry in 251; 468 were other chain entries.
- **169** were already liquidatable before the 5.4-min window. These are small (median repay $3.2) and in illiquid alts: WIF 42, FARTCOIN 41+11, BONK 19, JTO 12. They were not worth anyone's tx.
- **25** had no composition logs.

**The triggering move is tiny.** Excluding 723 artifacts (12%) where |move| > 20%, caused by chained or odd Scope entries:

| \|move\| | p10 | p25 | p50 | p75 | p90 | p99 |
| --- | --- | --- | --- | --- | --- | --- |
| bps (n = 5,284) | 1.0 | 1.9 | 3.6 | 7.6 | 20.9 | 65 |

- 99% are downward moves of collateral.
- Health margin just before the trigger: p50 1.8e-4, p90 1.2e-3.
- Obligations drift onto the threshold through many small ticks; no single large gap does it.

**Liquidators push the price themselves.** Scope is pull-updated: anyone may send `RefreshPriceList`.
- The trigger update was signed by the eventual winner in 39% of episodes (2,323), and in the liquidation tx itself in 31% (1,846).
- Another liquidation bot signed it in 60% (3,617).
- A non-liquidator crank signed it in 1% (67).
- 52% of all liquidation txs carry a Scope refresh.
- When pushed, the source price's age (`crt_slot − new_slot`) has p50 0 and p90 3 slots. The upstream feeds (Pyth Lazer EMA, `MostRecentOf` chains) are fresh. The previous Scope value's age has p50 5 slots and p90 185.

**Lag from trigger to the successful liquidation** (n = 6,007): 0 slots 50%; 1–2 slots 21%; 3–10 slots 14%; 11–100 slots 14%; over 100 slots 2%. p50 1, p90 21.

| Repay size | n | lag = 0 | lag p50 | lag p90 |
| --- | --- | --- | --- | --- |
| < $10 | 2,615 | 22% | 4 | 34 |
| $10–1k | 2,833 | 66% | 0 | 3 |
| > $1k | 559 | 93% | 0 | 0 |

**How early a watcher could know (I):**
- A bot reading only on-chain Scope learns of a crossing at the push, which is too late on anything over $10.
- To compete, a bot computes health off-chain from the upstream feeds (Pyth Lazer, or Scope's own source accounts), then sends the Scope refresh and the liquidation in one tx. That is what 31% of winners do.
- The opportunity is visible off-chain at most a few slots before the push (source age p90 is 3 slots).

## 3. Execution and competition (M)

- **Winners.** 72 distinct winning signers. By count: `Bq1pvmrs…` 1,140, `LionX7R6…` 659, `EpgGrYBL…` 636, `3rSutpkQ…` 500. By USD repaid: `3rSutpkQ` 32%, `4XBqViD1` 19%, `911HDraM` 10%, `BfDxFnpo` 8%.
  - `BkbtvEGS…`, the round-1 Jupiter Lend liquidator, also won 253 here.
  - `4XBqViD1…`, the round-1 Solend liquidator, won 316.
- **Funding (all LtvExceeded).**

  | Source | Count | Note |
  | --- | --- | --- |
  | Own inventory | 2,782 | |
  | Kamino flash borrow, same tx | 2,147 | the flash reserve is the repay reserve in 99% of these |
  | Jupiter Lend flash (`jupgfSgfu…`) | 1,378 | e.g. `Bq1pvmrs`, the top winner |

  Kamino flash fee is 0.001% on USDC, SOL and USDT, and 0 on many reserves.
- **Route.** 31% go through a bot program by CPI (`GnZr2L…`, `992xQL…`, `King7k…`, `SiVKAT…`, `9i7Qdb…`); 69% call klend at the top level.
- **Collateral swap venues** (txs mentioning each): Orca Whirlpool 2,711; Jupiter v6 1,684; Raydium CLMM 982; Meteora DLMM 622; Manifest 600; ZeroFi 549; PancakeSwap v3 436; Raydium AMM v4 240. 12% have no swap and keep the collateral.
  - USDC←SOL: Whirlpool 578, Raydium CLMM 497, Jupiter 397, none 292, Pancake v3 281.
- **Compute, block position, tips.**
  - CU p50 333k (p10 233k, p90 450k).
  - Block index p50 347 (p10 63, p90 771). These liquidations land mid-block, not at the top.
  - Jito tip in 46% of txs: p50 151k lamports, p90 6.3M.
  - Priority fee p50 5.2k lamports, p90 1.2M.
  - Total cost (fee + tip) p50 $0.03 and p90 $2.0; summed, $9.3k, which is **25% of the liquidators' gross bonus**. Per size bucket the median cost/bonus ratio is 0.28–0.44.
- **Profit (I: signer token and SOL deltas at log prices; misses bots holding funds in PDAs).**
  - Per liquidation: p50 $0.05, p90 $2.1, max $1,236; $16k in total.
  - By repay size, median profit: $0–10 → $0.004; $10–100 → $0.09; $100–1k → $0.59; $1k–10k → $3.9; over $10k → $36.
- **Competition** (attempts on the same obligation within ±20 slots, other signers):

  | Other signers | 0 | 1 | 2 | 3 | 4 | 5 | 6+ |
  | --- | --- | --- | --- | --- | --- | --- | --- |
  | Episodes | 2,141 | 1,358 | 877 | 630 | 502 | 368 | 431 |

  Mean other signers by size: under $10, 0.56; $10–1k, 2.6; over $1k, 4.4.
  - 69% of wins are followed by failed attempts. Failures within ±20 slots of a win total 24,577; 23,801 of them are `ObligationHealthy`, i.e. losers landing after health was restored.
  - Over all 9 days: 30,928 `ObligationHealthy`, 873 `InsufficientLiquidity`, 727 `ReserveStale`, 395 `InvalidAccountInput`.
  - 146 more failed after the liquidate ix had run, with Custom 6004/6001 later in the tx, typically a swap slippage guard.
- **Round-1 comparison** (`out/round1_liq_flash.json`):
  - 8 top-200 SOL-profit liquidations: 7 on Jupiter Lend by `BkbtvEGS` via program `King7k…`, with no tip and CU ~250k; 1 on Solend by `4XBqViD1`, which bundles Pyth `UpdatePriceFeed`, the same self-push pattern.
  - 18 Kamino flash-loan arbs, all borrowing from the main-market SOL reserve `d4A2prbA…` (10–65+ SOL).
  - No Kamino liquidation made the top 200: profits are small and mostly in USDC.

## 4. Detector (`scripts/detector.py`)

**Rule.** Refresh prices, then:
- U = Σ deposit_value × liq_threshold (the e-mode group threshold when the obligation is in a group).
- B = Σ borrow_value × borrow_factor.
- Flag when B ≥ U.
- Repay the highest-borrow-factor debt; seize the lowest-threshold collateral.
- bonus = min(max(min_bonus, LTV − unhealthyLTV), max_bonus or e-mode cap, 1 − no_bf_LTV).
- repay = min(close_factor × total_debt, that borrow, max_at_once), and the full borrow if it is under `min_full`.
- seized = repay × (1 + bonus), capped at the collateral's value.
- net = seized − protocol_fee_pct × bonus part − repay − flash_fee × repay − swap(fee + linear impact) − tx cost.
- Market parameters come from the LendingMarket account (`fetch_markets.py`).

**Test on the collected states.** For each successful `LtvExceeded` and each `ObligationHealthy` failure, the inputs were the `refresh_obligation` logs from that tx plus today's reserve and market configs.

| Case | Result |
| --- | --- |
| Successful liquidations | **6,270 of 6,282 flagged (99.8%)**; 25 more without composition logs |
| Healthy attempts | 11 of 30,886 wrongly flagged (all at health 1.0000 after the logs' 4-decimal rounding; a tolerance artefact) |
| Repay estimate / observed | p10 = p50 = p90 = 1.00 (n = 6,181) |
| Bonus | exact in 97.9%; misses on ETH (estimated 100, observed 500) and USDG/USDC←SOL (estimated 500, observed 100) |
| Health vs the program's | relative error p50 8.6e-8, p90 6.5e-6 |

- **Misses (12)** come from today's e-mode group and reserve thresholds differing from those at the time (health estimated 1.002–1.046), and from rounding (1.000002–1.0002).
- **What the misses show.** A detector must read the obligation's elevation group and the reserves as of that slot, not cached configs.
- **Before the e-mode fix** the detector flagged 62 healthy USDC←USDT attempts as liquidatable. A real bot sent those same failing txs again and again: a live example of the mistake.
- **Timing.** With the pre-state walk, the rule fires at the trigger update's slot: 0–2 slots before the winner in 71% of episodes (`out/detector_eval.json` joins `lag_slots`).

## 5. What the Surfpool snapshot needs

The existing `snapshots/kamino-2026-11-13` holds only the Backpack and xStocks markets. Liquidation-flash on the main market needs a new recipe.

**Accounts to add:**
- **Programs:** klend, Farms, Scope (already pinned), plus the swap venues' programs.
- **Main market:** `7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF` and its lookup table `FGMSBiyVE8TvZcdQnZETAAKw28tkQJ2ccZy6pyp95URb`.
- **Reserves:** SOL `d4A2prbA2whesmvHaL88BH6Ewn5N4bTSU2Ze8P6Bc4Q` and USDC `D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59`. Optionally cbBTC `37Jk2zkz23vkAYBT66HM2gaqJuNg2nYLsCreQAVt5MWK`, USDG `ESCkPWKHmgNE7Msf77n9yzqJd5kQVWWGy3o5Mgxhvavp` and USDT `H3t6qZ1JkguCNTi9uzVKqQ7dvt2cum4XiXWom6Gn5e5S` for multi-pair families.
- **For each reserve:** liquidity supply vault, fee receiver, collateral mint and collateral supply vault, and farm state when `farm_collateral`/`farm_debt` is set. All are in `data/reserves.json`.
- **Mints:** SOL, USDC.
- **Scope:** `OraclePrices` `3t4JZcueEzTbVP6kLxXrL3VpWx45jDer4eqysweBchNH` (SOL entry 3, USDC entry 13; SOL-LST chains are `[lst, 3]`). Also its `OracleMappings` and `OracleTwaps` accounts, if bots may call `RefreshPriceList`.
- **Swap pools for the collateral leg (SOL→USDC):** an Orca Whirlpool SOL/USDC (template `whirlpool-sol-usdc` exists), a Raydium CLMM SOL/USDC (`raydium-clmm-custom`) and a Meteora DLMM SOL/USDC (`meteora-dlmm-sol-usdc`). Jupiter routing is not available offline, so a bot must swap directly.

**Overrides already present** in surfpool's Kamino set: `kamino-scope-price`, `kamino-scope-twap`, `kamino-scope-price-source`, `kamino-reserve-config`/`-state`/`-fees`/`-limits`/`-interest-rate`, `kamino-obligation-positions`/`-health`, `kamino-lending-market-risk` (close factor, insolvency, `min_full`, max at once), `kamino-lending-market-elevation-groups`. mtx only emits `kamino-scope-price` (`crates/grader/src/oracles.rs`); the CLI knows `kamino-reserve-state` and `kamino-reserve-interest-rate`.

**Missing, or needed (I):**
1. **Scope's own refresh would undo the scripted price.** Bots on mainnet call `RefreshPriceList` (52% of txs), which reads upstream sources. On a snapshot it would rewrite the overridden entry from stale source accounts.
   - Fix A: block Scope refresh (an RPC guard).
   - Fix B: override the source accounts too (Pyth Lazer / `MostRecentOf` inputs). There is no Pyth Lazer template; only `pyth-price-feed-v2` exists.
   - Simplest: write `kamino-scope-price` every slot, with `last_updated_slot` set to that slot (what the grader's `ScopeOracle` does), and make refresh a no-op or refuse it.
2. **No bulk obligation seeding.** `kamino-obligation-positions` edits an existing obligation; the grader builds synthetic borrowers by real deposit+borrow txs, which is fine up to `MAX_BORROWERS` = 32. Mainnet has 400+ SOL/USDC obligations that drift onto the threshold. A template or warmup that clones N real obligations with their positions (rewritten owner) would raise realism. Otherwise keep the synthetic route.
3. **No market-parameter mirroring in the scenario.** Close factor 10% on the main market versus 20% in the venue doc; the protocol takes 50% of the bonus. Pin these through `kamino-lending-market-risk` and `kamino-reserve-config` rather than relying on snapshot values. The `specs/venues/kamino.md` close-factor line should say 10% (main) / 20% (others).
4. **Jupiter Lend flash (`jupgfSgfu…`) is not in the snapshot**, and 22% of winners use it. Optional: Kamino's own flash borrow covers the task.

## 6. Generator calibration (I, from the measured distributions above)

- **Price path.** Liquidations come in drawdown clusters. Days with 6–11% SOL ranges produced 470–2,500 liquidations; calm days produced fewer than 20. Use slow drifts of 1–20 bps per Scope update (p50 3.6 bps), with occasional 20–65 bp jumps; Scope updates every 5 slots (p50) while active.
- **Borrowers.**
  - Debt is log-normal: median ~$165 (main market), p90 ~$10k, a tail to $1–4M.
  - Health is seeded within 1e-4 to 1e-3 of the threshold; most liquidations are at an overshoot of 1e-5 to 1e-3.
  - Each obligation gets liquidated 1–10+ times as the price keeps falling, in 10% slices.
- **Competitors.**
  - Their number scales with size: about 0.5 under $10, 2–3 for $10–1k, 4–5 over $1k.
  - Latency: 50% act in the trigger slot, 70% within 2 slots, p90 21 slots. For positions over $1k, 93% are taken in the same slot.
  - The current grader `liquidators.delay_steps` and the actor's `latency_slots` (default 2) should be drawn so that large positions get 0-slot competitors.
- **Economics.**
  - The liquidator keeps ~50 bps of repay (bonus 100 bps, minus 50% protocol fee).
  - Flash fee is ~0.1 bp; swap cost on SOL/USDC pools is small relative to that.
  - Fees and tips cost ~25–45% of the bonus; tips appear in ~46% of wins.
  - Net per liquidation p50 is about $0.05, and about $36 on positions over $10k. A task worth grading needs either 5–10× the mainnet median sizes, or the 500 bps bonus reserves (ETH-like) to give the bot a signal above fee noise.
- **Self-push as a skill.** Rewarding a bot that sends Scope refresh + liquidation atomically mirrors 31% of real winners, but conflicts with point 1 of §5; decide which oracle model the task uses.

## Caveats

- Reserve and market configs and e-mode membership are as of 2026-11-21, not as of each event. Price-artefact moves (12%) were excluded from the move statistics.
- Profit counts only accounts the signer owns, so it is understated for CPI bots that keep funds in PDAs.
- USD values come from the `refresh_obligation` log values; the SOL price for fees and tips comes from Kamino's hourly API.
- Competition counts only attempts that landed on chain: failed txs, not dropped bundles. Losers who never landed are invisible.
- The pre-state walk ignores interest accrual and multi-entry chains beyond simple multiplication; a 5.4-min window bounds `liquidatable_before_window`.

## Files

- **`data/` (downloaded):**
  - `klend_d*/liq_txs.jsonl.gz`: trimmed liquidation txs.
  - `klend_d*/all_compact.jsonl.gz`: all klend txs, compact.
  - `scope_cache/`: Scope txs with parsed updates.
  - `api/`: Kamino OpenAPI spec, SOL price, main-market reserves.
  - `reserves.json`, `markets.json`: current decoded configs.
- **`out/` (derived):**
  - `records.jsonl`: one record per liquidate ix, including failures.
  - `liquidations_ltv.csv/json`: successful `LtvExceeded`.
  - `liquidations_all_reasons.csv`.
  - `competition.json`, `episodes.json`, `prestate.csv/json`.
  - `detector_eval.json/txt`, `attempts_summary.json`, `summary.json`, `round1_liq_flash.json`.
- **`scripts/`:** `rpc.py`, `b58.py`, `collect.py`, `parse_liq.py`, `fetch_reserves.py`, `fetch_markets.py`, `analyze.py`, `prestate.py`, `detector.py`, `round1_cmp.py`. Each lists its usage in its docstring; run with `python3 -I`.

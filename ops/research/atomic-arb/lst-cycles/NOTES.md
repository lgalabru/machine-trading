# triangular-lst: what the on-chain pattern looks like (mainnet, epochs 1086-1093, 2026-11-14..21)

Labels: **[M]** measured from chain data in this directory; **[I]** inferred / approximated; **[K]** prior knowledge, not re-verified here.
Epochs: 432,000 slots, `epoch = slot // 432000` (getEpochSchedule, warmup false). Boundaries studied: 1091 = 471,312,000; 1092 = 471,744,000; 1093 = 472,176,000.

## 0. Headline

1. **Epoch-boundary rate steps don't create pool arbs** [M]. The NAV step is about 1.3–1.6 bps per epoch. LST/SOL pools trade 4–13 bps *below* NAV, and they don't reprice when the stake pool updates. Their discount to the new NAV just widens by the step. No cycle on the 5 main LST/SOL pools reacted to the update within 300 slots in 3 of 3 boundaries.
2. **The money in "LST triangles" is backrunning large swaps on thin pools where an LST is the bridge or quote asset** [M]: JTO/JitoSOL, ORE/JitoSOL, JLP/JitoSOL, hSOL/SOL, soBTC/mSOL. The trigger is a same-block swap moving that pool by hundreds of bps (median 545 bps). The arb closes it at once, at small size (median 9 SOL).
3. **Pure LST cycles** (SOL→JitoSOL→jupSOL→mSOL→SOL) are a separate niche [M]: 3–11 bps edge, very large size (120–590 SOL), 0.12–0.23 SOL profit. They come from *inconsistent per-LST discounts*: each LST trades at its own discount to NAV, and the LST/LST cross pools drift away from the LST/SOL pools.
4. **Stake-pool legs (DepositSol/WithdrawSol) are rare and tiny** [M]. They are absent from all 400 round1 arbs. In a 1/5 sample of the Jito pool's transactions, 43 WithdrawSol cycles (about 215 over 2.3 epochs, inferred from the sample) had a median signer gain of 0.0000085 SOL and a max of 0.038. They cluster in epoch 1091, when the JitoSOL discount sat at the WithdrawSol floor (−12.4 bps ≈ 10 bp fee + 1 bp pool fee + slack).

## 1. Round1 multi-token arbs (data/round1_multitoken_arbs.csv, round1_triggers.json, round1_cycles.json)

Legs were extracted from inner transfers grouped by invoking program (`scripts/legs.py`). Prices come from venue events where available: Whirlpool `Traded` (pre/post sqrt), Raydium/Pancake/Byreal CLMM `SwapEvent` (post sqrt + liquidity), DLMM `Swap` event-CPI (start/end bin), Raydium v4 `ray_log` (reserves). 400/400 txs parse; 188 are closed cycles with ≥3 distinct tokens (106 in top200, 82 in last200).

- top200 multi-token: n=106, 50.9 SOL (explorer figure) [M]. LST-touching: n=24, 11.0 SOL. By role: LST as bridge to a mid-cap, 14 / 5.5 SOL. LST+stable, 4 / 4.5 SOL, of which 2 are actually Hylo xSOL mints (see §4). Pure LST/SOL, 6 / 1.05 SOL.
- Trigger [M], meaning the most recent non-cycle swap on any pool of the route, looking back 150 slots with `getSignaturesForAddress(pool, before=arb)`: top200 has a trigger for 96/106, same block and earlier index for 77/106, ≤1 slot for 88/106, median lag 0 slots. For 46 same-block triggers with a decoded price, median |impact on the pool| = 545 bps, p90 = 5,877 bps. last200 (mostly near-zero profit): same block 33/82, median lag 1 slot. There the previous tx on the pool is itself a cycle within 2 slots in 57/82 cases, so competing searchers re-arb the same residual.
- Size [M]: first-leg input, median 9.3 SOL, p90 249 SOL. Block position: median tx index 158. Jito tip >0 in only 13/106. Profit goes to priority fee and position, or to non-Jito tip accounts (not resolved). Signers: MriyaNN8 33, DtvmxrTA 13, BkbtvEGS 10, gtagyESa 9, GxDC9e7S 6, C39ohmu1 6.
- Timing vs boundary [M]: none of the 24 top200 LST arbs falls within 15,000 slots after a boundary update except at slot-in-epoch 16k–17k (unrelated triggers). Across all top200, the only arbs within ±3,000 slots of a boundary are USDC/SOL and mid-cap cycles.
- Pure LST cycles, per leg vs NAV (round1_lst_leg_vs_nav.json; NAV for epochs before 1090 is back-extrapolated at 1.42 bp/epoch [I]; mSOL NAV is approximate [I]). Examples:
  - SOL→JitoSOL (Orca) +20.7: JitoSOL 20.7 bps below NAV.
  - JitoSOL→jupSOL (DLMM 5cvcHr) +3.6.
  - jupSOL→mSOL (Orca) +5.0.
  - mSOL→SOL (Raydium CLMM) −21.4.

  The gross edge is about 8 bps on 588 SOL. The mispricing is spread over the legs. It isn't one stale leg.
- Venue mix of the LST legs [M]: Orca Whirlpool, Meteora DLMM, Raydium CLMM. JitoSOL/SOL is also traded via **Manifest** CLOB (`MNFSTqtC93rEfYHB6hF82sKdZpUDFWkViLByLd1k1Ms`) and **ALPHAQ** (`ALPHAQmeA7bjrVuccPsYPiCvsi428SNwte66Srvs4pHA`; prop-AMM-like, identity not verified [I]). The SOL/USDC legs go through BisonFi (`BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi`, prop AMM). No arb in round1 used Sanctum Infinity, Sanctum router, SPL stake pool, or Marinade instructions [M: grep over 400 raw txs].

## 2. Epoch boundary study (data/stakepools_now.json, rate_steps_feemint.json, rate_series.py output, boundary_study.json)

Stake pool state now (epoch 1093) [M]. `step` is exact, from `last_epoch_total_lamports / last_epoch_pool_token_supply`:

| LST | pool | program | NAV | step 1092→93 (bps) | epoch fee | SOL dep fee | SOL wd fee | TVL (SOL) |
|---|---|---|---|---|---|---|---|---|
| JitoSOL | Jito4APyf642JPZPx3hGc6WWJ8zPKtRbRs4P815Awbb | SPoo1 | 1.312796 | 1.404 | 4% | 0 | 0.1% | 10.14M |
| jupSOL | 8VpRhuxa7sUUepdY3kQiTmX9rS5vx4WgaXiAnXq4KCtr | SPMBzs (Sanctum multi) | 1.220786 | 1.447 | 5% | 0 | 0.1% | 5.15M |
| bSOL | stk9ApL5HeVAwPLr3TLhDXdZS8ptVu7zp6ov8HFDuMi | SPoo1 | 1.327121 | 1.413 | 5% | 0.08% | 0.1% | 0.88M |
| hSOL | 3wK2g8ZdzAH8FJ7PKr2RcvGh7V9VYson5hrVsJM5Lmws | SP12tW | 1.197339 | 1.391 | 2.5% | 0 | 0.1% | 1.02M |
| pSOL | pSPcvR8GmG9aKDUbn9nbKYjkxt9hxMS7kF1qqKJaPqJ | SPoo1 | 1.107925 | 1.625 | 0 | 0 | 0.1% | 1.64M |
| dSOL | 9mhGNSPArRMHpLDMSmxAvuoizBqtBGqYdT8WGuqgxNdn | SP12tW | 1.225987 | 1.451 | 2.5% | 0 | 0.1% | 2.80M |
| bonkSOL | ArAQfbzsdotoKB5jJcZa3ajQrrPcWr2YQoDAEAiFxJAC | SP12tW | 1.210273 | 1.540 | 5% | 0 | 0.1% | 0.07M |

mSOL: Marinade state `8szGkuLTAux9XMgZ2vtY39jVSowEcpBfFfD8hXSEqdGC`, `msol_price` u64 ×2^32 at offset 512 = 1.419507 [M]. Per-epoch history is not measured; I assume ~1.4 bps [I].

Slot of the first fee-minting `UpdateStakePoolBalance` after the boundary [M]. Step from minted manager-fee tokens, `step ≈ fee_tokens/supply·(1−f)/f`; it matches the exact JitoSOL value to 3 decimals:

| | e1091 | e1092 | e1093 |
|---|---|---|---|
| JitoSOL | +565 (1.472 bp) | +727 (1.427) | +683 (1.404) |
| jupSOL | +549 | +787 | +718 |
| hSOL | +806 | +771 | +697 |
| dSOL | +1370 | +1466 | +1895 |
| bonkSOL | +1624 | +1724 | +2154 |

- The update is preceded by `UpdateValidatorListBalance` batches from the boundary +~527 slots on (signers `stakeDoZ…`, `Bq1pvm…`, `Ggxop1Bg…`). Right after it, several consumers read the new rate: Sanctum Infinity `UpdateLstPrices` (CiDwZEqm…, 4–50 slots later); `UpdatePrice` and `UpdateExchangePrice/UpdateRate` oracles at +200 to +2,000 slots. The pool account is also *read* by Manifest/ALPHAQ/Infinity swaps [M: 64+23+44 of 1,583 sampled swap txs referencing Jito4A].
- Rate per annum: 1.4 bps × ~182 epochs ≈ 2.6% [I]. That is lower than older ~7% LST APYs. Don't calibrate from old APYs.

Pool prices around the boundaries [M], ±15,000 slots × 3 boundaries, every swap on the pool (data/price_paths.json). Values are median deviation of the post-trade mid from the on-chain NAV in force (bps):

| pool | e1091 before → after update | e1092 | e1093 |
|---|---|---|---|
| JitoSOL/SOL Orca WP Hp53XEtt4S8SvPCXarsLSdGfZBuUr5mMmZmX2DRNXQKp | −12.36 → −10.99 | −4.86 → −5.34 | −7.38 → −8.76 |
| JitoSOL/SOL Raydium CLMM 2uoKbPEidR7KAMYtY4x7xdkHXWqYib5k4CutJauSL3Mc | −12.36 → −10.96 | −4.83 → −5.35 | −7.38 → −8.76 |
| JitoSOL/SOL Meteora DLMM BoeMUkCLHchTD31HdXsbDExuZZfcUppSLpYtV3LZTH6U | −12.41 → −10.89 | −4.89 → −6.32 | −7.32 → −8.72 |
| jupSOL/SOL Orca WP DtYKbQELgMZ3ihFUrCcCs9gy4djcUuhwgR7UpxVpP2Tg | −9.16 → −10.20 | −7.54 → −8.78 | −8.56 → −9.41 |
| mSOL/SOL Raydium CLMM 8EzbUfvcRT1Q6RL462ekGkgqbxsPmwC5FMLQZhSPMjJ3 (NAV approx) | −11.2 → −9.8 | −9.0 → −10.1 | −18.1 → −19.5 |

The discount widens by about the step (1.4 bps) in 7 of 9 cases. In e1091 it narrowed: the pool was pinned at the WithdrawSol floor, and the floor itself moved up with the NAV. Swaps on these pools are sparse: median gap 50 slots on the Orca/Raydium JitoSOL pools, 4 on DLMM. The first swap after the update came +0 to +6,067 slots later and was not a repricing. **Which arbs captured the step: none identifiable** [M]. The only boundary-correlated flow is the WithdrawSol-floor arbs in e1091 (§3).

Between boundary and update, SPL DepositSol/WithdrawSol fail (pool out of date) [K]. There is a 550–2,150-slot window when the stake-pool edge doesn't exist.

## 3. Background deviation (data/background_stats.json, background_moves.json)

Over 2,753 swaps with a decodable mid [M]:
- **Never above NAV**: max JitoSOL dev −3.9 bps, jupSOL −5.5, mSOL −5.7. DepositSol is free, so pool > NAV+fee would be arbed. It never got there.
- **Floor**: JitoSOL min −12.56 bps on all three JitoSOL pools. That equals the WithdrawSol fee (10 bps) + pool fee (1 bp) + ~1.5 bp. 47% of Orca/Raydium JitoSOL swaps (mostly epoch 1091) were below −11 bps. jupSOL min −11.0.
- **Pools almost don't move**: |consecutive mid move| p50 = 0, p99 = 0.2–1.6 bps, max 4–8 bps. Only 10 moves ≥2 bps in total, 80% of them made by cycles. Depth now [M, pool_depth_now.txt]: about 1.9k (Orca) and 0.9k (Raydium) JitoSOL-units per 1 bp in range. jupSOL Orca about 95 per bp. mSOL Orca WP has zero active liquidity. Large swaps outside boundaries therefore don't create LST/SOL deviations beyond fees. The deviation regime changes between epochs (−12 → −5 → −7 bps) through a handful of flow-driven moves.
- Stake-pool-leg cycles [M, 1/5 sample of 26,387 Jito txs, 1/8 of 11,155 jupSOL]: JitoSOL had 43 WithdrawSol + 3 DepositSol cycles (30 in e1091). jupSOL had 37, of which many are Sanctum-router deposits by users rather than arbs.

## 4. Identifications

- `24Uqj9JCLxUeoC3hGfh5W3s9FM9uCHDS2SG3LYwBpyTi` = **Meteora Dynamic Vault** [M+K]. It is invoked only at stack depth 3 under Meteora DAMM v1 (`Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB`) `Swap`. Every DAMM v1 swap does vault `Deposit` (in-token) + `Withdraw` (out-token). It is not a separate venue (29 txs in round1). legs.py treats it as transparent.
- **Hylo arbs** (3 txs: `5qhggWzMv1…`, `5Yw6scDc6c…` slot 469,601,473, `2v4jFG4nta…` slot 470,351,133; signer MriyaNN8, bot program AN225ykGPAmckE9uMCCM7jQv3L3AYwiPZbHqgMUYEgCR) [M]. The route is SOL→JitoSOL (Whirlpool/CLMM/Manifest) → `HYEXCH…::MintLevercoinLst` (deposit JitoSOL, mint xSOL `4sWNB8zGWHkh6UnmwiEtzNxL4XrN7uK9tosbESbJFfVs` at protocol NAV) → sell xSOL on Meteora DLMM/Raydium CLMM → USDC/SOL → SOL. The trigger was a same-block retail **buy of xSOL** on the DLMM pool (5,161 and 6,013 USDC) that pushed market xSOL above mint NAV. It is the same mint-at-NAV / sell-in-pool structure as a stake-pool DepositSol arb, applied to a leveraged token. Profit was 3.02 + 1.71 + 1.10 SOL (explorer). legs.py misses the mint leg (no token transfer from the user for xSOL), so the detector misses these 2+1.
- INF (Sanctum Infinity) NAV was not reconstructed. INF appears in 1 round1 cycle (SOL>hSOL>INF>SOL, 0.18 SOL, triggered by a −6,143 bp hSOL dump on Raydium CLMM).

## 5. Detector (scripts/detector.py, scripts/reconstruct.py, data/detector_results.json)

**Rule.** Nodes are mints. Edges are one direction of one venue:
- `cpv`: virtual reserves `(r_in, r_out)` plus fee. Exact for CP pools. Inside the current range for CLMM/Whirlpool/DLMM, with `r_in` calibrated from (mid, liquidity) or from an observed fill.
- `linear`: rate, fee, capacity. Used for DepositSol at `1/NAV` with dep fee, WithdrawSol at `NAV·(1−wd fee)` capped by reserve lamports, and prop AMMs/CLOB top level.

A Bellman–Ford pass on `−log(marginal·(1−fee))` gates the graph. Then all simple cycles through SOL of depth ≤4 are enumerated. A candidate needs `Π marginal > 1 + 0.5 bp`. Its input is golden-section sized on the chained out-amount, and it is reported if `out − in − tx_cost > 0`.

**Runs:**
- State A, "as the arb saw it": each leg's pre-trade mid plus depth calibrated on the arb's own fill (circular, so it tests the plumbing). 152/188 hit. Estimated/realized profit median 1.00 (n=98). Misses: 13 have realized ≈0 (below the 0.5 bp edge or tx cost). 14 are 'other': 5-leg routes beyond depth 4 and prop-AMM-only USDT/USDC loops priced at the executed rate. 7 have split/parallel routes, so the leg chain breaks. 2 are Hylo (mint leg not decoded).
- State B, "before the trigger": the trigger pool's mid is rolled back by the trigger's own impact. Tested on 48, the cycle disappears in 28. In the 20 where it remains, the rolled-back swap was one of several moves, or not the real cause.
- Synthetic stake-pool cases, illustrative depth [I]:
  - pool at NAV−5 bp with a +1.4 bp NAV step: no hit. That matches the chain.
  - pool at NAV+3 bp: DepositSol→sell hit, 2.0 bp edge.
  - pool at NAV−12.5 bp: buy→WithdrawSol hit, 1.5 bp.
  - pool at NAV−14 bp: 3.0 bp hit.

Not done: true pre-block state from getBlock / Old Faithful (Jetstreamer not used). States are reconstructed from events, which is enough for Whirlpool/CLMM/DLMM. It is weak for DAMM v1, prop AMMs, and Manifest (executed price only).

## 6. Scenario calibration (inferred from the above)

- Main family: "LST-bridged backrun". 3–4 pool graph SOL / JitoSOL / (JTO|ORE|JLP|USDC) with one thin mid-cap/LST pool. Exogenous retail swap of 100–6,000 bps impact, arriving within the block. Opportunity lifetime: same block (77/106 top arbs are same-block backruns), with competing actors re-arbing residuals within 1–2 slots. Optimal size 1–120 SOL.
- Secondary: "LST discount dispersion". JitoSOL/jupSOL/mSOL/SOL pools at independent discounts of −5 to −20 bps to NAV, with LST/LST cross pools drifting by 3–10 bps. Deep pools (≈1–2k SOL per bp), so the optimal size is 100–600 SOL for 0.1–0.25 SOL.
- Stake-pool edges as bounds, not as the trade: DepositSol caps pool ≤ NAV+fee, and WithdrawSol (10 bp, reserve-capped) floors pool ≥ NAV−~12 bp. An epoch step of 1.4 bps should be modelled, but on its own it must yield ~0 opportunity. A bot that trades the step blindly should lose fees. That is a useful distractor.
- Update latency: 550–800 slots (Jito/jup/hSOL), 1,400–2,150 (dSOL/bonkSOL). Deposits and withdrawals are unavailable in between.

## 7. Surfpool snapshot needs and missing override templates

Programs (clone the executables):
- Orca Whirlpool, Raydium CLMM, Meteora DLMM, Meteora DAMM v1 + Dynamic Vault `24Uqj9…`
- SPL stake pool `SPoo1…`, Sanctum SPL `SP12tW…` and `SPMBzs…`
- optionally Marinade `MarBms…`, Manifest, Sanctum Infinity `5ocnV1…` (+ its SOL-value calculator programs), Hylo `HYEXCH…`

Accounts:
- Per stake pool: pool account, validator list, reserve stake, pool mint, manager fee token account. The withdraw-authority PDA has no data. Sysvar clock and stake history.
- Per AMM pool: pool state, both vaults, both mints, tick arrays / bin arrays around the active price (±a few arrays), oracle (Whirlpool), amm_config (CLMM), bitmap extension (CLMM), and the DAMM v1 vault + LP accounts.

Existing templates (surfpool `feat/scenario-scripts` checkout):
- `whirlpool-custom` (liquidity, sqrt_price, tick_current_index)
- `raydium-clmm-custom`
- `raydium-amm-*`
- `meteora-dlmm-sol-usdc` / `meteora-dlmm-usdt-sol` only, with active_id. No `meteora-dlmm-custom`.
- `spl-token-account-balance`, `spl-token-mint-supply`
- `pyth-price-feed-v2`, `kamino-scope-price`, `phoenix-*`

Missing:
- (a) `spl-stake-pool-rate` writing `total_lamports`, `pool_token_supply`, `last_update_epoch`, and optionally `last_epoch_*`. This is needed for a NAV step **without** cloning hundreds of validator stake accounts. `last_update_epoch` must equal the surfnet clock's epoch, or DepositSol/WithdrawSol fail [K].
- (b) `marinade-msol-price`
- (c) `meteora-dlmm-custom` (active_id for any lb_pair)
- (d) `meteora-damm-v1` (vault balances / LP); `raydium-cpmm` can use spl-token vault balances.
- (e) `sanctum-infinity-lst-state` (sol_value)
- (f) an epoch-advance or clock override, to cross a boundary offline.

The mtx runtime has no `amm` or `stake-pool` venue yet: `crates/runtime/src/venue.rs` lists both kinds as draft profiles with no holding. `specs/instruments/stake-pool.md` is still a draft and leaves the depeg valuation question open. The data above says LSTs trade 5–20 bps under NAV permanently, so "value at NAV" overstates holdings by that much.

## Caveats

- Profits are the explorer's figures. Tips are only Jito-tip accounts. Fees/tips/CU come from round1 recs.
- NAV before epoch 1092 is chained from fee-mint steps. Before 1090 it is extrapolated at 1.42 bp/epoch. mSOL NAV per epoch is approximate.
- "Trigger" = the most recent non-cycle swap on a route pool in 150 slots. That is a correlate, not proof of causation.
- Stake-pool-leg counts are from 1/5 and 1/8 samples.
- Price-path windows cover ±15k slots × 3 boundaries, not the whole week.
- 3/153 whirlpool swaps on Hp53 escaped leg extraction in the coverage check.
- Raw caches: data/tx (330 MB of getTransaction JSON), data/sigs (75 MB). Delete if not needed.
